//! Fact provenance storage over the `fact_sources` table (fact →
//! source document + quote links).
//!
//! Oracle mapping: `../synopsis/internal/database/dao/fact_source_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `document_id` is an `i64` end to end (human decision 2026-08-20, task
//!   1.14 revision 2): the oracle is self-inconsistent — its schema
//!   declared `fact_sources.document_id TEXT NOT NULL` while the Go code
//!   typed the field `int` (relying on SQLite type affinity). The model is
//!   corrected: the squashed init migration now declares
//!   `INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE`, exactly
//!   like the neighbouring `entity_sources.document_id`.
//! - `delete` and `delete_by_fact_id` return `bool` (`false` = no such
//!   row) instead of the oracle's "not found" error (house convention, as
//!   in `fact.rs`/`entity.rs`).
//! - `create` takes `quote` and `extracted_at` as `Option`s: `None`
//!   leaves the column `NULL` / at its `CURRENT_TIMESTAMP` default. The
//!   oracle inserted `extracted_at` unconditionally, and callers that
//!   passed the zero value produced empty timestamps that only a data
//!   backfill migration (`004`) repaired after the fact.
//! - The oracle's `fact_id == 0` guard in `create` is a Go zero-value
//!   idiom (it has no `Option<i64>`); the id is a required parameter and
//!   the real invariant is the schema FK, so the guard is dropped.
//! - `delete_by_document_id` orders its affected fact ids by fact id
//!   (the oracle returned them in arbitrary row order).
//! - `create` returns the generated row id via `RETURNING` (the oracle
//!   read `LastInsertId` — same value, one round-trip less).
//!
//! Deleting a fact or a document cascades to its `fact_sources` rows per
//! the schema FKs (no explicit cleanup needed, as in the oracle).

use rusqlite::{Row, params};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Shared `SELECT` for the `fact_sources` row queries (column order is the
/// contract of [`row_to_fact_source`]).
const SELECT_FACT_SOURCE: &str = "SELECT id, fact_id, document_id, quote, extracted_at \
     FROM fact_sources";

/// One provenance row of a fact: where it was extracted from and the exact
/// quote (`fact_sources` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactSource {
    /// Row id (autoincrement).
    pub id: i64,
    /// The fact this source backs (FK to `facts.id`, cascades on fact
    /// delete).
    pub fact_id: i64,
    /// The source document (FK to `documents.id`, cascades on document
    /// delete).
    pub document_id: i64,
    /// Exact quote from the source, if any.
    pub quote: Option<String>,
    /// Extraction timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub extracted_at: String,
}

/// CRUD + scoped cleanup over the `fact_sources` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewFactSourceDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DbError, FactSourceDao};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let sources = FactSourceDao::new(ConnectionOrTx::Connection(conn));
///     let id = sources.create(1, 2, Some("quote"), None)?;
///     assert!(id > 0);
///     let got = sources.get_by_fact_id(1)?;
///     assert_eq!(got.first().map(|s| s.id), Some(id));
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct FactSourceDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> FactSourceDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert a new fact source and return its generated id. `quote` is
    /// stored as `NULL` when `None`; `extracted_at` falls back to
    /// `CURRENT_TIMESTAMP` when `None` (the schema's `DEFAULT` is not
    /// applied to an explicit `NULL`, hence the `COALESCE`).
    pub fn create(
        &self,
        fact_id: i64,
        document_id: i64,
        quote: Option<&str>,
        extracted_at: Option<&str>,
    ) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO fact_sources (fact_id, document_id, quote, extracted_at) \
             VALUES (?1, ?2, ?3, COALESCE(?4, CURRENT_TIMESTAMP)) RETURNING id",
            params![fact_id, document_id, quote, extracted_at],
            |row| row.get(0),
        )
    }

    /// All sources of one fact, ordered by id.
    pub fn get_by_fact_id(&self, fact_id: i64) -> Result<Vec<FactSource>, DbError> {
        self.exec.query(
            &format!("{SELECT_FACT_SOURCE} WHERE fact_id = ? ORDER BY id"),
            [fact_id],
            row_to_fact_source,
        )
    }

    /// Delete one source row by id. Returns `true` if a row was deleted,
    /// `false` if no row has `id`.
    pub fn delete(&self, id: i64) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM fact_sources WHERE id = ?", [id])?;
        Ok(changed > 0)
    }

    /// Remove all sources of one fact. Returns `true` if at least one row
    /// was deleted, `false` if the fact had no sources.
    pub fn delete_by_fact_id(&self, fact_id: i64) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM fact_sources WHERE fact_id = ?", [fact_id])?;
        Ok(changed > 0)
    }

    /// Remove all sources of one document and return the distinct affected
    /// fact ids BEFORE deletion (the input of a scoped orphan cleanup),
    /// ordered by fact id.
    pub fn delete_by_document_id(&self, document_id: i64) -> Result<Vec<i64>, DbError> {
        let fact_ids = self.exec.query(
            "SELECT DISTINCT fact_id FROM fact_sources WHERE document_id = ? \
             ORDER BY fact_id",
            [document_id],
            |row| row.get(0),
        )?;
        self.exec.execute(
            "DELETE FROM fact_sources WHERE document_id = ?",
            [document_id],
        )?;
        Ok(fact_ids)
    }
}

/// Map a `fact_sources` row (in [`SELECT_FACT_SOURCE`] column order) to a
/// [`FactSource`].
fn row_to_fact_source(row: &Row<'_>) -> rusqlite::Result<FactSource> {
    Ok(FactSource {
        id: row.get(0)?,
        fact_id: row.get(1)?,
        document_id: row.get(2)?,
        quote: row.get(3)?,
        extracted_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::document::DocumentDao;
    use crate::entity::EntityDao;
    use crate::fact::FactDao;
    use crate::test_util::in_memory_db;

    /// Seed a document (the `document_id` FK requires an existing document;
    /// `original_path` is unique).
    fn seed_document(db: &Db, path: &str) -> i64 {
        db.with_conn(|conn| {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            docs.create("markdown", path, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Seed one fact (the `fact_sources` FK requires an existing fact, and
    /// the fact FKs require existing entities; `foreign_keys=ON`) and
    /// return its id.
    fn seed_fact(db: &Db) -> i64 {
        db.with_conn(|conn| -> Result<i64, DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let subject = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let object = entities.create("ORGANIZATION", "Acme", "hr", None, None, None)?;
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(subject),
                "works_at",
                Some(object),
                "hr",
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap()
    }

    // Smoke: create + get_by_fact_id round-trip (full suite — task 1.15).
    #[test]
    fn create_then_get_by_fact_id_round_trip() {
        let db = in_memory_db();
        let doc1 = seed_document(&db, "/docs/hr.md");
        let doc2 = seed_document(&db, "/docs/it.md");
        let fact_id = seed_fact(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let sources = FactSourceDao::new(ConnectionOrTx::Connection(conn));
            let id = sources.create(fact_id, doc1, Some("exact quote"), None)?;
            assert!(id > 0, "generated id must be positive");

            let got = sources.get_by_fact_id(fact_id)?;
            assert_eq!(got.len(), 1, "one source for the fact");
            let row = &got[0];
            assert_eq!(row.id, id);
            assert_eq!(row.fact_id, fact_id);
            assert_eq!(row.document_id, doc1);
            assert_eq!(row.quote.as_deref(), Some("exact quote"));
            assert!(!row.extracted_at.is_empty(), "CURRENT_TIMESTAMP default");

            // A second source of the same fact is allowed (no unique
            // constraint) and keeps the id order.
            let id2 = sources.create(fact_id, doc2, None, None)?;
            let got = sources.get_by_fact_id(fact_id)?;
            assert_eq!(
                got.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![id, id2],
                "ordered by id"
            );
            assert_eq!(got[1].quote, None, "None quote → NULL");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
