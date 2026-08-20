//! Chunk↔entity link storage over the `chunk_entities` junction table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/chunk_entity_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `get_entities_by_chunks` batches the `IN` list in chunks of
//!   [`ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
//!   placeholder list (potential 32766 bound-parameter violation). It also
//!   de-duplicates the input chunk ids and reuses
//!   [`crate::entity::EntityDao::get_by_ids`] for the entity rows (DRY).
//! - `get_entities_by_chunks` scans the FULL `entities` row including
//!   `confidence`: the oracle's hand-rolled `SELECT` omitted the column,
//!   silently zeroing the confidence of every returned entity.
//! - `link`/`unlink`/`unlink_chunk`/`unlink_entity` return `bool`
//!   (`false` = nothing to do) instead of "not found" errors (house
//!   convention, as in `entity.rs`/`fact.rs`); `link` additionally reports
//!   whether a new row was inserted (the oracle always returned `nil`).
//! - `get_entities_by_chunk` selects `entity_id` straight from
//!   `chunk_entities` ordered by id: the oracle's `INNER JOIN entities` is
//!   redundant (the FK guarantees the entity exists) and its
//!   `ORDER BY e.name` is not deterministic when names collide.
//! - `get_chunk_texts_by_entity` returns an empty `Vec` when the entity has
//!   no chunks (or `limit == 0`); the oracle injected a
//!   `"<no context available>"` placeholder — a presentation concern that
//!   belongs to the caller, not the DAO. Its `ORDER BY` gains an `id`
//!   tie-break (the oracle ordered by `sequence_num` alone, which ties
//!   across documents).
//! - Deletion of a chunk, its document or the entity cascades to
//!   `chunk_entities` per the schema FKs (no explicit cleanup method needed,
//!   as in the oracle).

use std::collections::{HashMap, HashSet};

use rusqlite::params_from_iter;

use crate::entity::{Entity, EntityDao};
use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Maximum ids per single `IN (...)` statement (design D9): SQLite bounds
/// bound parameters per statement at 32766; 500 stays far below it.
const ID_BATCH_SIZE: usize = 500;

/// Link management over the `chunk_entities` junction table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewChunkEntityDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ChunkEntityDao, ConnectionOrTx, Db, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
///     links.link(1, 1)?;
///     assert!(links.is_linked(1, 1)?);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct ChunkEntityDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> ChunkEntityDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Link an entity to a chunk (idempotent: an existing link is not an
    /// error). Returns `true` if a new row was inserted, `false` if the pair
    /// was already linked.
    pub fn link(&self, chunk_id: i64, entity_id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "INSERT OR IGNORE INTO chunk_entities (chunk_id, entity_id) VALUES (?1, ?2)",
            [chunk_id, entity_id],
        )?;
        Ok(changed > 0)
    }

    /// Remove one link. Returns `true` if a row was deleted, `false` if the
    /// pair was not linked.
    pub fn unlink(&self, chunk_id: i64, entity_id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "DELETE FROM chunk_entities WHERE chunk_id = ?1 AND entity_id = ?2",
            [chunk_id, entity_id],
        )?;
        Ok(changed > 0)
    }

    /// All entity ids linked to one chunk, ordered by entity id.
    pub fn get_entities_by_chunk(&self, chunk_id: i64) -> Result<Vec<i64>, DbError> {
        self.exec.query(
            "SELECT entity_id FROM chunk_entities WHERE chunk_id = ? ORDER BY entity_id",
            [chunk_id],
            |row| row.get(0),
        )
    }

    /// All chunk ids linked to one entity, ordered by chunk id.
    pub fn get_chunks_by_entity(&self, entity_id: i64) -> Result<Vec<i64>, DbError> {
        self.exec.query(
            "SELECT chunk_id FROM chunk_entities WHERE entity_id = ? ORDER BY chunk_id",
            [entity_id],
            |row| row.get(0),
        )
    }

    /// Whether the chunk/entity pair is linked.
    pub fn is_linked(&self, chunk_id: i64, entity_id: i64) -> Result<bool, DbError> {
        let count: i64 = self.exec.query_row(
            "SELECT COUNT(*) FROM chunk_entities WHERE chunk_id = ?1 AND entity_id = ?2",
            [chunk_id, entity_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Remove ALL entity links of one chunk (the explicit pre-rebuild step;
    /// the schema cascade does the same when the chunk is deleted). Returns
    /// `true` if at least one row was deleted.
    pub fn unlink_chunk(&self, chunk_id: i64) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM chunk_entities WHERE chunk_id = ?", [chunk_id])?;
        Ok(changed > 0)
    }

    /// Remove ALL chunk links of one entity. Returns `true` if at least one
    /// row was deleted.
    pub fn unlink_entity(&self, entity_id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "DELETE FROM chunk_entities WHERE entity_id = ?",
            [entity_id],
        )?;
        Ok(changed > 0)
    }

    /// Up to `limit` chunk texts linked to one entity, ordered by
    /// `sequence_num` (context assembly). Empty `Vec` when the entity has no
    /// chunks or `limit == 0`.
    pub fn get_chunk_texts_by_entity(
        &self,
        entity_id: i64,
        limit: i64,
    ) -> Result<Vec<String>, DbError> {
        self.exec.query(
            "SELECT c.chunk_text FROM chunks c \
             INNER JOIN chunk_entities ce ON c.id = ce.chunk_id \
             WHERE ce.entity_id = ?1 \
             ORDER BY c.sequence_num, c.id LIMIT ?2",
            [entity_id, limit],
            |row| row.get(0),
        )
    }

    /// Full [`Entity`] records for each of `chunk_ids`, grouped by chunk id.
    /// Chunk ids without links are absent from the map; empty `chunk_ids`
    /// yields an empty map. Within one chunk the entities are sorted by
    /// name, `id` as the tie-break (the oracle's `ORDER BY e.name`, made
    /// deterministic).
    ///
    /// The `IN` list is batched in chunks of [`ID_BATCH_SIZE`] (design D9);
    /// input ids are de-duplicated, and the entity rows come from
    /// [`EntityDao::get_by_ids`] (DRY, full column set).
    pub fn get_entities_by_chunks(
        &self,
        chunk_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<Entity>>, DbError> {
        let mut by_chunk: HashMap<i64, Vec<i64>> = HashMap::new();
        if !chunk_ids.is_empty() {
            let unique: Vec<i64> = chunk_ids
                .iter()
                .copied()
                .collect::<HashSet<i64>>()
                .into_iter()
                .collect();
            for batch in unique.chunks(ID_BATCH_SIZE) {
                let placeholders = vec!["?"; batch.len()].join(", ");
                let pairs = self.exec.query(
                    &format!(
                        "SELECT chunk_id, entity_id FROM chunk_entities \
                         WHERE chunk_id IN ({placeholders})"
                    ),
                    params_from_iter(batch.iter().copied()),
                    |row| -> rusqlite::Result<(i64, i64)> { Ok((row.get(0)?, row.get(1)?)) },
                )?;
                for (chunk_id, entity_id) in pairs {
                    by_chunk.entry(chunk_id).or_default().push(entity_id);
                }
            }
        }
        let entity_ids: Vec<i64> = by_chunk
            .values()
            .flatten()
            .copied()
            .collect::<HashSet<i64>>()
            .into_iter()
            .collect();
        let entities = EntityDao::new(self.exec).get_by_ids(&entity_ids)?;
        let by_id: HashMap<i64, Entity> = entities
            .into_iter()
            .map(|entity| (entity.id, entity))
            .collect();
        let mut result: HashMap<i64, Vec<Entity>> = HashMap::new();
        for (chunk_id, mut entity_ids) in by_chunk {
            entity_ids.sort_unstable();
            let mut entities: Vec<Entity> = entity_ids
                .into_iter()
                .filter_map(|id| by_id.get(&id).cloned())
                .collect();
            entities.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
            result.insert(chunk_id, entities);
        }
        Ok(result)
    }

    /// Distinct entity ids mentioned by any chunk of one document, ordered
    /// by entity id (entity lookup without loading the chunk data).
    pub fn get_entity_ids_by_doc_id(&self, doc_id: i64) -> Result<Vec<i64>, DbError> {
        self.exec.query(
            "SELECT DISTINCT ce.entity_id FROM chunk_entities ce \
             INNER JOIN chunks ch ON ch.id = ce.chunk_id \
             WHERE ch.doc_id = ? ORDER BY ce.entity_id",
            [doc_id],
            |row| row.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::chunk::ChunkDao;
    use crate::document::DocumentDao;
    use crate::entity::EntityDao;
    use crate::test_util::in_memory_db;

    /// A (document, chunk, entity) fixture; returns their ids.
    fn seed_chunk_and_entity(db: &Db, path: &str) -> (i64, i64, i64) {
        db.with_conn(|conn| -> Result<(i64, i64, i64), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc_id = docs.create("markdown", path, None, None)?;
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let chunk_id = chunks.create(doc_id, "smoke text", 0, None, None)?;
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let entity_id = entities.create("PERSON", "Smoke", "smoke", None, None, None)?;
            Ok((doc_id, chunk_id, entity_id))
        })
        .unwrap()
        .unwrap()
    }

    // Smoke: link / is_linked / both-direction listing round-trip.
    #[test]
    fn link_is_linked_and_listing_round_trip() {
        let db = in_memory_db();
        let (doc_id, chunk_id, entity_id) = seed_chunk_and_entity(&db, "/smoke/chunk_entity.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(!links.is_linked(chunk_id, entity_id)?);

            assert!(links.link(chunk_id, entity_id)?, "new link must insert");
            assert!(
                !links.link(chunk_id, entity_id)?,
                "second link must be ignored"
            );
            assert!(links.is_linked(chunk_id, entity_id)?);

            assert_eq!(links.get_entities_by_chunk(chunk_id)?, vec![entity_id]);
            assert_eq!(links.get_chunks_by_entity(entity_id)?, vec![chunk_id]);

            // get_entities_by_chunks returns the full Entity record.
            let by_chunk = links.get_entities_by_chunks(&[chunk_id])?;
            let entities = by_chunk.get(&chunk_id).expect("chunk id must be present");
            assert_eq!(entities.len(), 1);
            assert_eq!(entities[0].id, entity_id);
            assert_eq!(entities[0].name, "Smoke");

            // get_entity_ids_by_doc_id sees the linked entity.
            assert_eq!(links.get_entity_ids_by_doc_id(doc_id)?, vec![entity_id]);

            // unlink removes exactly the pair.
            assert!(links.unlink(chunk_id, entity_id)?);
            assert!(!links.is_linked(chunk_id, entity_id)?);
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
