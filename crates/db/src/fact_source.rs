//! Fact provenance storage over the `fact_sources` table (fact →
//! source document + quote links).
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

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_sources<T>(db: &Db, f: impl FnOnce(&FactSourceDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&FactSourceDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

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

    /// Seed an endpoint entity pair (one subject, one object, same domain)
    /// and return their ids; `label` keeps the unique (type, name, domain)
    /// key fresh across calls.
    fn seed_entities(db: &Db, label: &str) -> (i64, i64) {
        db.with_conn(|conn| -> Result<(i64, i64), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let subject =
                entities.create("PERSON", &format!("Alice-{label}"), "hr", None, None, None)?;
            let object = entities.create(
                "ORGANIZATION",
                &format!("Acme-{label}"),
                "hr",
                None,
                None,
                None,
            )?;
            Ok((subject, object))
        })
        .unwrap()
        .unwrap()
    }

    /// Seed one fact over the given endpoint entities (the `fact_sources`
    /// `fact_id` FK requires an existing fact, and the fact's endpoint FKs
    /// require existing entities; `foreign_keys=ON`) and return its id.
    fn seed_fact(db: &Db, subject: i64, predicate: &str, object: i64) -> i64 {
        db.with_conn(|conn| {
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(subject),
                predicate,
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

    // (a1) CRUD round-trip: create + get_by_fact_id, all fields, id order,
    // a second source of the same fact is allowed (no unique constraint).
    #[test]
    fn create_then_get_by_fact_id_round_trip() {
        let db = in_memory_db();
        let doc1 = seed_document(&db, "/docs/hr.md");
        let doc2 = seed_document(&db, "/docs/it.md");
        let (subject, object) = seed_entities(&db, "smoke");
        let fact_id = seed_fact(&db, subject, "works_at", object);

        with_sources(&db, |sources| {
            let id = sources
                .create(fact_id, doc1, Some("exact quote"), None)
                .unwrap();
            assert!(id > 0, "generated id must be positive");

            let got = sources.get_by_fact_id(fact_id).unwrap();
            assert_eq!(got.len(), 1, "one source for the fact");
            let row = &got[0];
            assert_eq!(row.id, id);
            assert_eq!(row.fact_id, fact_id);
            assert_eq!(row.document_id, doc1);
            assert_eq!(row.quote.as_deref(), Some("exact quote"));
            assert!(!row.extracted_at.is_empty(), "CURRENT_TIMESTAMP default");

            // A second source of the same fact is allowed (no unique
            // constraint) and keeps the id order.
            let id2 = sources.create(fact_id, doc2, None, None).unwrap();
            let got = sources.get_by_fact_id(fact_id).unwrap();
            assert_eq!(
                got.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![id, id2],
                "ordered by id"
            );
            assert_eq!(got[1].quote, None, "None quote → NULL");
        });
    }

    // (a2) create: an explicit extracted_at is stored verbatim (the
    // CURRENT_TIMESTAMP default is only the `None` fallback), and an
    // empty-string quote is a value, not NULL.
    #[test]
    fn create_stores_explicit_extracted_at_and_empty_quote() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "explicit");
        let fact_id = seed_fact(&db, subject, "works_at", object);

        with_sources(&db, |sources| {
            let id = sources
                .create(fact_id, doc, Some(""), Some("2026-01-02 03:04:05"))
                .unwrap();
            let row = &sources.get_by_fact_id(fact_id).unwrap()[0];
            assert_eq!(row.id, id);
            assert_eq!(
                row.quote.as_deref(),
                Some(""),
                "an empty string is a value, not NULL"
            );
            assert_eq!(
                row.extracted_at, "2026-01-02 03:04:05",
                "the explicit value must be stored verbatim"
            );
        });
    }

    // (a3) create: the schema FKs are enforced (foreign_keys=ON) — a missing
    // fact or a missing document is a Sqlite error. This pins the i64
    // `document_id` contract (human decision 2026-08-20): the column is an
    // INTEGER FK to documents(id), not the oracle's TEXT.
    #[test]
    fn create_rejects_missing_fact_or_document() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "fk");
        let fact_id = seed_fact(&db, subject, "works_at", object);

        with_sources(&db, |sources| {
            let err = sources
                .create(999_999, doc, None, None)
                .expect_err("a missing fact must fail");
            assert!(
                matches!(err, DbError::Sqlite { .. }),
                "missing fact_id must be an FK violation, got {err:?}"
            );

            let err = sources
                .create(fact_id, 999_999, None, None)
                .expect_err("a missing document must fail");
            assert!(
                matches!(err, DbError::Sqlite { .. }),
                "missing document_id must be an FK violation, got {err:?}"
            );
        });
    }

    // (b) get_by_fact_id: scoped to the fact (other facts' sources are not
    // returned), id order, empty for an unknown fact.
    #[test]
    fn get_by_fact_id_scopes_to_the_fact() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (a_subject, a_object) = seed_entities(&db, "a");
        let (b_subject, b_object) = seed_entities(&db, "b");
        let fact_a = seed_fact(&db, a_subject, "p_a", a_object);
        let fact_b = seed_fact(&db, b_subject, "p_b", b_object);

        with_sources(&db, |sources| {
            let a1 = sources.create(fact_a, doc, Some("a1"), None).unwrap();
            let a2 = sources.create(fact_a, doc, Some("a2"), None).unwrap();
            let b1 = sources.create(fact_b, doc, Some("b1"), None).unwrap();

            let got_a: Vec<i64> = sources
                .get_by_fact_id(fact_a)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(got_a, vec![a1, a2], "only fact A's sources, id order");

            let got_b: Vec<i64> = sources
                .get_by_fact_id(fact_b)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(got_b, vec![b1], "only fact B's source");

            assert!(
                sources.get_by_fact_id(999_999).unwrap().is_empty(),
                "unknown fact → empty"
            );
        });
    }

    // (c1) delete: true on a hit, false on a miss; only the addressed row
    // is removed.
    #[test]
    fn delete_reports_hit_and_miss() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "del");
        let fact_id = seed_fact(&db, subject, "works_at", object);

        with_sources(&db, |sources| {
            let id1 = sources.create(fact_id, doc, Some("q1"), None).unwrap();
            let id2 = sources.create(fact_id, doc, Some("q2"), None).unwrap();

            assert!(sources.delete(id1).unwrap(), "existing id must report true");
            assert!(
                !sources.delete(id1).unwrap(),
                "second delete must report false"
            );
            assert!(
                !sources.delete(999_999).unwrap(),
                "unknown id must report false"
            );

            let remaining: Vec<i64> = sources
                .get_by_fact_id(fact_id)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(remaining, vec![id2], "only the other row survives");
        });
    }

    // (c2) delete_by_fact_id: removes every source of the fact (true),
    // leaves other facts' sources alone, false when nothing was removed.
    #[test]
    fn delete_by_fact_id() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (a_subject, a_object) = seed_entities(&db, "a");
        let (b_subject, b_object) = seed_entities(&db, "b");
        let fact_a = seed_fact(&db, a_subject, "p_a", a_object);
        let fact_b = seed_fact(&db, b_subject, "p_b", b_object);

        with_sources(&db, |sources| {
            sources.create(fact_a, doc, Some("a1"), None).unwrap();
            sources.create(fact_a, doc, Some("a2"), None).unwrap();
            let b1 = sources.create(fact_b, doc, Some("b1"), None).unwrap();

            assert!(
                sources.delete_by_fact_id(fact_a).unwrap(),
                "the fact had sources"
            );
            assert!(
                sources.get_by_fact_id(fact_a).unwrap().is_empty(),
                "every source of the fact is gone"
            );
            let remaining_b: Vec<i64> = sources
                .get_by_fact_id(fact_b)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(remaining_b, vec![b1], "other facts' sources survive");

            assert!(
                !sources.delete_by_fact_id(fact_a).unwrap(),
                "no sources left → false"
            );
            assert!(
                !sources.delete_by_fact_id(999_999).unwrap(),
                "unknown fact → false"
            );
        });
    }

    // (c3) delete_by_document_id: returns the DISTINCT affected fact ids in
    // fact-id order BEFORE deletion, removes only that document's sources.
    #[test]
    fn delete_by_document_id_returns_affected_fact_ids_ordered() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let other_doc = seed_document(&db, "/docs/it.md");
        let (a_subject, a_object) = seed_entities(&db, "a");
        let (b_subject, b_object) = seed_entities(&db, "b");
        let fact_a = seed_fact(&db, a_subject, "p_a", a_object);
        let fact_b = seed_fact(&db, b_subject, "p_b", b_object);

        with_sources(&db, |sources| {
            // Insert in a non-id order (fact B first, twice) to prove the
            // result is DISTINCT and ordered by fact id, not insertion
            // order.
            sources.create(fact_b, doc, Some("b1"), None).unwrap();
            sources.create(fact_a, doc, Some("a1"), None).unwrap();
            sources.create(fact_b, doc, Some("b2"), None).unwrap();
            let other_id = sources
                .create(fact_a, other_doc, Some("other"), None)
                .unwrap();

            let affected = sources.delete_by_document_id(doc).unwrap();
            assert_eq!(
                affected,
                vec![fact_a, fact_b],
                "distinct fact ids, id order"
            );

            let remaining_a: Vec<i64> = sources
                .get_by_fact_id(fact_a)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(
                remaining_a,
                vec![other_id],
                "the other document's source survives"
            );
            assert!(
                sources.get_by_fact_id(fact_b).unwrap().is_empty(),
                "every source of the document is gone"
            );

            assert!(
                sources.delete_by_document_id(999_999).unwrap().is_empty(),
                "unknown document → no affected facts"
            );
            assert!(
                sources.delete_by_document_id(doc).unwrap().is_empty(),
                "already deleted → empty"
            );
        });
    }

    // (d1) deleting a fact cascades to its fact_sources rows (schema FK).
    #[test]
    fn fact_delete_cascades_sources() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "casc");
        let fact_id = seed_fact(&db, subject, "works_at", object);
        with_sources(&db, |sources| {
            sources.create(fact_id, doc, Some("q1"), None).unwrap();
            sources.create(fact_id, doc, Some("q2"), None).unwrap();
        });

        let deleted = db
            .with_conn(|conn| {
                let facts = FactDao::new(ConnectionOrTx::Connection(conn));
                facts.delete(fact_id)
            })
            .unwrap()
            .unwrap();
        assert!(deleted, "the fact existed");

        with_sources(&db, |sources| {
            assert!(
                sources.get_by_fact_id(fact_id).unwrap().is_empty(),
                "the sources must cascade"
            );
        });
    }

    // (d2) deleting a document cascades to its fact_sources rows (schema FK,
    // human decision 2026-08-20: INTEGER document_id with ON DELETE CASCADE).
    #[test]
    fn document_delete_cascades_sources() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "doc-casc");
        let fact_id = seed_fact(&db, subject, "works_at", object);
        with_sources(&db, |sources| {
            sources.create(fact_id, doc, Some("q1"), None).unwrap();
            sources.create(fact_id, doc, Some("q2"), None).unwrap();
        });

        let deleted = db
            .with_conn(|conn| {
                let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
                docs.delete(doc)
            })
            .unwrap()
            .unwrap();
        assert!(deleted, "the document existed");

        with_sources(&db, |sources| {
            assert!(
                sources.get_by_fact_id(fact_id).unwrap().is_empty(),
                "the sources must cascade"
            );
        });
    }

    // (e) the DAO works over a transaction: commit and rollback paths
    // (house pattern, as in the sibling DAOs).
    #[test]
    fn create_inside_transaction() {
        let db = in_memory_db();
        let doc = seed_document(&db, "/docs/hr.md");
        let (subject, object) = seed_entities(&db, "tx");
        let fact_id = seed_fact(&db, subject, "works_at", object);

        db.exec_tx(|tx| -> Result<(), DbError> {
            let sources = FactSourceDao::new(ConnectionOrTx::Transaction(&*tx));
            sources.create(fact_id, doc, Some("committed"), None)?;
            Ok(())
        })
        .expect("commit");

        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let sources = FactSourceDao::new(ConnectionOrTx::Transaction(&*tx));
                sources.create(fact_id, doc, Some("rolled back"), None)?;
                // A genuine failure after a partial write: FK (missing
                // document).
                sources.create(fact_id, 999_999, None, None)?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        with_sources(&db, |sources| {
            let rows = sources.get_by_fact_id(fact_id).unwrap();
            let quotes: Vec<&str> = rows
                .iter()
                .map(|s| s.quote.as_deref().expect("the seeded quote"))
                .collect();
            assert_eq!(quotes, vec!["committed"], "only the committed source");
        });
    }
}
