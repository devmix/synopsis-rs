//! Chunk storage and FTS5 full-text search over the `chunks` table.
//!
//! FTS5: `chunks_fts` is an external-content table (`content='chunks'`)
//! indexing `search_text` (the re-point folded into the squashed init
//! migration) and kept in sync by the `chunks_fts_ai/ad/au` triggers, so
//! plain CRUD automatically keeps the search index correct.
//!
//! **Design:**
//! - `search_text` column + FTS over it (search-text-embedding design
//!   D1/D2): `chunk_text` stays pure (the byte-offset invariant) and the
//!   breadcrumb-prefixed search text is stored separately — overwriting
//!   `chunk_text` with the prefixed text would break its offset semantics.
//!   [`ChunkDao::create`] defaults `search_text` to `chunk_text` (source-
//!   compatible with the pre-v5 call sites);
//!   [`ChunkDao::create_with_search_text`] stores a distinct search text;
//!   [`ChunkDao::update`] always takes it explicitly.
//! - `search_fts` returns one [`FtsHit`] (chunk + bm25 score) per row instead
//!   of a transient `Score` field on the chunk (Rust idiom);
//! - `search_fts` takes `domain: Option<&str>` instead of a
//!   `""`-means-absent string;
//! - the result is `ORDER BY bm25(chunks_fts)` — a deterministic bm25
//!   ranking (spike S1 records the intended ranked order 247, 30, 106);
//! - the domain filter checks `metadata_json IS NOT NULL AND
//!   json_valid(metadata_json)` in the outer `WHERE` before the `json_each`
//!   subquery, so a malformed-metadata row can never fail the query (same
//!   deviation as `document.rs`);
//! - `update`/`delete` return `bool` (`false` = no such id) instead of a
//!   "chunk not found" error (consistent with `DocumentDao`);
//! - `delete_by_ids` returns the number of rows deleted and batches the `IN`
//!   list in chunks of [`config::ID_BATCH_SIZE`] (design D9);
//! - the legacy vector-store operations are deliberately absent — vector
//!   search moves to the `vectors` change (design D7);
//! - `metadata_json` column (chunk-metadata-persistence design D1): the
//!   chunk's own metadata bag as raw JSON (`Option<String>`, parsed on
//!   demand — the `documents.metadata_json` pattern; `NULL` = no metadata).
//!   The v5 `chunks` table has no such column; this restores the field
//!   from the Rust design.
//!
//! Note: the task body's `Chunk` field list (token_count, updated_at) does
//! not match the frozen v5 schema, which has exactly the columns of
//! [`Chunk`]; the schema is the contract.

use config::ID_BATCH_SIZE;
use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Default page size applied when `search_fts` gets an out-of-range limit
/// (`limit <= 0 || limit > 100 → 20`).
const FTS_DEFAULT_LIMIT: i64 = 20;

/// Maximum page size for `search_fts`.
const FTS_MAX_LIMIT: i64 = 100;

/// Shared `SELECT` list for the `chunks` row queries (column order is the
/// contract of [`row_to_chunk`]).
const SELECT_CHUNK: &str = "SELECT id, doc_id, chunk_text, search_text, metadata_json, sequence_num, \
     start_offset, end_offset, \
     created_at \
     FROM chunks";

/// The single `search_fts` statement: FTS5 MATCH + `bm25()` ranking +
/// optional domain filter. The domain parameter is NULL-means-absent, so the
/// same statement serves both cases (DRY, as `document.rs` `FILTER_WHERE`);
/// the `json_valid` guard sits in the outer `WHERE` so a malformed
/// `metadata_json` can never fail the query.
const FTS_QUERY: &str = "SELECT c.id, c.doc_id, c.chunk_text, c.search_text, c.metadata_json, c.sequence_num, \
     c.start_offset, c.end_offset, \
     c.created_at, bm25(chunks_fts) \
     FROM chunks c \
     INNER JOIN chunks_fts ON chunks_fts.rowid = c.id \
     INNER JOIN documents d ON d.id = c.doc_id \
     WHERE chunks_fts MATCH ?1 \
       AND (?2 IS NULL OR (d.metadata_json IS NOT NULL AND json_valid(d.metadata_json) \
            AND EXISTS (SELECT 1 FROM json_each(d.metadata_json, '$.domain') WHERE json_each.value = ?2))) \
     ORDER BY bm25(chunks_fts) \
     LIMIT ?3";

/// A text fragment of a document (one row of the v5 `chunks` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Row id (autoincrement).
    pub id: i64,
    /// Owning document (FK to `documents.id`, `ON DELETE CASCADE`).
    pub doc_id: i64,
    /// The chunk text: a pure slice of the source document (the
    /// byte-offset invariant `content[start_offset..end_offset] ==
    /// chunk_text` holds).
    pub chunk_text: String,
    /// The text the FTS5 index and the embedding leg operate on:
    /// `breadcrumb + "\n\n" + body` for sectioned chunks, equal to
    /// `chunk_text` otherwise (search-text-embedding design D1).
    pub search_text: String,
    /// The chunk's own metadata bag as raw JSON, if any (per-chunk keys such
    /// as `section_title`/`breadcrumb`; `None` = no metadata). Parsed on
    /// demand by the reader (the `documents.metadata_json` pattern;
    /// chunk-metadata-persistence design D1).
    pub metadata_json: Option<String>,
    /// Position of the chunk within its document.
    pub sequence_num: i64,
    /// Start offset in the original text, if any.
    pub start_offset: Option<i64>,
    /// End offset in the original text, if any.
    pub end_offset: Option<i64>,
    /// Creation timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub created_at: String,
}

/// One FTS5 search hit: the matched chunk plus its `bm25()` rank score
/// (lower/more negative = better).
#[derive(Debug, Clone, PartialEq)]
pub struct FtsHit {
    /// The matched chunk.
    pub chunk: Chunk,
    /// The `bm25(chunks_fts)` score of the match.
    pub score: f64,
}

/// CRUD + FTS5 search over the `chunks` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`].
///
/// # Examples
///
/// ```no_run
/// # use db::{ChunkDao, ConnectionOrTx, Db, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
///     let id = chunks.create(1, "full text search works", 0, None, None)?;
///     assert_eq!(chunks.get_by_id(id)?.map(|c| c.id), Some(id));
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct ChunkDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> ChunkDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert a new chunk and return its generated id. The `chunks_fts_ai`
    /// trigger indexes the text automatically. `start_offset`/`end_offset`
    /// are stored as `NULL` when `None`, and `metadata_json` as `NULL`.
    ///
    /// `search_text` defaults to `chunk_text` (the init-migration backfill
    /// semantics); use [`Self::create_with_search_text`] to store a distinct
    /// search text (breadcrumb + body) and a per-chunk metadata bag.
    pub fn create(
        &self,
        doc_id: i64,
        chunk_text: &str,
        sequence_num: i64,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
    ) -> Result<i64, DbError> {
        self.create_with_search_text(
            doc_id,
            chunk_text,
            chunk_text,
            None,
            sequence_num,
            start_offset,
            end_offset,
        )
    }

    /// Insert a new chunk with an explicit `search_text` (the text the FTS5
    /// index and the embedding leg operate on; search-text-embedding design
    /// D1/D2) and, optionally, the chunk's own metadata bag as raw JSON
    /// (`metadata_json`, `NULL` when `None`; chunk-metadata-persistence
    /// design D1), and return its generated id.
    // The seven data parameters mirror the `chunks` write column set (house
    // style: plain parameters, as in the sibling DAOs).
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_search_text(
        &self,
        doc_id: i64,
        chunk_text: &str,
        search_text: &str,
        metadata_json: Option<&str>,
        sequence_num: i64,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
    ) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO chunks (doc_id, chunk_text, search_text, metadata_json, sequence_num, \
             start_offset, end_offset) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
            params![
                doc_id,
                chunk_text,
                search_text,
                metadata_json,
                sequence_num,
                start_offset,
                end_offset
            ],
            |row| row.get(0),
        )
    }

    /// Retrieve a chunk by id, or `None` if absent.
    pub fn get_by_id(&self, id: i64) -> Result<Option<Chunk>, DbError> {
        let rows = self
            .exec
            .query(&format!("{SELECT_CHUNK} WHERE id = ?"), [id], row_to_chunk)?;
        Ok(rows.into_iter().next())
    }

    /// All chunks of one document, ordered by `sequence_num`.
    pub fn list_by_doc_id(&self, doc_id: i64) -> Result<Vec<Chunk>, DbError> {
        self.exec.query(
            &format!("{SELECT_CHUNK} WHERE doc_id = ? ORDER BY sequence_num"),
            [doc_id],
            row_to_chunk,
        )
    }

    /// All chunks, ordered by id.
    pub fn list_all(&self) -> Result<Vec<Chunk>, DbError> {
        self.exec
            .query(&format!("{SELECT_CHUNK} ORDER BY id"), [], row_to_chunk)
    }

    /// Update text, position, offsets and the metadata bag of an existing
    /// chunk, refreshing the FTS index via the `chunks_fts_au` trigger.
    /// `search_text` is the new value of the indexed column (pass
    /// `chunk_text` when they coincide); `metadata_json` is the new raw-JSON
    /// bag (`NULL` when `None`). Returns `true` if a row was updated,
    /// `false` if no chunk has `id`.
    // Same seven data parameters as [`Self::create_with_search_text`].
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        id: i64,
        chunk_text: &str,
        search_text: &str,
        metadata_json: Option<&str>,
        sequence_num: i64,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
    ) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE chunks SET chunk_text = ?1, search_text = ?2, metadata_json = ?3, \
             sequence_num = ?4, start_offset = ?5, end_offset = ?6 \
             WHERE id = ?7",
            params![
                chunk_text,
                search_text,
                metadata_json,
                sequence_num,
                start_offset,
                end_offset,
                id
            ],
        )?;
        Ok(changed > 0)
    }

    /// Delete a chunk; the `chunks_fts_ad` trigger removes it from the index.
    /// Returns `true` if a row was deleted, `false` if no chunk has `id`.
    pub fn delete(&self, id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute("DELETE FROM chunks WHERE id = ?", [id])?;
        Ok(changed > 0)
    }

    /// Delete several chunks by id; ids that do not exist are ignored.
    /// Returns the number of rows deleted.
    ///
    /// The `IN` list is batched in chunks of [`config::ID_BATCH_SIZE`] to
    /// stay far below SQLite's 32766 bound on bound parameters (design D9).
    pub fn delete_by_ids(&self, ids: &[i64]) -> Result<usize, DbError> {
        let mut deleted = 0;
        for batch in ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            deleted += self.exec.execute(
                &format!("DELETE FROM chunks WHERE id IN ({placeholders})"),
                params_from_iter(batch.iter().copied()),
            )?;
        }
        Ok(deleted)
    }

    /// Number of chunks of one document.
    pub fn count_by_doc_id(&self, doc_id: i64) -> Result<i64, DbError> {
        self.exec.query_row(
            "SELECT COUNT(*) FROM chunks WHERE doc_id = ?",
            [doc_id],
            |row| row.get(0),
        )
    }

    /// Total number of chunks.
    pub fn count(&self) -> Result<i64, DbError> {
        self.exec
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
    }

    /// Full-text search over the FTS5 index, ranked by `bm25()` (lower score
    /// = better). When `domain` is `Some`, only chunks of documents whose
    /// `metadata_json` `$.domain` (string or array member) equals it are
    /// returned — the filter is applied before the `LIMIT`.
    ///
    /// `limit` is clamped to the page contract: values `<= 0` or
    /// `> 100` fall back to 20.
    ///
    /// `query` is an FTS5 MATCH expression (plain term, `"phrase"`,
    /// `a OR b`, …), passed through verbatim.
    pub fn search_fts(
        &self,
        query: &str,
        limit: i64,
        domain: Option<&str>,
    ) -> Result<Vec<FtsHit>, DbError> {
        self.exec.query(
            FTS_QUERY,
            params![query, domain, normalize_fts_limit(limit)],
            row_to_hit,
        )
    }
}

/// Limit contract: `limit <= 0 || limit > 100 → 20`.
fn normalize_fts_limit(limit: i64) -> i64 {
    if (1..=FTS_MAX_LIMIT).contains(&limit) {
        limit
    } else {
        FTS_DEFAULT_LIMIT
    }
}

/// Map a `chunks` row (in [`SELECT_CHUNK`] order) to a [`Chunk`].
fn row_to_chunk(row: &Row<'_>) -> rusqlite::Result<Chunk> {
    Ok(Chunk {
        id: row.get(0)?,
        doc_id: row.get(1)?,
        chunk_text: row.get(2)?,
        search_text: row.get(3)?,
        metadata_json: row.get(4)?,
        sequence_num: row.get(5)?,
        start_offset: row.get(6)?,
        end_offset: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// Map a [`FTS_QUERY`] row to an [`FtsHit`] (chunk columns + bm25 score).
fn row_to_hit(row: &Row<'_>) -> rusqlite::Result<FtsHit> {
    Ok(FtsHit {
        chunk: row_to_chunk(row)?,
        score: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::document::DocumentDao;
    use crate::test_util::in_memory_db;

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_chunks<T>(db: &Db, f: impl FnOnce(&ChunkDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&ChunkDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Create a document with `metadata_json` and return its id.
    fn seed_doc(db: &Db, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    // (a) create + get_by_id round-trip, with and without offsets.
    #[test]
    fn create_then_get_by_id_round_trip() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let id = chunks
                .create(doc, "the quick brown fox", 0, Some(0), Some(19))
                .unwrap();
            let chunk = chunks
                .get_by_id(id)
                .unwrap()
                .expect("created chunk must exist");
            assert_eq!(chunk.id, id);
            assert_eq!(chunk.doc_id, doc);
            assert_eq!(chunk.chunk_text, "the quick brown fox");
            // `create` defaults search_text to chunk_text (init-migration
            // backfill semantics).
            assert_eq!(chunk.search_text, "the quick brown fox");
            assert_eq!(chunk.sequence_num, 0);
            assert_eq!(chunk.start_offset, Some(0));
            assert_eq!(chunk.end_offset, Some(19));
            assert!(
                !chunk.created_at.is_empty(),
                "created_at default must be set"
            );

            let no_offsets = chunks.create(doc, "second chunk", 1, None, None).unwrap();
            let c2 = chunks.get_by_id(no_offsets).unwrap().unwrap();
            assert_eq!(c2.start_offset, None);
            assert_eq!(c2.end_offset, None);

            assert_eq!(chunks.get_by_id(999_999).unwrap(), None);
        });
    }

    // create_with_search_text stores a distinct search_text; row reads
    // return both texts.
    #[test]
    fn create_with_search_text_stores_both_texts() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let id = chunks
                .create_with_search_text(
                    doc,
                    "zebra stripes gallop",
                    "Atlas Guide\n\nzebra stripes gallop",
                    None,
                    0,
                    Some(0),
                    Some(20),
                )
                .unwrap();
            let chunk = chunks
                .get_by_id(id)
                .unwrap()
                .expect("created chunk must exist");
            assert_eq!(chunk.chunk_text, "zebra stripes gallop");
            assert_eq!(chunk.search_text, "Atlas Guide\n\nzebra stripes gallop");
            // list_by_doc_id / list_all return both as well.
            assert_eq!(
                chunks.list_by_doc_id(doc).unwrap()[0].search_text,
                "Atlas Guide\n\nzebra stripes gallop"
            );
            assert_eq!(
                chunks.list_all().unwrap()[0].search_text,
                "Atlas Guide\n\nzebra stripes gallop"
            );
        });
    }

    // (criterion 1) the squashed init migration gives `chunks` a nullable
    // `metadata_json` column; `user_version` stays 1.
    #[test]
    fn schema_has_nullable_metadata_json_column() {
        let db = in_memory_db();
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(user_version, 1, "user_version must stay 1");
        // PRAGMA table_info columns: (name, notnull).
        let columns: Vec<(String, i64)> = db
            .with_conn(|conn| {
                conn.prepare("PRAGMA table_info(chunks)")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(1)?, r.get(3)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            })
            .unwrap();
        assert!(
            columns
                .iter()
                .any(|(name, notnull)| name == "metadata_json" && *notnull == 0),
            "chunks must have a nullable metadata_json column: {columns:?}"
        );
    }

    // (criterion 3) metadata_json round-trip: a chunk created with a bag
    // returns it on every read path; a chunk created without one returns
    // None.
    #[test]
    fn metadata_json_round_trip() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let bag = r#"{"section_title":"Intro","breadcrumb":["Guide","Intro"]}"#;
            let with_meta = chunks
                .create_with_search_text(
                    doc,
                    "intro body",
                    "Guide > Intro\n\nintro body",
                    Some(bag),
                    0,
                    None,
                    None,
                )
                .unwrap();
            let without_meta = chunks
                .create_with_search_text(doc, "plain body", "plain body", None, 1, None, None)
                .unwrap();

            // get_by_id returns the stored bag (and None when absent).
            let c1 = chunks.get_by_id(with_meta).unwrap().expect("exists");
            assert_eq!(c1.metadata_json.as_deref(), Some(bag));
            let c2 = chunks.get_by_id(without_meta).unwrap().expect("exists");
            assert_eq!(c2.metadata_json, None, "absent metadata reads back as None");

            // list_by_doc_id / list_all carry it as well.
            let listed = chunks.list_by_doc_id(doc).unwrap();
            assert_eq!(
                listed
                    .iter()
                    .find(|c| c.id == with_meta)
                    .unwrap()
                    .metadata_json
                    .as_deref(),
                Some(bag)
            );
            assert_eq!(
                chunks
                    .list_all()
                    .unwrap()
                    .iter()
                    .find(|c| c.id == without_meta)
                    .unwrap()
                    .metadata_json,
                None
            );

            // search_fts carries it (criterion 2).
            let hits = chunks.search_fts("intro", 20, None).unwrap();
            assert_eq!(hits.len(), 1, "the sectioned chunk must match");
            assert_eq!(hits[0].chunk.id, with_meta);
            assert_eq!(hits[0].chunk.metadata_json.as_deref(), Some(bag));
        });
    }

    // (criterion 3, write half) update accepts metadata_json: sets a bag,
    // then clears it back to NULL.
    #[test]
    fn update_metadata_json_set_and_clear() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let id = chunks.create(doc, "old text", 0, None, None).unwrap();
            assert_eq!(
                chunks.get_by_id(id).unwrap().unwrap().metadata_json,
                None,
                "create stores NULL metadata"
            );
            let bag = r#"{"section_title":"Section"}"#;
            assert!(
                chunks
                    .update(id, "new text", "new search text", Some(bag), 1, None, None)
                    .unwrap()
            );
            assert_eq!(
                chunks
                    .get_by_id(id)
                    .unwrap()
                    .unwrap()
                    .metadata_json
                    .as_deref(),
                Some(bag)
            );
            assert!(
                chunks
                    .update(id, "new text", "new search text", None, 1, None, None)
                    .unwrap()
            );
            assert_eq!(
                chunks.get_by_id(id).unwrap().unwrap().metadata_json,
                None,
                "update with None must clear the bag back to NULL"
            );
        });
    }

    // (criterion 4) the FTS index is over search_text: a term present only
    // in search_text is found, a term only in chunk_text is not.
    #[test]
    fn fts_index_is_over_search_text() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            // "atlas" exists only in search_text; "quokka" only in
            // chunk_text.
            let id = chunks
                .create_with_search_text(
                    doc,
                    "quokka stripes gallop",
                    "Atlas Guide\n\nfox trot",
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();

            let hits = chunks.search_fts("atlas", 20, None).unwrap();
            assert_eq!(hits.len(), 1, "a search_text-only term must match");
            assert_eq!(hits[0].chunk.id, id);
            assert_eq!(hits[0].chunk.search_text, "Atlas Guide\n\nfox trot");
            assert_eq!(hits[0].chunk.chunk_text, "quokka stripes gallop");

            assert!(
                chunks.search_fts("quokka", 20, None).unwrap().is_empty(),
                "a chunk_text-only term must NOT match"
            );
        });
    }

    // (b) list_by_doc_id: per-document, ordered by sequence_num.
    #[test]
    fn list_by_doc_id_orders_by_sequence() {
        let db = in_memory_db();
        let a = seed_doc(&db, "/docs/a.md", None);
        let b = seed_doc(&db, "/docs/b.md", None);
        with_chunks(&db, |chunks| {
            // Interleaved insertion: the result must come out in sequence
            // order, not insertion order.
            let a0 = chunks.create(a, "a zero", 0, None, None).unwrap();
            let b0 = chunks.create(b, "b zero", 0, None, None).unwrap();
            let a1 = chunks.create(a, "a one", 1, None, None).unwrap();
            let b1 = chunks.create(b, "b one", 1, None, None).unwrap();
            let a2 = chunks.create(a, "a two", 2, None, None).unwrap();

            let list_a = chunks.list_by_doc_id(a).unwrap();
            assert_eq!(
                list_a.iter().map(|c| c.id).collect::<Vec<_>>(),
                vec![a0, a1, a2]
            );
            let list_b = chunks.list_by_doc_id(b).unwrap();
            assert_eq!(
                list_b.iter().map(|c| c.id).collect::<Vec<_>>(),
                vec![b0, b1]
            );
            assert!(chunks.list_by_doc_id(42).unwrap().is_empty());
        });
    }

    // update changes both texts, position and offsets; missing id → false.
    #[test]
    fn update() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let id = chunks.create(doc, "old text", 0, None, None).unwrap();
            assert!(
                chunks
                    .update(
                        id,
                        "new text",
                        "new search text",
                        None,
                        5,
                        Some(3),
                        Some(11)
                    )
                    .unwrap()
            );
            let c = chunks.get_by_id(id).unwrap().unwrap();
            assert_eq!(c.chunk_text, "new text");
            assert_eq!(c.search_text, "new search text");
            assert_eq!(c.metadata_json, None, "update with None must store NULL");
            assert_eq!(c.sequence_num, 5);
            assert_eq!(c.start_offset, Some(3));
            assert_eq!(c.end_offset, Some(11));
            assert!(
                !chunks
                    .update(999_999, "x", "x", None, 0, None, None)
                    .unwrap(),
                "missing id must report false, not error"
            );
        });
    }

    // delete removes the row; repeat → false; get → None.
    #[test]
    fn delete() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let id = chunks.create(doc, "gone soon", 0, None, None).unwrap();
            assert!(chunks.delete(id).unwrap());
            assert_eq!(chunks.get_by_id(id).unwrap(), None);
            assert!(
                !chunks.delete(id).unwrap(),
                "second delete must report false"
            );
        });
    }

    // count and count_by_doc_id.
    #[test]
    fn count_and_count_by_doc_id() {
        let db = in_memory_db();
        let a = seed_doc(&db, "/docs/a.md", None);
        let b = seed_doc(&db, "/docs/b.md", None);
        let c = seed_doc(&db, "/docs/c.md", None);
        with_chunks(&db, |chunks| {
            chunks.create(a, "one", 0, None, None).unwrap();
            chunks.create(a, "two", 1, None, None).unwrap();
            chunks.create(b, "three", 0, None, None).unwrap();
            assert_eq!(chunks.count_by_doc_id(a).unwrap(), 2);
            assert_eq!(chunks.count_by_doc_id(b).unwrap(), 1);
            assert_eq!(chunks.count_by_doc_id(c).unwrap(), 0);
            assert_eq!(chunks.count().unwrap(), 3);
        });
    }

    // list_all: everything, ordered by id.
    #[test]
    fn list_all_orders_by_id() {
        let db = in_memory_db();
        let a = seed_doc(&db, "/docs/a.md", None);
        let b = seed_doc(&db, "/docs/b.md", None);
        with_chunks(&db, |chunks| {
            let a0 = chunks.create(a, "a zero", 0, None, None).unwrap();
            let b0 = chunks.create(b, "b zero", 0, None, None).unwrap();
            let a1 = chunks.create(a, "a one", 1, None, None).unwrap();
            let all = chunks.list_all().unwrap();
            assert_eq!(
                all.iter().map(|c| c.id).collect::<Vec<_>>(),
                vec![a0, b0, a1]
            );
        });
    }

    // (c) delete_by_ids: removes rows (missing ids ignored) and the FTS
    //     trigger cleans the index.
    #[test]
    fn delete_by_ids_cleans_fts_index() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            let keep = chunks.create(doc, "keep the zebra", 0, None, None).unwrap();
            let drop1 = chunks
                .create(doc, "drop the quokka", 1, None, None)
                .unwrap();
            let drop2 = chunks
                .create(doc, "drop the wombat", 2, None, None)
                .unwrap();

            let deleted = chunks.delete_by_ids(&[drop1, drop2, 999_999]).unwrap();
            assert_eq!(deleted, 2);
            assert_eq!(chunks.get_by_id(drop1).unwrap(), None);
            assert_eq!(chunks.get_by_id(drop2).unwrap(), None);
            assert!(chunks.get_by_id(keep).unwrap().is_some());

            // Deleted text is gone from the FTS index.
            assert!(chunks.search_fts("quokka", 20, None).unwrap().is_empty());
            assert!(chunks.search_fts("wombat", 20, None).unwrap().is_empty());
            assert_eq!(chunks.search_fts("zebra", 20, None).unwrap().len(), 1);
        });
    }

    // D9 batch boundary: 600 ids = 2 batches of ≤ 500.
    #[test]
    fn delete_by_ids_batches_over_500() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        let mut ids = Vec::with_capacity(600);
        db.exec_tx(|tx| -> Result<(), DbError> {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..600 {
                ids.push(chunks.create(doc, &format!("batch row {i}"), i as i64, None, None)?);
            }
            Ok(())
        })
        .expect("seed transaction commits");
        with_chunks(&db, |chunks| {
            let requested: Vec<i64> = ids.iter().rev().copied().collect();
            assert_eq!(chunks.delete_by_ids(&requested).unwrap(), 600);
            assert_eq!(chunks.count().unwrap(), 0);
        });
    }

    // search_fts: ranking order, limit normalization, phrase, no-match.
    #[test]
    fn search_fts_basic_ranking_and_limit_normalization() {
        let db = in_memory_db();
        let a = seed_doc(&db, "/docs/a.md", None);
        let b = seed_doc(&db, "/docs/b.md", None);
        with_chunks(&db, |chunks| {
            chunks.create(a, "alpha beta one", 0, None, None).unwrap();
            chunks.create(a, "alpha beta two", 1, None, None).unwrap();
            chunks.create(a, "gamma three", 2, None, None).unwrap();
            chunks.create(b, "alpha delta", 0, None, None).unwrap();

            let hits = chunks.search_fts("alpha", 20, None).unwrap();
            assert_eq!(hits.len(), 3);
            // bm25 rank: scores non-decreasing (more negative = better).
            for (cur, next) in hits.iter().zip(hits.iter().skip(1)) {
                assert!(cur.score <= next.score, "results must be bm25-ranked");
            }

            // Limit normalization: 0 and 1000 → default 20.
            assert_eq!(chunks.search_fts("alpha", 0, None).unwrap().len(), 3);
            assert_eq!(chunks.search_fts("alpha", 1000, None).unwrap().len(), 3);
            // In-range limit is honored and preserves rank order.
            let top2 = chunks.search_fts("alpha", 2, None).unwrap();
            assert_eq!(top2.len(), 2);
            assert_eq!(top2[0].chunk.id, hits[0].chunk.id);
            assert_eq!(top2[1].chunk.id, hits[1].chunk.id);

            // Phrase query.
            assert_eq!(
                chunks.search_fts("\"alpha beta\"", 20, None).unwrap().len(),
                2
            );
            // No match.
            assert!(chunks.search_fts("nomatch", 20, None).unwrap().is_empty());
        });
    }

    // (e) domain filter — the fixture covers scalar and array domains.
    #[test]
    fn search_fts_domain_filter() {
        let db = in_memory_db();
        let hr = seed_doc(&db, "/docs/hr.md", Some(r#"{"domain":"hr"}"#));
        let eng = seed_doc(&db, "/docs/eng.md", Some(r#"{"domain":"engineering"}"#));
        let multi = seed_doc(
            &db,
            "/docs/multi.md",
            Some(r#"{"domain":["legal","policy"]}"#),
        );
        with_chunks(&db, |chunks| {
            for i in 0..2 {
                chunks
                    .create(
                        hr,
                        &format!("hr policy document chunk {i}"),
                        i as i64,
                        None,
                        None,
                    )
                    .unwrap();
            }
            for i in 0..3 {
                chunks
                    .create(
                        eng,
                        &format!("engineering spec chunk {i}"),
                        i as i64,
                        None,
                        None,
                    )
                    .unwrap();
            }
            chunks
                .create(
                    multi,
                    "multi domain legal and policy chunk content",
                    0,
                    None,
                    None,
                )
                .unwrap();

            for (domain, want) in [("hr", 2), ("engineering", 3), ("policy", 1), ("product", 0)] {
                let hits = chunks.search_fts("chunk", 20, Some(domain)).unwrap();
                assert_eq!(hits.len(), want, "domain {domain}");
            }
            // No domain: all 6.
            assert_eq!(chunks.search_fts("chunk", 20, None).unwrap().len(), 6);
        });
    }

    // A malformed metadata_json must never fail the domain-filtered query.
    #[test]
    fn search_fts_domain_filter_ignores_malformed_metadata() {
        let db = in_memory_db();
        let ok = seed_doc(&db, "/docs/ok.md", Some(r#"{"domain":"hr"}"#));
        let bad = seed_doc(&db, "/docs/bad.md", Some("{not valid json"));
        with_chunks(&db, |chunks| {
            chunks
                .create(ok, "zebra in a valid doc", 0, None, None)
                .unwrap();
            chunks
                .create(bad, "zebra in a broken doc", 0, None, None)
                .unwrap();

            let hits = chunks.search_fts("zebra", 20, Some("hr")).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].chunk.doc_id, ok);

            // Without the domain filter the broken row still matches on text.
            assert_eq!(chunks.search_fts("zebra", 20, None).unwrap().len(), 2);
        });
    }

    // (f) create/update/delete keep the FTS index in sync (ai/ad/au triggers).
    #[test]
    fn fts_index_stays_in_sync_with_crud() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        with_chunks(&db, |chunks| {
            // ai: insert → searchable (search_text defaults to chunk_text).
            let id = chunks.create(doc, "zebra stripes", 0, None, None).unwrap();
            assert_eq!(chunks.search_fts("zebra", 20, None).unwrap().len(), 1);

            // au: update → old search_text drops out, new one enters the
            // index (chunk_text may stay untouched).
            assert!(
                chunks
                    .update(id, "zebra stripes", "zebra and quokka", None, 0, None, None)
                    .unwrap()
            );
            assert_eq!(chunks.search_fts("quokka", 20, None).unwrap().len(), 1);
            assert!(chunks.search_fts("stripes", 20, None).unwrap().is_empty());
            assert_eq!(chunks.search_fts("zebra", 20, None).unwrap().len(), 1);

            // ad: delete → gone from the index.
            assert!(chunks.delete(id).unwrap());
            assert!(chunks.search_fts("quokka", 20, None).unwrap().is_empty());
            assert_eq!(chunks.count().unwrap(), 0);
        });
    }

    // The DAO works over a transaction: commit and rollback paths.
    #[test]
    fn create_inside_transaction() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/docs/a.md", None);
        db.exec_tx(|tx| -> Result<(), DbError> {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc, "committed chunk", 0, None, None)?;
            Ok(())
        })
        .expect("commit");

        db.exec_tx(|tx| -> Result<(), DbError> {
            let chunks = ChunkDao::new(ConnectionOrTx::Transaction(&*tx));
            chunks.create(doc, "rolled-back chunk", 1, None, None)?;
            // A genuine SQL failure after a partial write (CHECK violation).
            tx.execute(
                "INSERT INTO facts (predicate, status) VALUES ('p', 'bogus')",
                [],
            )?;
            Ok(())
        })
        .expect_err("closure error must surface");

        with_chunks(&db, |chunks| {
            assert_eq!(chunks.count().unwrap(), 1);
            assert!(chunks.search_fts("rolled", 20, None).unwrap().is_empty());
        });
    }
}
