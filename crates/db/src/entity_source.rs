//! Entity provenance storage over the `entity_sources` table (entity →
//! document links).
//!
//! Oracle mapping: `../synopsis/internal/database/dao/entity_source_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `link_batch` keeps the oracle's single multi-row `INSERT OR IGNORE`
//!   shape, batched in rows of [`LINK_BATCH_SIZE`]: 500 × 2 = 1000 bound
//!   parameters per statement stay far below SQLite's 32766 bound (design
//!   D9). Duplicates in the input (across or inside a batch) are skipped by
//!   `OR IGNORE`, so no pre-de-duplication is needed.
//! - `delete_by_document_id`, `get_documents_by_entity_id` and
//!   `find_orphaned_entity_ids` order their results (the oracle returned
//!   them in arbitrary row order).
//! - `create` returns the generated row id via `RETURNING` (the oracle read
//!   `LastInsertId` — same value, one round-trip less).
//!
//! Deletion of an entity or a document cascades to `entity_sources` per the
//! schema FKs (no explicit cleanup method needed, as in the oracle).

use rusqlite::params_from_iter;

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Maximum rows per single multi-row `INSERT` statement (design D9): 500
/// rows × 2 columns = 1000 bound parameters, far below SQLite's 32766.
const LINK_BATCH_SIZE: usize = 500;

/// Provenance links between entities and the documents they were extracted
/// from (`entity_sources` table).
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewEntitySourceDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DbError, EntitySourceDao};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
///     let id = sources.create(1, 1)?;
///     assert!(id > 0);
///     assert_eq!(sources.get_documents_by_entity_id(1)?, vec![1]);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct EntitySourceDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> EntitySourceDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert one provenance row and return its generated id. A duplicate
    /// `(entity_id, document_id)` pair violates the unique index and
    /// surfaces as [`DbError::Sqlite`] (use [`Self::link_batch`] for
    /// idempotent bulk linking).
    pub fn create(&self, entity_id: i64, document_id: i64) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO entity_sources (entity_id, document_id) VALUES (?1, ?2) RETURNING id",
            [entity_id, document_id],
            |row| row.get(0),
        )
    }

    /// Bulk-link `entity_ids` to `document_id` with multi-row
    /// `INSERT OR IGNORE` statements of at most [`LINK_BATCH_SIZE`] rows
    /// each (idempotent: existing pairs are skipped). Empty `entity_ids` is
    /// a no-op.
    pub fn link_batch(&self, document_id: i64, entity_ids: &[i64]) -> Result<(), DbError> {
        for batch in entity_ids.chunks(LINK_BATCH_SIZE) {
            let rows = vec!["(?, ?)"; batch.len()].join(", ");
            let sql = format!(
                "INSERT OR IGNORE INTO entity_sources (entity_id, document_id) VALUES {rows}"
            );
            // Row-wise parameter order: (entity_id, document_id) per row.
            self.exec.execute(
                &sql,
                params_from_iter(batch.iter().flat_map(|id| [*id, document_id])),
            )?;
        }
        Ok(())
    }

    /// Remove all provenance rows of one document and return the distinct
    /// affected entity ids BEFORE deletion (the input of a scoped orphan
    /// cleanup), ordered by entity id.
    pub fn delete_by_document_id(&self, document_id: i64) -> Result<Vec<i64>, DbError> {
        let entity_ids = self.exec.query(
            "SELECT DISTINCT entity_id FROM entity_sources WHERE document_id = ? \
             ORDER BY entity_id",
            [document_id],
            |row| row.get(0),
        )?;
        self.exec.execute(
            "DELETE FROM entity_sources WHERE document_id = ?",
            [document_id],
        )?;
        Ok(entity_ids)
    }

    /// All document ids that reference one entity, ordered by document id.
    pub fn get_documents_by_entity_id(&self, entity_id: i64) -> Result<Vec<i64>, DbError> {
        self.exec.query(
            "SELECT document_id FROM entity_sources WHERE entity_id = ? ORDER BY document_id",
            [entity_id],
            |row| row.get(0),
        )
    }

    /// Ids of entities with zero `entity_sources` rows, ordered by id.
    /// Entities of type `'EntityType'` are shared infrastructure and are
    /// NEVER reported (they must not be auto-deleted).
    pub fn find_orphaned_entity_ids(&self) -> Result<Vec<i64>, DbError> {
        self.exec.query(
            "SELECT e.id FROM entities e \
             LEFT JOIN entity_sources es ON e.id = es.entity_id \
             WHERE es.id IS NULL AND e.type != 'EntityType' \
             ORDER BY e.id",
            [],
            |row| row.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::document::DocumentDao;
    use crate::entity::EntityDao;
    use crate::test_util::in_memory_db;

    /// A (document, entity) fixture; returns their ids.
    fn seed(db: &Db, path: &str) -> (i64, i64) {
        db.with_conn(|conn| -> Result<(i64, i64), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc_id = docs.create("markdown", path, None, None)?;
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let entity_id = entities.create("PERSON", "Alice", "", None, None, None)?;
            Ok((doc_id, entity_id))
        })
        .unwrap()
        .unwrap()
    }

    // Smoke: create + get_documents_by_entity_id round-trip.
    #[test]
    fn create_then_get_documents_round_trip() {
        let db = in_memory_db();
        let (doc, entity) = seed(&db, "/smoke/entity_source.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
            let id = sources.create(entity, doc)?;
            assert!(id > 0, "generated id must be positive");
            assert_eq!(sources.get_documents_by_entity_id(entity)?, vec![doc]);
            assert!(
                sources.get_documents_by_entity_id(999_999)?.is_empty(),
                "unknown entity → empty"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Smoke: link_batch is idempotent and crosses the 500-row batch
    // boundary (501 rows → two INSERT statements, no duplicates).
    #[test]
    fn link_batch_is_idempotent_and_batches() {
        let db = in_memory_db();
        let (doc, first_entity_id) = seed(&db, "/smoke/entity_source_batch.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let mut ids = vec![first_entity_id];
            for i in 0..500 {
                ids.push(entities.create("PERSON", &format!("E-{i}"), "", None, None, None)?);
            }
            sources.link_batch(doc, &ids)?;
            sources.link_batch(doc, &ids)?; // idempotent: same rows, no error

            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM entity_sources", [], |r| r.get(0))?;
            assert_eq!(
                count, 501,
                "501 rows across the batch boundary, no duplicates"
            );
            assert_eq!(
                sources.get_documents_by_entity_id(first_entity_id)?,
                vec![doc]
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Smoke: delete_by_document_id returns the affected ids BEFORE deleting.
    #[test]
    fn delete_by_document_id_returns_affected_ids() {
        let db = in_memory_db();
        let (doc, entity) = seed(&db, "/smoke/entity_source_delete.md");
        let other = db
            .with_conn(|conn| {
                let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
                entities.create("PERSON", "Bob", "", None, None, None)
            })
            .unwrap()
            .unwrap();

        db.with_conn(|conn| -> Result<(), DbError> {
            let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
            sources.link_batch(doc, &[entity, other])?;
            let affected = sources.delete_by_document_id(doc)?;
            assert_eq!(affected, vec![entity, other], "affected ids, ordered");
            assert!(
                sources.get_documents_by_entity_id(entity)?.is_empty(),
                "rows must be gone after deletion"
            );
            // Deleting again yields an empty id list (no rows left).
            assert!(sources.delete_by_document_id(doc)?.is_empty());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Smoke: find_orphaned_entity_ids excludes 'EntityType' entities.
    #[test]
    fn find_orphaned_entity_ids_excludes_entity_type() {
        let db = in_memory_db();
        let (doc, linked) = seed(&db, "/smoke/entity_source_orphans.md");

        db.with_conn(|conn| -> Result<(), DbError> {
            let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let orphan = entities.create("PERSON", "Orphan", "", None, None, None)?;
            let type_id = entities.create("EntityType", "PERSON", "", None, None, None)?;
            sources.create(linked, doc)?;

            let found = sources.find_orphaned_entity_ids()?;
            assert_eq!(found, vec![orphan], "only the orphan, EntityType excluded");
            assert!(!found.contains(&type_id));
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
