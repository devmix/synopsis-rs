//! Chunk↔entity link storage over the `chunk_entities` junction table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/chunk_entity_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `get_entities_by_chunks` batches the `IN` list in chunks of
//!   [`config::ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
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

use config::ID_BATCH_SIZE;
use rusqlite::params_from_iter;

use crate::entity::{Entity, EntityDao};
use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

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
    /// The `IN` list is batched in chunks of [`config::ID_BATCH_SIZE`]
    /// (design D9); input ids are de-duplicated, and the entity rows come
    /// from [`EntityDao::get_by_ids`] (DRY, full column set).
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

    /// A (document, entity) fixture without chunks; returns their ids.
    fn seed_doc_and_entity(db: &Db, path: &str) -> (i64, i64) {
        db.with_conn(|conn| -> Result<(i64, i64), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc_id = docs.create("markdown", path, None, None)?;
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let entity_id = entities.create("PERSON", "Smoke", "smoke", None, None, None)?;
            Ok((doc_id, entity_id))
        })
        .unwrap()
        .unwrap()
    }

    /// A (document, chunk, entity) fixture; returns their ids.
    fn seed_chunk_and_entity(db: &Db, path: &str) -> (i64, i64, i64) {
        let (doc_id, entity_id) = seed_doc_and_entity(db, path);
        let chunk_id = db
            .with_conn(|conn| {
                let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
                chunks.create(doc_id, "smoke text", 0, None, None)
            })
            .unwrap()
            .unwrap();
        (doc_id, chunk_id, entity_id)
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

    // (a, cont.) unlink of an unlinked pair reports `false` (no error).
    #[test]
    fn unlink_miss_reports_false() {
        let db = in_memory_db();
        let (_doc_id, chunk_id, entity_id) = seed_chunk_and_entity(&db, "/t/ce-unlink-miss.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(!links.unlink(chunk_id, entity_id)?, "unlinked pair → false");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (b) get_entities_by_chunk / get_chunks_by_entity: multiple links,
    // id-ordered, empty for ids without links.
    #[test]
    fn listings_multiple_links_id_ordered() {
        let db = in_memory_db();
        let (doc_id, chunk_id, entity_id) = seed_chunk_and_entity(&db, "/t/ce-listings.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));

            let e2 = entities.create("PERSON", "E2", "ce", None, None, None)?;
            let e3 = entities.create("PERSON", "E3", "ce", None, None, None)?;
            links.link(chunk_id, entity_id)?;
            links.link(chunk_id, e2)?;
            links.link(chunk_id, e3)?;
            let chunk2 = chunks.create(doc_id, "second", 1, None, None)?;
            links.link(chunk2, entity_id)?;

            // Autoincrement ids: entity_id < e2 < e3, chunk_id < chunk2.
            assert_eq!(
                links.get_entities_by_chunk(chunk_id)?,
                vec![entity_id, e2, e3],
                "ordered by entity id"
            );
            assert_eq!(
                links.get_chunks_by_entity(entity_id)?,
                vec![chunk_id, chunk2],
                "ordered by chunk id"
            );
            assert!(links.get_entities_by_chunk(999_999)?.is_empty());
            assert!(links.get_chunks_by_entity(999_999)?.is_empty());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (c) unlink_chunk / unlink_entity remove ALL links of the side; a miss
    // reports `false`.
    #[test]
    fn unlink_chunk_and_unlink_entity_remove_all_links() {
        let db = in_memory_db();
        let (doc_id, chunk_id, entity_id) = seed_chunk_and_entity(&db, "/t/ce-unlink-all.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));

            let e2 = entities.create("PERSON", "E2", "ce", None, None, None)?;
            let chunk2 = chunks.create(doc_id, "second", 1, None, None)?;
            links.link(chunk_id, e2)?;
            links.link(chunk2, entity_id)?;

            assert!(!links.unlink_chunk(999_999)?, "unknown chunk → false");
            assert!(
                links.unlink_chunk(chunk_id)?,
                "all links of the chunk must go"
            );
            assert!(links.get_entities_by_chunk(chunk_id)?.is_empty());
            assert!(links.is_linked(chunk2, entity_id)?, "other chunks stay");
            assert!(!links.unlink_chunk(chunk_id)?, "nothing left → false");

            assert!(
                links.unlink_entity(entity_id)?,
                "all links of the entity must go"
            );
            assert!(links.get_chunks_by_entity(entity_id)?.is_empty());
            assert!(!links.unlink_entity(entity_id)?, "nothing left → false");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (d) get_chunk_texts_by_entity: ordered by sequence_num (id tie-break,
    // NOT by chunk id), limit, and an empty Vec for limit == 0 or an entity
    // without chunks (conscious deviation: the oracle injected a
    // "<no context available>" placeholder).
    #[test]
    fn chunk_texts_ordered_by_sequence_with_limit() {
        let db = in_memory_db();
        let (doc_id, entity_id) = seed_doc_and_entity(&db, "/t/ce-texts.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            // Out-of-order ids with sequential sequence numbers (the oracle's
            // ordering test): chunk ids do NOT follow sequence order.
            let c2 = chunks.create(doc_id, "seq_2", 2, None, None)?;
            let c0 = chunks.create(doc_id, "seq_0", 0, None, None)?;
            let c1 = chunks.create(doc_id, "seq_1", 1, None, None)?;
            links.link(c2, entity_id)?;
            links.link(c0, entity_id)?;
            links.link(c1, entity_id)?;

            assert_eq!(
                links.get_chunk_texts_by_entity(entity_id, 10)?,
                vec!["seq_0", "seq_1", "seq_2"],
                "ordered by sequence_num, not by id"
            );
            assert_eq!(
                links.get_chunk_texts_by_entity(entity_id, 2)?,
                vec!["seq_0", "seq_1"],
                "limit restricts the count"
            );
            assert!(
                links.get_chunk_texts_by_entity(entity_id, 0)?.is_empty(),
                "limit == 0 → empty Vec (no placeholder)"
            );
            assert!(
                links.get_chunk_texts_by_entity(999_999, 5)?.is_empty(),
                "unknown entity → empty Vec (no placeholder)"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (d, cont.) texts of two entities stay isolated (oracle
    // MultipleEntities case).
    #[test]
    fn chunk_texts_isolated_per_entity() {
        let db = in_memory_db();
        let (doc_id, entity_a) = seed_doc_and_entity(&db, "/t/ce-texts-multi.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let entity_b = entities.create("PERSON", "EntityB", "ce", None, None, None)?;

            let chunk_a = chunks.create(doc_id, "text for A", 0, None, None)?;
            let chunk_b = chunks.create(doc_id, "text for B", 0, None, None)?;
            links.link(chunk_a, entity_a)?;
            links.link(chunk_b, entity_b)?;

            assert_eq!(
                links.get_chunk_texts_by_entity(entity_a, 5)?,
                vec!["text for A"]
            );
            assert_eq!(
                links.get_chunk_texts_by_entity(entity_b, 5)?,
                vec!["text for B"]
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // get_entity_ids_by_doc_id: DISTINCT (one entity in two chunks of the
    // doc → one id), ordered by entity id, empty for a doc without links.
    #[test]
    fn entity_ids_by_doc_dedup_and_order() {
        let db = in_memory_db();
        let (doc_id, entity_a) = seed_doc_and_entity(&db, "/t/ce-doc-ids.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));

            let entity_b = entities.create("PERSON", "EntityB", "ce", None, None, None)?;
            let entity_c = entities.create("PERSON", "EntityC", "ce", None, None, None)?;

            // chunk 1 → A and B; chunk 2 → A again (dedup) and C.
            let chunk1 = chunks.create(doc_id, "text for A and B", 0, None, None)?;
            let chunk2 = chunks.create(doc_id, "text for A and C", 1, None, None)?;
            links.link(chunk1, entity_a)?;
            links.link(chunk1, entity_b)?;
            links.link(chunk2, entity_a)?;
            links.link(chunk2, entity_c)?;

            // Autoincrement ids: entity_a < entity_b < entity_c.
            assert_eq!(
                links.get_entity_ids_by_doc_id(doc_id)?,
                vec![entity_a, entity_b, entity_c],
                "DISTINCT, ordered by entity id"
            );

            let empty_doc = docs.create("markdown", "/t/ce-doc-ids-empty.md", None, None)?;
            assert!(links.get_entity_ids_by_doc_id(empty_doc)?.is_empty());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // get_entities_by_chunks: full Entity records (the confidence column
    // survives — Go bug fix), name-sorted with id tie-break, unlinked chunk
    // ids absent, empty input → empty map.
    #[test]
    fn entities_by_chunks_full_records_sorted() {
        let db = in_memory_db();
        let (doc_id, chunk_id, entity_a) = seed_chunk_and_entity(&db, "/t/ce-by-chunks.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));

            // Same name, different ids → the id tie-break decides the order.
            let entity_b = entities.create("PERSON", "Zeta", "ce", None, Some(0.5), None)?;
            let entity_c = entities.create("PERSON", "Zeta", "ce2", None, Some(0.7), None)?;
            links.link(chunk_id, entity_a)?;
            links.link(chunk_id, entity_b)?;
            links.link(chunk_id, entity_c)?;
            let chunk2 = chunks.create(doc_id, "second chunk", 1, None, None)?;
            links.link(chunk2, entity_b)?;

            let by_chunk = links.get_entities_by_chunks(&[chunk_id, chunk2, 999_999])?;
            assert!(!by_chunk.contains_key(&999_999), "unlinked chunk id absent");

            let first = by_chunk.get(&chunk_id).expect("chunk present");
            assert_eq!(first.len(), 3);
            let names: Vec<&str> = first.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["Smoke", "Zeta", "Zeta"], "name order");
            assert_eq!(first[0].id, entity_a);
            assert_eq!(
                (first[1].id, first[2].id),
                (entity_b, entity_c),
                "name tie broken by id"
            );
            // Full row: the confidence column must survive (Go bug fix).
            assert_eq!(first[1].confidence, Some(0.5));
            assert_eq!(first[2].confidence, Some(0.7));

            let second = by_chunk.get(&chunk2).expect("chunk2 present");
            assert_eq!(second.len(), 1);
            assert_eq!(second[0].id, entity_b);

            assert!(links.get_entities_by_chunks(&[])?.is_empty());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // get_entities_by_chunks across the D9 batch boundary: 501 chunk ids →
    // two IN batches, every link found.
    #[test]
    fn entities_by_chunks_batches_over_500() {
        let db = in_memory_db();
        let (doc_id, first_entity) = seed_doc_and_entity(&db, "/t/ce-batch.md");

        let (chunk_ids, entity_ids) = db
            .exec_tx(|tx| -> Result<(Vec<i64>, Vec<i64>), DbError> {
                let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
                let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
                let links = ChunkEntityDao::new(ConnectionOrTx::Transaction(&*tx));
                let mut entity_ids = vec![first_entity];
                for i in 0..500 {
                    entity_ids.push(entities.create(
                        "PERSON",
                        &format!("E-{i}"),
                        "ce",
                        None,
                        None,
                        None,
                    )?);
                }
                let mut chunk_ids = Vec::with_capacity(entity_ids.len());
                for (i, entity_id) in entity_ids.iter().enumerate() {
                    let chunk_id =
                        chunks.create(doc_id, &format!("text {i}"), i as i64, None, None)?;
                    links.link(chunk_id, *entity_id)?;
                    chunk_ids.push(chunk_id);
                }
                Ok((chunk_ids, entity_ids))
            })
            .expect("seed commits");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let expected: HashMap<i64, i64> = chunk_ids
                .iter()
                .zip(entity_ids.iter())
                .map(|(c, e)| (*c, *e))
                .collect();
            let by_chunk = links.get_entities_by_chunks(&chunk_ids)?;
            assert_eq!(by_chunk.len(), 501);
            for (chunk_id, entities) in &by_chunk {
                assert_eq!(entities.len(), 1);
                assert_eq!(entities[0].id, expected[chunk_id]);
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Schema FK cascades: deleting a chunk, the entity or the document
    // removes the chunk_entities rows (no explicit cleanup method needed).
    #[test]
    fn schema_cascades_remove_links() {
        let db = in_memory_db();
        let (doc_id, chunk_id, entity_id) = seed_chunk_and_entity(&db, "/t/ce-cascade.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));

            let e2 = entities.create("PERSON", "E2", "ce", None, None, None)?;
            let chunk2 = chunks.create(doc_id, "second", 1, None, None)?;
            links.link(chunk_id, entity_id)?;
            links.link(chunk_id, e2)?;
            links.link(chunk2, entity_id)?;

            // Chunk deletion cascades to its links only.
            assert!(chunks.delete(chunk_id)?);
            assert!(links.get_entities_by_chunk(chunk_id)?.is_empty());
            assert!(links.is_linked(chunk2, entity_id)?, "other chunks stay");

            // Entity deletion cascades to its remaining links; the chunk stays.
            assert!(entities.delete(entity_id)?);
            assert!(links.get_chunks_by_entity(entity_id)?.is_empty());
            assert!(links.get_entities_by_chunk(chunk2)?.is_empty());
            assert!(chunks.get_by_id(chunk2)?.is_some(), "chunk must survive");

            // Document deletion cascades chunks AND their links.
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            assert!(docs.delete(doc_id)?);
            assert!(chunks.get_by_id(chunk2)?.is_none());
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM chunk_entities", [], |r| r.get(0))?;
            assert_eq!(count, 0, "all junction rows cascaded");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
