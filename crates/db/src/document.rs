//! Document storage over the `documents` table.
//!
//! The v5 schema has NO `domain` column on `documents`: a document's
//! domain(s) live in `metadata_json` under `$.domain` as a string or an
//! array of strings, and are filtered/counted with `json_each`. The
//! `ner_status` operations are deliberately absent: the column does not
//! exist in any migration, so those methods could only ever fail at
//! runtime (dead code; YAGNI).
//!
//! **Design:**
//! - `get_by_ids` returns a `Vec<Document>` (Rust idiom; callers that need
//!   lookup build a map themselves);
//! - `update`/`update_hash`/`delete` return `bool` (`false` = no such id)
//!   instead of a "document not found" error — the caller decides the
//!   semantics, and no per-DAO error variant is needed;
//! - `unique_domains` is ordered (`ORDER BY` value) for a deterministic
//!   API;
//! - the domain filter checks `metadata_json IS NOT NULL AND
//!   json_valid(metadata_json)` in the outer `WHERE` before the
//!   `json_each` subquery, so a malformed-metadata row can never make the
//!   query fail (a check inside the `EXISTS` cannot protect the `FROM`
//!   evaluation).

use std::collections::HashMap;

use config::ID_BATCH_SIZE;
use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::escape_like;

/// Shared `SELECT` list for the `documents` row queries (column order is
/// the contract of [`row_to_document`]).
const SELECT_DOCUMENT: &str = "SELECT id, source_type, original_path, metadata_json, content_hash, created_at, updated_at \
     FROM documents";

/// Shared `WHERE` for filtered listing and counting. Fixed shape: each
/// optional filter compares against a possibly-`NULL` parameter, so the SQL
/// is never assembled from user input and the same clause serves
/// [`DocumentDao::list_paginated`] and [`DocumentDao::count`] (DRY).
const FILTER_WHERE: &str = "WHERE (?1 IS NULL OR (metadata_json IS NOT NULL AND json_valid(metadata_json) \
     AND EXISTS (SELECT 1 FROM json_each(metadata_json, '$.domain') WHERE json_each.value = ?1))) \
     AND (?2 IS NULL OR source_type = ?2) \
     AND (?3 IS NULL OR lower(original_path) LIKE lower(?3) ESCAPE '\\')";

/// A source document in the knowledge base (one row of the v5 `documents`
/// table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Row id (autoincrement).
    pub id: i64,
    /// Originating source (`'json'`, `'markdown'`, `'steam'`, `'unstructured'`).
    pub source_type: String,
    /// Path of the original file.
    pub original_path: String,
    /// Metadata as a JSON string, if any.
    pub metadata_json: Option<String>,
    /// SHA-256 of the document content for deduplication, if any.
    pub content_hash: Option<String>,
    /// Creation timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub created_at: String,
    /// Last-update timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub updated_at: String,
}

/// Optional filters for [`DocumentDao::list_paginated`] and
/// [`DocumentDao::count`]; a `None` (or empty) member is not applied
/// ("empty string = no filter" semantics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentFilter {
    /// Match documents whose `metadata_json` `$.domain` equals this value
    /// (string or array member).
    pub domain: Option<String>,
    /// Match documents with exactly this source type.
    pub source_type: Option<String>,
    /// Case-insensitive substring match on the original path (`LIKE`, with
    /// `%`/`_`/`\` escaped so user input is matched literally).
    pub name: Option<String>,
}

impl DocumentFilter {
    /// The bound-parameter triple for [`FILTER_WHERE`]: the name filter is
    /// escaped and wrapped as a `LIKE` substring pattern.
    fn args(&self) -> (Option<String>, Option<String>, Option<String>) {
        let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
        (
            non_empty(self.domain.clone()),
            non_empty(self.source_type.clone()),
            self.name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!("%{}%", escape_like(n))),
        )
    }
}

/// CRUD + pagination + domain operations over the `documents` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`].
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DocumentDao, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// let id = db.with_conn(|conn| -> Result<i64, DbError> {
///     let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
///     let id = docs.create("markdown", "/docs/hr.md", Some(r#"{"domain":"hr"}"#), None)?;
///     assert_eq!(docs.get_by_id(id)?.map(|d| d.id), Some(id));
///     Ok(id)
/// })??;
/// assert!(id > 0);
/// # Ok(())
/// # }
/// ```
pub struct DocumentDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> DocumentDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert a new document and return its generated id. `metadata_json`
    /// and `content_hash` are stored as `NULL` when `None`.
    pub fn create(
        &self,
        source_type: &str,
        original_path: &str,
        metadata_json: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO documents (source_type, original_path, metadata_json, content_hash) \
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![source_type, original_path, metadata_json, content_hash],
            |row| row.get(0),
        )
    }

    /// Retrieve a document by id, or `None` if absent.
    pub fn get_by_id(&self, id: i64) -> Result<Option<Document>, DbError> {
        let rows = self.exec.query(
            &format!("{SELECT_DOCUMENT} WHERE id = ?"),
            [id],
            row_to_document,
        )?;
        Ok(rows.into_iter().next())
    }

    /// Retrieve a document by its original path, or `None` if absent
    /// (`original_path` is unique by `idx_documents_original_path`).
    pub fn get_by_path(&self, path: &str) -> Result<Option<Document>, DbError> {
        let rows = self.exec.query(
            &format!("{SELECT_DOCUMENT} WHERE original_path = ?"),
            [path],
            row_to_document,
        )?;
        Ok(rows.into_iter().next())
    }

    /// Retrieve several documents by id; ids that do not exist are simply
    /// absent from the result. Empty `ids` yields an empty vec.
    ///
    /// The `IN` list is batched in chunks of [`config::ID_BATCH_SIZE`] to
    /// stay far below SQLite's 32766 bound on bound parameters (design D9).
    pub fn get_by_ids(&self, ids: &[i64]) -> Result<Vec<Document>, DbError> {
        let mut docs = Vec::new();
        for batch in ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!("{SELECT_DOCUMENT} WHERE id IN ({placeholders})");
            docs.extend(self.exec.query(
                &sql,
                params_from_iter(batch.iter().copied()),
                row_to_document,
            )?);
        }
        Ok(docs)
    }

    /// Update path, metadata and content hash of an existing document,
    /// refreshing `updated_at`. Returns `true` if a row was updated,
    /// `false` if no document has `id`.
    pub fn update(
        &self,
        id: i64,
        original_path: &str,
        metadata_json: Option<&str>,
        content_hash: Option<&str>,
    ) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE documents SET original_path = ?1, metadata_json = ?2, content_hash = ?3, \
             updated_at = CURRENT_TIMESTAMP WHERE id = ?4",
            params![original_path, metadata_json, content_hash, id],
        )?;
        Ok(changed > 0)
    }

    /// Set the content hash, refreshing `updated_at`. Returns `true` if a
    /// row was updated, `false` if no document has `id`.
    pub fn update_hash(&self, id: i64, hash: &str) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE documents SET content_hash = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![hash, id],
        )?;
        Ok(changed > 0)
    }

    /// All documents, newest first.
    pub fn list(&self) -> Result<Vec<Document>, DbError> {
        self.exec.query(
            &format!("{SELECT_DOCUMENT} ORDER BY created_at DESC"),
            [],
            row_to_document,
        )
    }

    /// Delete a document (its chunks cascade per the schema FK). Returns
    /// `true` if a row was deleted, `false` if no document has `id`.
    pub fn delete(&self, id: i64) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM documents WHERE id = ?", [id])?;
        Ok(changed > 0)
    }

    /// Number of documents matching `filter` (same semantics as
    /// [`Self::list_paginated`]).
    pub fn count(&self, filter: &DocumentFilter) -> Result<i64, DbError> {
        let (domain, source_type, name) = filter.args();
        self.exec.query_row(
            &format!("SELECT COUNT(*) FROM documents {FILTER_WHERE}"),
            params![domain, source_type, name],
            |row| row.get(0),
        )
    }

    /// One page of documents matching `filter`, ordered by id; returns the
    /// page and the total number of matching documents.
    pub fn list_paginated(
        &self,
        offset: i64,
        limit: i64,
        filter: &DocumentFilter,
    ) -> Result<(Vec<Document>, i64), DbError> {
        let (domain, source_type, name) = filter.args();
        let docs = self.exec.query(
            &format!("{SELECT_DOCUMENT} {FILTER_WHERE} ORDER BY id LIMIT ?4 OFFSET ?5"),
            params![domain, source_type, name, limit, offset],
            row_to_document,
        )?;
        let total = self.exec.query_row(
            &format!("SELECT COUNT(*) FROM documents {FILTER_WHERE}"),
            params![domain, source_type, name],
            |row| row.get(0),
        )?;
        Ok((docs, total))
    }

    /// Number of documents per source type.
    pub fn documents_by_type(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = self.exec.query(
            "SELECT source_type, COUNT(*) FROM documents GROUP BY source_type",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Distinct domain values across all documents' `metadata_json`
    /// `$.domain` (string or array members), sorted for determinism.
    pub fn unique_domains(&self) -> Result<Vec<String>, DbError> {
        self.exec.query(
            "SELECT DISTINCT json_each.value FROM documents, json_each(metadata_json, '$.domain') \
             WHERE metadata_json IS NOT NULL AND json_valid(metadata_json) \
               AND json_each.value IS NOT NULL AND json_each.value != '' \
             ORDER BY json_each.value",
            [],
            |row| row.get(0),
        )
    }
}

/// Map a `documents` row (in [`SELECT_DOCUMENT`] order) to a [`Document`].
fn row_to_document(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        source_type: row.get(1)?,
        original_path: row.get(2)?,
        metadata_json: row.get(3)?,
        content_hash: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::Db;
    use crate::test_util::in_memory_db;

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_docs<T>(db: &Db, f: impl FnOnce(&DocumentDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&DocumentDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Create `n` documents with distinct paths and return their ids in
    /// insertion order.
    fn seed_documents(db: &Db, n: usize) -> Vec<i64> {
        let mut ids = Vec::with_capacity(n);
        db.exec_tx(|tx| -> Result<(), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..n {
                ids.push(docs.create("markdown", &format!("/docs/doc-{i}.md"), None, None)?);
            }
            Ok(())
        })
        .expect("seed transaction commits");
        ids
    }

    /// The three-document fixture for the domain filter: array domain,
    /// scalar domain, no metadata.
    fn seed_domain_fixtures(docs: &DocumentDao<'_>) {
        docs.create(
            "markdown",
            "/docs/hr-policy.md",
            Some(r#"{"domain":["hr","engineering"]}"#),
            None,
        )
        .unwrap();
        docs.create(
            "json",
            "/data/product.json",
            Some(r#"{"domain":"product"}"#),
            None,
        )
        .unwrap();
        docs.create("markdown", "/docs/README.md", None, None)
            .unwrap();
    }

    // (a) create + get_by_id round-trip.
    #[test]
    fn create_then_get_by_id_round_trip() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            let id = docs
                .create(
                    "markdown",
                    "/docs/hr-policy.md",
                    Some(r#"{"domain":["hr"]}"#),
                    Some("sha256:abc"),
                )
                .unwrap();
            let doc = docs
                .get_by_id(id)
                .unwrap()
                .expect("created document must exist");
            assert_eq!(doc.id, id);
            assert_eq!(doc.source_type, "markdown");
            assert_eq!(doc.original_path, "/docs/hr-policy.md");
            assert_eq!(doc.metadata_json.as_deref(), Some(r#"{"domain":["hr"]}"#));
            assert_eq!(doc.content_hash.as_deref(), Some("sha256:abc"));
            assert!(!doc.created_at.is_empty(), "created_at default must be set");
            assert!(!doc.updated_at.is_empty(), "updated_at default must be set");
            assert_eq!(docs.get_by_id(999_999).unwrap(), None);
        });
    }

    // (b) get_by_path (and absence).
    #[test]
    fn get_by_path() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            let id = docs
                .create("json", "/data/product.json", None, None)
                .unwrap();
            let doc = docs
                .get_by_path("/data/product.json")
                .unwrap()
                .expect("must exist");
            assert_eq!(doc.id, id);
            assert_eq!(docs.get_by_path("/data/missing.json").unwrap(), None);
        });
    }

    // (c) get_by_ids with several ids, missing ids absent, empty input.
    #[test]
    fn get_by_ids() {
        let db = in_memory_db();
        let ids = seed_documents(&db, 3);
        with_docs(&db, |docs| {
            let found = docs.get_by_ids(&[ids[1], ids[0]]).unwrap();
            assert_eq!(found.len(), 2);
            let got: HashSet<i64> = found.iter().map(|d| d.id).collect();
            assert_eq!(got, [ids[0], ids[1]].into_iter().collect());

            let with_missing = docs.get_by_ids(&[ids[0], 999_999]).unwrap();
            assert_eq!(with_missing.len(), 1);
            assert_eq!(with_missing[0].id, ids[0]);

            assert!(docs.get_by_ids(&[]).unwrap().is_empty());
        });
    }

    // (c2) get_by_ids across the D9 batch boundary (3 × 500).
    #[test]
    fn get_by_ids_batches_over_500() {
        let db = in_memory_db();
        let ids = seed_documents(&db, 1200);
        with_docs(&db, |docs| {
            let requested: Vec<i64> = ids.iter().rev().copied().collect();
            let found = docs.get_by_ids(&requested).unwrap();
            assert_eq!(found.len(), 1200);
            let got: HashSet<i64> = found.iter().map(|d| d.id).collect();
            assert_eq!(got, ids.into_iter().collect::<HashSet<i64>>());
        });
    }

    // (d) update_hash changes content_hash; missing id → false.
    #[test]
    fn update_hash() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            let id = docs.create("markdown", "/docs/a.md", None, None).unwrap();
            assert!(docs.update_hash(id, "sha256:new").unwrap());
            let doc = docs.get_by_id(id).unwrap().unwrap();
            assert_eq!(doc.content_hash.as_deref(), Some("sha256:new"));
            assert!(
                !docs.update_hash(999_999, "x").unwrap(),
                "missing id must report false, not error"
            );
        });
    }

    // update() changes path/metadata/hash; missing id → false.
    #[test]
    fn update() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            let id = docs.create("markdown", "/docs/old.md", None, None).unwrap();
            assert!(
                docs.update(id, "/docs/new.md", Some(r#"{"domain":"hr"}"#), Some("h1"))
                    .unwrap()
            );
            let doc = docs.get_by_id(id).unwrap().unwrap();
            assert_eq!(doc.original_path, "/docs/new.md");
            assert_eq!(doc.metadata_json.as_deref(), Some(r#"{"domain":"hr"}"#));
            assert_eq!(doc.content_hash.as_deref(), Some("h1"));
            assert!(!docs.update(999_999, "/x.md", None, None).unwrap());
        });
    }

    // (e) delete removes the row; repeat → false; get → None.
    #[test]
    fn delete() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            let id = docs.create("markdown", "/docs/a.md", None, None).unwrap();
            assert!(docs.delete(id).unwrap());
            assert_eq!(docs.get_by_id(id).unwrap(), None);
            assert!(!docs.delete(id).unwrap(), "second delete must report false");
        });
    }

    // (f1) list_paginated domain filter from metadata (array + scalar).
    #[test]
    fn list_paginated_domain_filter() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            seed_domain_fixtures(docs);

            for (domain, want) in [
                ("hr", 1),
                ("engineering", 1),
                ("product", 1),
                ("finance", 0),
            ] {
                let (page, total) = docs
                    .list_paginated(
                        0,
                        10,
                        &DocumentFilter {
                            domain: Some(domain.to_string()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_eq!(total, want, "domain {domain}");
                assert_eq!(page.len() as i64, want, "domain {domain}");
            }

            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        domain: Some("hr".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, "/docs/hr-policy.md");

            // No filter returns all.
            let (_, total) = docs
                .list_paginated(0, 10, &DocumentFilter::default())
                .unwrap();
            assert_eq!(total, 3);
        });
    }

    // (f2) list_paginated source_type + name filters.
    #[test]
    fn list_paginated_source_type_and_name_filters() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            seed_domain_fixtures(docs);

            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        source_type: Some("markdown".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 2);
            assert!(page.iter().all(|d| d.source_type == "markdown"));

            // Combined filters.
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        domain: Some("hr".into()),
                        source_type: Some("markdown".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, "/docs/hr-policy.md");

            // Name substring (case-insensitive).
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        name: Some("README".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, "/docs/README.md");
        });
    }

    // (f3) name filter: LIKE wildcards in user input match literally.
    #[test]
    fn name_filter_escapes_like_wildcards() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            docs.create("markdown", "/docs/report_2024.md", None, None)
                .unwrap();
            docs.create("markdown", "/docs/report 2024.md", None, None)
                .unwrap();
            docs.create("markdown", "/docs/100%_off.md", None, None)
                .unwrap();
            docs.create("markdown", r"/docs/a\b.md", None, None)
                .unwrap();

            // Literal underscore: must NOT match the space in "report 2024".
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        name: Some("report_2024".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1, "escaped underscore must be literal");
            assert_eq!(page[0].original_path, "/docs/report_2024.md");

            // Case-insensitive substring with a plain space.
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        name: Some("REPORT 2024".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, "/docs/report 2024.md");

            // Literal percent + underscore.
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        name: Some("100%_off".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, "/docs/100%_off.md");

            // Literal backslash.
            let (page, total) = docs
                .list_paginated(
                    0,
                    10,
                    &DocumentFilter {
                        name: Some("a\\b".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].original_path, r"/docs/a\b.md");
        });
    }

    // (f4) pagination windows over id order.
    #[test]
    fn list_paginated_pages_by_id() {
        let db = in_memory_db();
        let ids = seed_documents(&db, 5);
        with_docs(&db, |docs| {
            let (page, total) = docs
                .list_paginated(1, 2, &DocumentFilter::default())
                .unwrap();
            assert_eq!(total, 5);
            assert_eq!(page.len(), 2);
            // OFFSET 1 skips the first row (ids[0]) of the id-ordered list.
            assert_eq!(page[0].id, ids[1]);
            assert_eq!(page[1].id, ids[2]);

            // Offset past the end → empty page, full total.
            let (page, total) = docs
                .list_paginated(10, 2, &DocumentFilter::default())
                .unwrap();
            assert!(page.is_empty());
            assert_eq!(total, 5);
        });
    }

    // (g) unique_domains from metadata_json (json_each), both shapes.
    #[test]
    fn unique_domains() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            assert!(
                docs.unique_domains().unwrap().is_empty(),
                "empty db → empty vec"
            );
            seed_domain_fixtures(docs);
            assert_eq!(
                docs.unique_domains().unwrap(),
                vec!["engineering", "hr", "product"],
                "distinct values, sorted, from array and scalar domains"
            );
        });
    }

    // (з) count respects the same filters as list_paginated.
    #[test]
    fn count_respects_filters() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            seed_domain_fixtures(docs);
            assert_eq!(docs.count(&DocumentFilter::default()).unwrap(), 3);
            assert_eq!(
                docs.count(&DocumentFilter {
                    domain: Some("hr".into()),
                    ..Default::default()
                })
                .unwrap(),
                1
            );
            assert_eq!(
                docs.count(&DocumentFilter {
                    source_type: Some("markdown".into()),
                    ..Default::default()
                })
                .unwrap(),
                2
            );
            assert_eq!(
                docs.count(&DocumentFilter {
                    domain: Some("hr".into()),
                    source_type: Some("markdown".into()),
                    ..Default::default()
                })
                .unwrap(),
                1
            );
            assert_eq!(
                docs.count(&DocumentFilter {
                    name: Some("README".into()),
                    ..Default::default()
                })
                .unwrap(),
                1
            );
        });
    }

    // documents_by_type counts per source_type.
    #[test]
    fn documents_by_type() {
        let db = in_memory_db();
        with_docs(&db, |docs| {
            seed_domain_fixtures(docs);
            let by_type = docs.documents_by_type().unwrap();
            assert_eq!(by_type.get("markdown"), Some(&2));
            assert_eq!(by_type.get("json"), Some(&1));
            assert_eq!(by_type.len(), 2);
        });
    }

    // list: all documents, ordered by created_at DESC.
    #[test]
    fn list_orders_by_created_at_desc() {
        let db = in_memory_db();
        let ids = seed_documents(&db, 3);
        // Give each row a distinct created_at (CURRENT_TIMESTAMP has only
        // second resolution, so ties would make the order unobservable).
        for (i, id) in ids.iter().enumerate() {
            db.with_conn(|conn| {
                conn.execute(
                    "UPDATE documents SET created_at = ? WHERE id = ?",
                    params![format!("2026-01-{i:02} 00:00:00"), *id],
                )
                .unwrap()
            })
            .unwrap();
        }
        with_docs(&db, |docs| {
            let all = docs.list().unwrap();
            assert_eq!(
                all.iter().map(|d| d.id).collect::<Vec<_>>(),
                ids.iter().rev().copied().collect::<Vec<_>>(),
                "newest created_at first"
            );
        });
    }

    // The DAO works over a transaction: commit and rollback paths.
    #[test]
    fn create_inside_transaction() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", "/docs/tx.md", None, None)?;
            Ok(())
        })
        .expect("commit");

        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
                docs.create("markdown", "/docs/tx-rollback.md", None, None)?;
                // A genuine failure: original_path has a unique index
                // (/docs/tx.md was committed by the previous transaction).
                docs.create("markdown", "/docs/tx.md", None, None)?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        with_docs(&db, |docs| {
            assert_eq!(docs.count(&DocumentFilter::default()).unwrap(), 1);
            assert_eq!(docs.get_by_path("/docs/tx-rollback.md").unwrap(), None);
        });
    }
}
