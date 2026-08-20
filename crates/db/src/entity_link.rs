//! Cross-domain entity link storage over the `entity_links` table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/entity_link_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `delete_by_entity_ids` batches its `IN` lists in chunks of
//!   [`ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
//!   placeholder list (potential 32766 bound-parameter violation). The two
//!   lists are bound full-list-then-full-list (the oracle's
//!   `append(args, args...)`), and the input ids are de-duplicated.
//! - `create` rejects self-links with an explicit pre-check returning
//!   `false` (house `bool` convention): the schema's
//!   `CHECK (subject_entity_id != target_entity_id)` alone is NOT
//!   sufficient, because `INSERT OR IGNORE` (the idempotency mechanism)
//!   silently swallows CHECK violations too — the oracle's pre-check is
//!   therefore kept.
//! - `delete` returns `bool` (`false` = no link in either direction)
//!   instead of the oracle's "not found" error (house convention, as in
//!   `entity.rs`/`fact.rs`).
//! - `list_by_method` carries a deterministic `ORDER BY` (the oracle had
//!   none); the other listings keep the oracle's order.
//!
//! Note: the v5 `entity_links` table has NO row id — the composite primary
//! key `(subject_entity_id, target_entity_id, relation_type)` identifies a
//! link — so [`EntityLink`] carries no `id` field.

use std::collections::HashSet;

use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Maximum ids per single `IN (...)` statement (design D9): SQLite bounds
/// bound parameters per statement at 32766; 500 stays far below it even when
/// one statement carries two `IN` lists (500 × 2 = 1000 parameters).
const ID_BATCH_SIZE: usize = 500;

/// Shared `SELECT` list for the `entity_links` row queries (column order is
/// the contract of [`row_to_link`]).
const SELECT_LINK: &str = "SELECT subject_entity_id, target_entity_id, relation_type, method, \
      confidence, evidence \
      FROM entity_links";

/// Shared `DELETE` prefix for the bulk link removal
/// ([`EntityLinkDao::delete_by_entity_ids`]).
const DELETE_LINK: &str = "DELETE FROM entity_links";

/// A cross-domain link between two entities (one row of the v5
/// `entity_links` table).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityLink {
    /// The subject entity (FK to `entities.id`, `ON DELETE CASCADE`).
    pub subject_entity_id: i64,
    /// The target entity (FK to `entities.id`, `ON DELETE CASCADE`).
    pub target_entity_id: i64,
    /// Relation kind (schema default `'same_entity'`).
    pub relation_type: String,
    /// How the link was established (`'rule'`, `'equals'`, `'llm'`).
    pub method: String,
    /// Extraction confidence.
    pub confidence: f64,
    /// Free-text evidence, if any.
    pub evidence: Option<String>,
}

/// CRUD over the `entity_links` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewEntityLinkDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DbError, EntityLink, EntityLinkDao};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
///     let link = EntityLink {
///         subject_entity_id: 1,
///         target_entity_id: 2,
///         relation_type: "same_entity".into(),
///         method: "rule".into(),
///         confidence: 0.9,
///         evidence: None,
///     };
///     assert!(links.create(&link)?);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct EntityLinkDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> EntityLinkDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert one link (idempotent on the composite primary key: an
    /// existing link is not an error). Returns `true` if a new row was
    /// inserted, `false` if the link already exists OR is a self-link
    /// (subject == target, rejected by the pre-check — `INSERT OR IGNORE`
    /// would silently swallow the schema `CHECK` violation).
    pub fn create(&self, link: &EntityLink) -> Result<bool, DbError> {
        if link.subject_entity_id == link.target_entity_id {
            return Ok(false);
        }
        let changed = self.exec.execute(
            "INSERT OR IGNORE INTO entity_links \
             (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                link.subject_entity_id,
                link.target_entity_id,
                link.relation_type,
                link.method,
                link.confidence,
                link.evidence
            ],
        )?;
        Ok(changed > 0)
    }

    /// All links in which `entity_id` appears as subject OR target, in the
    /// oracle's order (`target_entity_id, subject_entity_id`). The caller
    /// determines direction by comparing with `subject_entity_id`.
    pub fn list_by_entity(&self, entity_id: i64) -> Result<Vec<EntityLink>, DbError> {
        self.exec.query(
            &format!(
                "{SELECT_LINK} WHERE subject_entity_id = ?1 OR target_entity_id = ?2 \
                 ORDER BY target_entity_id, subject_entity_id"
            ),
            [entity_id, entity_id],
            row_to_link,
        )
    }

    /// All links created by `method` (`'rule'`, `'equals'`, `'llm'`),
    /// ordered by subject, target and relation type.
    pub fn list_by_method(&self, method: &str) -> Result<Vec<EntityLink>, DbError> {
        self.exec.query(
            &format!("{SELECT_LINK} WHERE method = ? ORDER BY subject_entity_id, target_entity_id, relation_type"),
            [method],
            row_to_link,
        )
    }

    /// All links, ordered by subject, target and relation type.
    pub fn list_all(&self) -> Result<Vec<EntityLink>, DbError> {
        self.exec.query(
            &format!("{SELECT_LINK} ORDER BY subject_entity_id, target_entity_id, relation_type"),
            [],
            row_to_link,
        )
    }

    /// Remove BOTH directions of the link between two entities (A→B and
    /// B→A). Returns `true` if at least one row was deleted, `false` if no
    /// link existed in either direction.
    pub fn delete(&self, subject_id: i64, target_id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "DELETE FROM entity_links WHERE (subject_entity_id = ?1 AND target_entity_id = ?2) \
             OR (subject_entity_id = ?2 AND target_entity_id = ?1)",
            [subject_id, target_id],
        )?;
        Ok(changed > 0)
    }

    /// Total number of entity links.
    pub fn count(&self) -> Result<i64, DbError> {
        self.exec
            .query_row("SELECT COUNT(*) FROM entity_links", [], |row| row.get(0))
    }

    /// Number of distinct entities referenced in `entity_links` (graph
    /// nodes: subjects ∪ targets).
    pub fn graph_node_count(&self) -> Result<i64, DbError> {
        self.exec.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT subject_entity_id FROM entity_links \
             UNION SELECT DISTINCT target_entity_id FROM entity_links)",
            [],
            |row| row.get(0),
        )
    }

    /// Remove all links in which ANY of `entity_ids` appears as subject or
    /// target (the incremental-relink cleanup step). Empty `entity_ids` →
    /// `0`. Returns the number of rows deleted.
    ///
    /// The input ids are de-duplicated and the two `IN` lists are batched in
    /// chunks of [`ID_BATCH_SIZE`] (design D9: 500 × 2 = 1000 parameters per
    /// statement), bound full-list-then-full-list.
    pub fn delete_by_entity_ids(&self, entity_ids: &[i64]) -> Result<i64, DbError> {
        let mut deleted = 0;
        let unique: Vec<i64> = entity_ids
            .iter()
            .copied()
            .collect::<HashSet<i64>>()
            .into_iter()
            .collect();
        for batch in unique.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!(
                "{DELETE_LINK} WHERE subject_entity_id IN ({placeholders}) \
                 OR target_entity_id IN ({placeholders})"
            );
            deleted += self
                .exec
                .execute(&sql, params_from_iter(batch.iter().chain(batch.iter())))?;
        }
        Ok(deleted as i64)
    }
}

/// Map an `entity_links` row (in [`SELECT_LINK`] column order) to an
/// [`EntityLink`].
fn row_to_link(row: &Row<'_>) -> rusqlite::Result<EntityLink> {
    Ok(EntityLink {
        subject_entity_id: row.get(0)?,
        target_entity_id: row.get(1)?,
        relation_type: row.get(2)?,
        method: row.get(3)?,
        confidence: row.get(4)?,
        evidence: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::entity::EntityDao;
    use crate::test_util::in_memory_db;

    /// Create an entity through a pooled connection and return its id (the
    /// link endpoint FKs require existing entities; `foreign_keys=ON`).
    fn insert_entity(db: &Db, name: &str) -> i64 {
        db.with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            entities.create("PERSON", name, "", None, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// A link fixture with default relation/method.
    fn link(subject: i64, target: i64) -> EntityLink {
        EntityLink {
            subject_entity_id: subject,
            target_entity_id: target,
            relation_type: "same_entity".into(),
            method: "rule".into(),
            confidence: 0.9,
            evidence: None,
        }
    }

    // Smoke: create + listing round-trip in both directions.
    #[test]
    fn create_then_list_round_trip() {
        let db = in_memory_db();
        let a = insert_entity(&db, "Alice");
        let b = insert_entity(&db, "Bob");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
            assert!(links.create(&link(a, b))?, "new link must insert");
            assert!(
                !links.create(&link(a, b))?,
                "duplicate link must be ignored"
            );
            let mut reverse = link(b, a);
            reverse.method = "equals".into();
            assert!(links.create(&reverse)?);

            let by_a = links.list_by_entity(a)?;
            assert_eq!(by_a.len(), 2, "both directions must be visible");
            assert_eq!(links.count()?, 2);
            assert_eq!(links.graph_node_count()?, 2);
            assert_eq!(links.list_by_method("rule")?.len(), 1);
            assert_eq!(links.list_by_method("equals")?.len(), 1);
            assert_eq!(links.list_all()?.len(), 2);
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Smoke: a self-link is rejected (no row inserted) — the pre-check
    // matters: `INSERT OR IGNORE` would silently swallow the schema CHECK
    // violation instead.
    #[test]
    fn create_rejects_self_link() {
        let db = in_memory_db();
        let a = insert_entity(&db, "Alice");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
            assert!(!links.create(&link(a, a))?, "self-link must be rejected");
            assert_eq!(links.count()?, 0, "no row may be inserted");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Smoke: delete removes both directions; a miss reports `false`.
    #[test]
    fn delete_removes_both_directions() {
        let db = in_memory_db();
        let a = insert_entity(&db, "Alice");
        let b = insert_entity(&db, "Bob");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
            links.create(&link(a, b))?;
            links.create(&link(b, a))?;
            assert!(
                links.delete(b, a)?,
                "the reverse-direction call must hit both rows"
            );
            assert_eq!(links.count()?, 0);
            assert!(!links.delete(a, b)?, "second delete must report false");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
