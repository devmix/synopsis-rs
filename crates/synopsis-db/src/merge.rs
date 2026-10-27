//! Transactional entity merge (change multilingual-entity-resolution, design
//! D2).
//!
//! [`merge_entities`] is the single merge primitive shared by the CLI
//! (`db merge-entities`) and the linker's cross-script merge action: it
//! re-points every dependent row from `from` onto `into` and deletes the
//! merged-away entity, in one atomic unit of work.
//!
//! **Contract (data-schema spec, "entity_aliases table and transactional
//! merge"):**
//! - Preconditions are checked BEFORE any write (both entities exist, same
//!   `type` + `domain`, `into != from`); a violation returns
//!   [`DbError::MergePrecondition`] and leaves the database byte-identical.
//! - Inside one transaction the operation re-points:
//!   - `facts` (subject AND object positions); a from-fact whose post-merge
//!     `(subject, object, predicate)` triple collides with an existing
//!     non-from fact — or with a lower-id from-fact — is DROPPED (never
//!     duplicated) to respect `UNIQUE (subject, object, predicate)`.
//!   - `chunk_entities` (`INSERT OR IGNORE` + delete: PK collisions collapse
//!     onto the pre-existing `into` row).
//!   - `entity_sources` (`INSERT OR IGNORE` + delete: `(entity_id,
//!     document_id)` collisions ignored).
//!   - `entity_links` (subject AND target positions; rows that would become
//!     self-links are dropped, duplicates ignored via `INSERT OR IGNORE`).
//!   - records `from.name` and `into.name` as aliases of `into`
//!     (`INSERT OR IGNORE`).
//!   - deletes the `from` row LAST (the `facts` FKs have no `ON DELETE
//!     CASCADE`, so every `from`-referencing fact must already be re-pointed
//!     or dropped before the delete).
//!
//! The function runs on a [`ConnectionOrTx`]; atomicity comes from the
//! caller wrapping it in [`Db::exec_tx`](crate::Db::exec_tx) (a
//! `&Connection` cannot start a transaction, so the transaction is the
//! caller's unit of work).
//!
//! # NDA
//!
//! Subject-matter entity names must not appear in this module's tests; the
//! fixture uses generic words only (`Into`, `From`, `Other`).

use rusqlite::params;

use crate::entity::EntityDao;
use crate::entity_alias::EntityAliasDao;
use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// The result of a successful [`merge_entities`]: the re-pointed row counts
/// per table, the surviving entity's name, the merged-away entity's name, and
/// the aliases recorded for the survivor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSummary {
    /// The surviving entity's id (`into`).
    pub into_id: i64,
    /// The merged-away entity's id (`from`), now deleted.
    pub from_id: i64,
    /// The surviving entity's name (unchanged by the merge).
    pub surviving_name: String,
    /// The merged-away entity's name (recorded as an alias of the survivor).
    pub merged_name: String,
    /// Number of `facts` rows re-pointed from `from` to `into`.
    pub facts_repointed: i64,
    /// Number of `facts` rows dropped because their post-merge triple would
    /// collide with `UNIQUE (subject, object, predicate)`.
    pub facts_dropped: i64,
    /// Number of `chunk_entities` links re-pointed from `from` to `into`.
    pub chunk_entities_repointed: i64,
    /// Number of `entity_sources` rows moved from `from` to `into`.
    pub entity_sources_repointed: i64,
    /// Number of `entity_links` rows re-pointed from `from` to `into`
    /// (rows that would become self-links are excluded).
    pub entity_links_repointed: i64,
    /// The aliases recorded for the survivor: `[from.name, into.name]`.
    pub aliases: Vec<String>,
}

/// Merge entity `from_id` into entity `into_id` (design D2).
///
/// Preconditions (checked before any write; a violation returns
/// [`DbError::MergePrecondition`] and leaves the database byte-identical):
/// `into_id != from_id`; both entities exist; they have the same `type` and
/// the same `domain`.
///
/// On success, every dependent row is re-pointed from `from` to `into`
/// (respecting the schema's UNIQUE constraints — colliding rows are dropped,
/// never duplicated), `from.name` and `into.name` are recorded as aliases of
/// `into`, and the `from` row is deleted. The caller is responsible for the
/// surrounding transaction: wrap this call in
/// [`Db::exec_tx`](crate::Db::exec_tx) so a mid-way failure rolls back
/// atomically.
///
/// # Errors
///
/// - [`DbError::MergePrecondition`] when a precondition is violated.
/// - [`DbError::Sqlite`] on any underlying SQL failure (the caller's
///   transaction rolls back).
pub fn merge_entities(
    conn: ConnectionOrTx,
    into_id: i64,
    from_id: i64,
) -> Result<MergeSummary, DbError> {
    let entities = EntityDao::new(conn);

    // Preconditions: checked before any write, so a violation leaves the
    // database byte-identical (no write has run yet).
    let into = entities
        .get_by_id(into_id)?
        .ok_or_else(|| DbError::MergePrecondition {
            reason: format!("entity {into_id} does not exist"),
        })?;
    let from = entities
        .get_by_id(from_id)?
        .ok_or_else(|| DbError::MergePrecondition {
            reason: format!("entity {from_id} does not exist"),
        })?;
    if into_id == from_id {
        return Err(DbError::MergePrecondition {
            reason: "into_id and from_id must differ".into(),
        });
    }
    if into.entity_type != from.entity_type {
        return Err(DbError::MergePrecondition {
            reason: format!(
                "type mismatch: {:?} vs {:?}",
                into.entity_type, from.entity_type
            ),
        });
    }
    if into.domain != from.domain {
        return Err(DbError::MergePrecondition {
            reason: format!("domain mismatch: {:?} vs {:?}", into.domain, from.domain),
        });
    }

    // `?1` = from_id, `?2` = into_id in every statement below.
    // 1. facts: drop the from-facts whose post-merge (subject, object,
    //    predicate) triple collides, then re-point the survivors in both
    //    positions. A from-fact is dropped when (A) a non-from fact already
    //    holds its post-merge triple, or (B) it maps to the (into, into, p)
    //    triple and a lower-id from-fact maps there too (the only post-merge
    //    triple several from-facts can share).
    let facts_dropped = conn.execute(
        "DELETE FROM facts AS f \
         WHERE (f.subject_entity_id = ?1 OR f.object_entity_id = ?1) \
           AND ( \
             EXISTS ( \
               SELECT 1 FROM facts g \
               WHERE g.subject_entity_id IS (CASE WHEN f.subject_entity_id = ?1 THEN ?2 ELSE f.subject_entity_id END) \
                 AND g.object_entity_id IS (CASE WHEN f.object_entity_id = ?1 THEN ?2 ELSE f.object_entity_id END) \
                 AND g.predicate = f.predicate \
                 AND NOT (g.subject_entity_id = ?1 OR g.object_entity_id = ?1) \
             ) \
             OR ( \
               f.subject_entity_id IN (?1, ?2) AND f.object_entity_id IN (?1, ?2) \
               AND EXISTS ( \
                 SELECT 1 FROM facts h \
                 WHERE h.id < f.id \
                   AND h.subject_entity_id IN (?1, ?2) \
                   AND h.object_entity_id IN (?1, ?2) \
                   AND h.predicate = f.predicate \
               ) \
             ) \
           )",
        params![from_id, into_id],
    )? as i64;
    // Surviving from-facts: these are exactly the rows about to be re-pointed.
    let facts_repointed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM facts WHERE subject_entity_id = ?1 OR object_entity_id = ?1",
        params![from_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "UPDATE facts SET subject_entity_id = ?2 WHERE subject_entity_id = ?1",
        params![from_id, into_id],
    )?;
    conn.execute(
        "UPDATE facts SET object_entity_id = ?2 WHERE object_entity_id = ?1",
        params![from_id, into_id],
    )?;

    // 2. chunk_entities: collapse onto the pre-existing into row, then delete.
    let chunk_entities_repointed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunk_entities WHERE entity_id = ?1",
        params![from_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO chunk_entities (chunk_id, entity_id) \
         SELECT chunk_id, ?2 FROM chunk_entities WHERE entity_id = ?1",
        params![from_id, into_id],
    )?;
    conn.execute(
        "DELETE FROM chunk_entities WHERE entity_id = ?1",
        params![from_id],
    )?;

    // 3. entity_sources: move onto the into row, ignoring collisions.
    let entity_sources_repointed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entity_sources WHERE entity_id = ?1",
        params![from_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO entity_sources (entity_id, document_id) \
         SELECT ?2, document_id FROM entity_sources WHERE entity_id = ?1",
        params![from_id, into_id],
    )?;
    conn.execute(
        "DELETE FROM entity_sources WHERE entity_id = ?1",
        params![from_id],
    )?;

    // 4. entity_links: re-point both positions (skipping would-be self-links),
    //    then delete the from-rows. A row is "re-pointed" only when its target
    //    position is not the survivor (otherwise it would become a self-link).
    let entity_links_repointed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entity_links \
         WHERE (subject_entity_id = ?1 AND target_entity_id != ?2) \
            OR (target_entity_id = ?1 AND subject_entity_id != ?2)",
        params![from_id, into_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO entity_links \
         (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) \
         SELECT ?2, target_entity_id, relation_type, method, confidence, evidence \
         FROM entity_links WHERE subject_entity_id = ?1 AND target_entity_id != ?2",
        params![from_id, into_id],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO entity_links \
         (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) \
         SELECT subject_entity_id, ?2, relation_type, method, confidence, evidence \
         FROM entity_links WHERE target_entity_id = ?1 AND subject_entity_id != ?2",
        params![from_id, into_id],
    )?;
    conn.execute(
        "DELETE FROM entity_links WHERE subject_entity_id = ?1 OR target_entity_id = ?1",
        params![from_id],
    )?;

    // 5. aliases: record both names for the survivor (idempotent).
    let aliases = EntityAliasDao::new(conn);
    aliases.insert_or_ignore(into_id, &from.name)?;
    aliases.insert_or_ignore(into_id, &into.name)?;

    // 6. delete the from row LAST (the facts FKs have no ON DELETE CASCADE).
    entities.delete(from_id)?;

    Ok(MergeSummary {
        into_id,
        from_id,
        surviving_name: into.name.clone(),
        merged_name: from.name.clone(),
        facts_repointed,
        facts_dropped,
        chunk_entities_repointed,
        entity_sources_repointed,
        entity_links_repointed,
        aliases: vec![from.name, into.name],
    })
}
