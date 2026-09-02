//! Document job queue over the `document_jobs` table (migration
//! `2-document-jobs`).
//!
//! A persistent state machine for document operations: producers (file
//! watcher, startup reconcile, CLI) enqueue one row per document path via
//! [`DocumentJobDao::enqueue_index`] / [`DocumentJobDao::enqueue_delete`],
//! and a single background worker claims due rows atomically via
//! [`DocumentJobDao::claim_due`], runs the per-document pipeline and moves
//! each row through `pending -> processing -> done | error` (bounded
//! retries and backoff via [`DocumentJobDao::record_failure`], manual
//! re-queue via [`DocumentJobDao::reset_retries`]).
//!
//! New Rust operational construct: the Go oracle has no equivalent queue
//! (it ingests synchronously), so there is no oracle mapping and no parity
//! requirement.
//!
//! Statuses: `pending` (queued; due when `next_attempt_at <= now`),
//! `processing` (claimed by the worker), `done` (the index op succeeded),
//! `error` (retries exhausted; `last_error` holds the reason). Ops:
//! `index` (parse/NER/link the document) and `delete` (remove the
//! document; the job row itself is removed by
//! [`DocumentJobDao::mark_deleted_row`] once the delete succeeds).

use rusqlite::{Row, params};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::escape_like;

/// Shared `SELECT` list for the `document_jobs` row queries (column order is
/// the contract of [`row_to_job`]).
const SELECT_JOB: &str = "SELECT path, source_path, op, status, content_hash, attempts, \
      max_attempts, last_error, next_attempt_at, created_at, updated_at FROM document_jobs";

/// One queued document operation (one row of the `document_jobs` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentJob {
    /// Document path (primary key; at most one job per path).
    pub path: String,
    /// The source directory the document belongs to (empty for `delete`
    /// ops — the delete needs only the path).
    pub source_path: String,
    /// Operation: `'index'` or `'delete'`.
    pub op: String,
    /// State: `'pending'`, `'processing'`, `'done'` or `'error'`.
    pub status: String,
    /// SHA-256 of the document content at enqueue time, if any.
    pub content_hash: Option<String>,
    /// Number of failed attempts so far.
    pub attempts: i32,
    /// Failure cap: the job flips to `error` once `attempts` reaches it.
    pub max_attempts: i32,
    /// Last failure message, if any.
    pub last_error: Option<String>,
    /// Unix seconds before which the job is not due (0 = due immediately).
    pub next_attempt_at: i64,
    /// Creation timestamp (Unix seconds).
    pub created_at: i64,
    /// Last-update timestamp (Unix seconds).
    pub updated_at: i64,
}

/// State-machine operations over the `document_jobs` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the same
/// pattern as [`crate::DocumentDao`].
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DocumentJobDao, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let jobs = DocumentJobDao::new(ConnectionOrTx::Connection(conn));
///     jobs.enqueue_index("/docs/hr.md", "/docs", Some("sha256:abc"))?;
///     let due = jobs.claim_due(0, 10)?;
///     assert_eq!(due.len(), 1);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct DocumentJobDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> DocumentJobDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Queue `path` for (re)indexing as a `pending` job due immediately.
    ///
    /// Idempotent: the row is upserted by path, so a re-enqueue resets
    /// `attempts` to 0 and `status` to `pending` (one job per path).
    pub fn enqueue_index(
        &self,
        path: &str,
        source_path: &str,
        content_hash: Option<&str>,
    ) -> Result<(), DbError> {
        self.exec.execute(
            "INSERT OR REPLACE INTO document_jobs \
             (path, source_path, op, status, content_hash, attempts, max_attempts, \
              next_attempt_at, updated_at) \
             VALUES (?1, ?2, 'index', 'pending', ?3, 0, 3, 0, strftime('%s','now'))",
            params![path, source_path, content_hash],
        )?;
        Ok(())
    }

    /// Queue `path` for deletion as a `pending` job (`op = 'delete'`;
    /// `source_path` is stored empty and `content_hash` is `NULL` — the
    /// delete needs only the path).
    pub fn enqueue_delete(&self, path: &str) -> Result<(), DbError> {
        self.exec.execute(
            "INSERT OR REPLACE INTO document_jobs \
             (path, source_path, op, status, attempts, max_attempts, next_attempt_at, \
              updated_at) \
             VALUES (?1, '', 'delete', 'pending', 0, 3, 0, strftime('%s','now'))",
            [path],
        )?;
        Ok(())
    }

    /// Atomically claim up to `batch` due `pending` jobs
    /// (`next_attempt_at <= now`), oldest due first, flip them to
    /// `processing` and return exactly the rows claimed in this call.
    ///
    /// The claim is a single `UPDATE ... WHERE status='pending' ...
    /// RETURNING` statement (the task's UPDATE-then-SELECT variant would
    /// re-return rows already in `processing` from a crashed cycle — a
    /// double-processing hazard): a job is claimed at most once per claim,
    /// and stale `processing` rows are NOT re-returned here (a startup
    /// timeout reset re-queues them).
    pub fn claim_due(&self, now: i64, batch: i64) -> Result<Vec<DocumentJob>, DbError> {
        let mut claimed = self.exec.query(
            "UPDATE document_jobs \
             SET status = 'processing', updated_at = strftime('%s','now') \
             WHERE path IN ( \
                 SELECT path FROM document_jobs \
                 WHERE status = 'pending' AND next_attempt_at <= ?1 \
                 ORDER BY next_attempt_at LIMIT ?2) \
             RETURNING path, source_path, op, status, content_hash, attempts, max_attempts, \
              last_error, next_attempt_at, created_at, updated_at",
            params![now, batch],
            row_to_job,
        )?;
        // `RETURNING` has no ordering guarantee: sort for a deterministic
        // claim order (due time, then path as the tie-breaker).
        claimed.sort_by(|a, b| {
            a.next_attempt_at
                .cmp(&b.next_attempt_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(claimed)
    }

    /// Mark `path`'s job as `done` (the index op succeeded). Returns `true`
    /// if a row was updated, `false` if no job has `path`.
    pub fn mark_done(&self, path: &str) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE document_jobs SET status = 'done', updated_at = strftime('%s','now') \
             WHERE path = ?1",
            [path],
        )?;
        Ok(changed > 0)
    }

    /// Remove the job row for `path` (the delete op succeeded: the document
    /// is gone and its job is finished). Returns `true` if a row was
    /// deleted, `false` if no job has `path`.
    pub fn mark_deleted_row(&self, path: &str) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM document_jobs WHERE path = ?1", [path])?;
        Ok(changed > 0)
    }

    /// Record one failed attempt for `path`: `attempts += 1`,
    /// `last_error = err`. While the new attempt count is below
    /// `max_attempts` the job goes back to `pending` with
    /// `next_attempt_at = now + backoff_secs`; at the cap it flips to
    /// `error` (no new backoff is scheduled). Returns `true` if a row was
    /// updated, `false` if no job has `path`.
    pub fn record_failure(
        &self,
        path: &str,
        err: &str,
        now: i64,
        backoff_secs: i64,
        max_attempts: i32,
    ) -> Result<bool, DbError> {
        // `attempts + 1` is the NEW attempt count: SQLite evaluates the SET
        // expressions against the old row, where `attempts` is still the
        // pre-update value.
        let changed = self.exec.execute(
            "UPDATE document_jobs SET \
             attempts = attempts + 1, last_error = ?2, updated_at = strftime('%s','now'), \
             status = CASE WHEN attempts + 1 >= ?4 THEN 'error' ELSE 'pending' END, \
             next_attempt_at = CASE WHEN attempts + 1 >= ?4 THEN next_attempt_at ELSE ?3 + ?5 END \
             WHERE path = ?1",
            params![path, err, now, max_attempts, backoff_secs],
        )?;
        Ok(changed > 0)
    }

    /// Re-queue a failed job (CLI `index reset-retries`): `error ->
    /// pending`, `attempts = 0`, `next_attempt_at = now`. Returns `true` if
    /// a row was updated, `false` if no `error` job has `path`.
    pub fn reset_retries(&self, path: &str) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE document_jobs SET status = 'pending', attempts = 0, \
             next_attempt_at = strftime('%s','now'), updated_at = strftime('%s','now') \
             WHERE path = ?1 AND status = 'error'",
            [path],
        )?;
        Ok(changed > 0)
    }

    /// Retrieve a job by path, or `None` if absent.
    pub fn get_by_path(&self, path: &str) -> Result<Option<DocumentJob>, DbError> {
        let rows = self
            .exec
            .query(&format!("{SELECT_JOB} WHERE path = ?"), [path], row_to_job)?;
        Ok(rows.into_iter().next())
    }

    /// Jobs ordered by path, optionally filtered by exact `status` and/or a
    /// `source_path` prefix (the prefix is escaped with [`escape_like`] and
    /// matched as a literal `LIKE 'prefix%'` substring).
    pub fn list(
        &self,
        filter_status: Option<&str>,
        source: Option<&str>,
    ) -> Result<Vec<DocumentJob>, DbError> {
        let source_prefix: Option<String> = source.map(|s| format!("{}%", escape_like(s)));
        self.exec.query(
            &format!(
                "{SELECT_JOB} WHERE (?1 IS NULL OR status = ?1) \
                 AND (?2 IS NULL OR source_path LIKE ?2 ESCAPE '\\') ORDER BY path"
            ),
            params![filter_status, source_prefix],
            row_to_job,
        )
    }
}

/// Map a `document_jobs` row (in [`SELECT_JOB`] order) to a [`DocumentJob`].
fn row_to_job(row: &Row<'_>) -> rusqlite::Result<DocumentJob> {
    Ok(DocumentJob {
        path: row.get(0)?,
        source_path: row.get(1)?,
        op: row.get(2)?,
        status: row.get(3)?,
        content_hash: row.get(4)?,
        attempts: row.get(5)?,
        max_attempts: row.get(6)?,
        last_error: row.get(7)?,
        next_attempt_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::test_util::in_memory_db;

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_jobs<T>(db: &Db, f: impl FnOnce(&DocumentJobDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&DocumentJobDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Row count of `document_jobs` (test helper).
    fn count_jobs(db: &Db) -> i64 {
        db.with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM document_jobs", [], |r| r.get(0)))
            .unwrap()
            .unwrap()
    }

    /// Set a job's `next_attempt_at` directly (a state the DAO only writes
    /// through `record_failure`/`reset_retries`).
    fn set_due_at(db: &Db, path: &str, next_attempt_at: i64) {
        let changed: usize = db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE document_jobs SET next_attempt_at = ?1 WHERE path = ?2",
                    params![next_attempt_at, path],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(changed, 1, "the job must exist");
    }

    /// Drive `path` to the `error` status: `max_attempts` failed claims.
    /// Each failure is recorded at `now` with a 30 s backoff, and the clock
    /// advances 1_000 s (well past the backoff) before the next claim.
    fn fail_to_error(jobs: &DocumentJobDao<'_>, path: &str, max_attempts: i32) {
        let mut now = 1_000;
        for _ in 0..max_attempts {
            let claimed = jobs.claim_due(now, 10).unwrap();
            assert!(
                claimed.iter().any(|j| j.path == path),
                "attempt cycle must claim {path}"
            );
            jobs.record_failure(path, "parse error", now, 30, max_attempts)
                .unwrap();
            now += 1_000;
        }
    }

    // Migration 2-document-jobs: user_version is 5 (init + 2-document-jobs
    // + 3-usearch-vectors-log + 4-usearch-vectors-log-segment-id
    // + 5-search-text), the table and the due index exist.
    #[test]
    fn fresh_db_is_migrated_to_v2_with_document_jobs() {
        let db = in_memory_db();
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(
            user_version, 5,
            "migrations must advance user_version past 2-document-jobs"
        );

        let (table, index): (i64, i64) = db
            .with_conn(|conn| {
                let table = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'table' AND name = 'document_jobs'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let index = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'index' AND name = 'idx_document_jobs_due'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                (table, index)
            })
            .unwrap();
        assert_eq!(table, 1, "document_jobs table must exist");
        assert_eq!(index, 1, "idx_document_jobs_due must exist");
    }

    // enqueue_index twice → one row, attempts 0, latest hash wins.
    #[test]
    fn enqueue_index_is_idempotent() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/docs/a.md", "/docs", Some("h1"))
                .unwrap();
            jobs.enqueue_index("/docs/a.md", "/docs", Some("h2"))
                .unwrap();

            let rows = jobs.list(None, None).unwrap();
            assert_eq!(rows.len(), 1, "one job per path");
            let job = &rows[0];
            assert_eq!(job.path, "/docs/a.md");
            assert_eq!(job.source_path, "/docs");
            assert_eq!(job.op, "index");
            assert_eq!(job.status, "pending");
            assert_eq!(job.content_hash.as_deref(), Some("h2"));
            assert_eq!(job.attempts, 0, "re-enqueue must reset attempts");
            assert_eq!(job.max_attempts, 3);
            assert_eq!(job.next_attempt_at, 0, "re-enqueue must reset the due time");
            assert!(job.created_at > 0, "created_at default must be set");
            assert!(job.updated_at >= job.created_at);
        });
    }

    // enqueue_delete upserts the same path with op=delete, empty source.
    #[test]
    fn enqueue_delete_upserts() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/docs/a.md", "/docs", Some("h1"))
                .unwrap();
            jobs.enqueue_delete("/docs/a.md").unwrap();

            let job = jobs.get_by_path("/docs/a.md").unwrap().expect("must exist");
            assert_eq!(job.op, "delete");
            assert_eq!(job.source_path, "");
            assert_eq!(job.content_hash, None);
            assert_eq!(job.status, "pending");
            assert_eq!(job.attempts, 0);
        });
    }

    // claim_due returns only DUE pending rows, flips them to processing,
    // and a second call does not return already-claimed rows.
    #[test]
    fn claim_due_returns_only_due_pending_rows() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/a.md", "/src", None).unwrap();
            jobs.enqueue_index("/b.md", "/src", None).unwrap();
            // /b.md is not due yet.
            set_due_at(&db, "/b.md", 999_999_999);

            let claimed = jobs.claim_due(1_000, 10).unwrap();
            assert_eq!(claimed.len(), 1, "only the due pending row is claimed");
            assert_eq!(claimed[0].path, "/a.md");
            assert_eq!(claimed[0].status, "processing");

            // Second call: /a.md is 'processing' (not pending) and /b.md is
            // not due → nothing is returned.
            let again = jobs.claim_due(1_000, 10).unwrap();
            assert!(
                again.is_empty(),
                "already-claimed rows must not be re-claimed, got {again:?}"
            );

            // Once due, /b.md is claimed.
            let later = jobs.claim_due(1_000_000_000, 10).unwrap();
            assert_eq!(later.len(), 1);
            assert_eq!(later[0].path, "/b.md");
            assert_eq!(later[0].status, "processing");
        });
    }

    // claim_due honors the batch limit.
    #[test]
    fn claim_due_respects_batch_limit() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            for path in ["/a.md", "/b.md", "/c.md"] {
                jobs.enqueue_index(path, "/src", None).unwrap();
            }
            let claimed = jobs.claim_due(1_000, 2).unwrap();
            assert_eq!(claimed.len(), 2, "at most `batch` rows per claim");
            let remaining = jobs.list(Some("pending"), None).unwrap();
            assert_eq!(remaining.len(), 1, "the third job stays pending");
        });
    }

    // record_failure below the cap: attempts+1, pending, now+backoff.
    #[test]
    fn record_failure_increments_and_backs_off() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/a.md", "/src", None).unwrap();
            jobs.claim_due(1_000, 10).unwrap();

            assert!(
                jobs.record_failure("/a.md", "parse error", 2_000, 30, 3)
                    .unwrap(),
                "existing job must be updated"
            );
            let job = jobs.get_by_path("/a.md").unwrap().expect("must exist");
            assert_eq!(job.attempts, 1);
            assert_eq!(job.status, "pending");
            assert_eq!(
                job.next_attempt_at, 2_030,
                "next_attempt_at = now + backoff"
            );
            assert_eq!(job.last_error.as_deref(), Some("parse error"));
        });
    }

    // record_failure at the cap: error status, last_error, no new backoff.
    #[test]
    fn record_failure_at_cap_sets_error() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/a.md", "/src", None).unwrap();
            fail_to_error(jobs, "/a.md", 3);

            let job = jobs.get_by_path("/a.md").unwrap().expect("must exist");
            assert_eq!(job.attempts, 3);
            assert_eq!(job.status, "error");
            assert_eq!(job.last_error.as_deref(), Some("parse error"));
            // `fail_to_error` records the 2nd (pre-cap) failure at now=2_000
            // with a 30s backoff; the 3rd failure hits the cap and keeps it.
            assert_eq!(
                job.next_attempt_at, 2_030,
                "the last (pre-cap) backoff is kept, no new one scheduled"
            );

            // An error row is not due anymore: it stays put.
            assert!(jobs.claim_due(10_000_000, 10).unwrap().is_empty());
        });
    }

    // record_failure on a missing path reports false, not an error.
    #[test]
    fn record_failure_missing_path_is_false() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            assert!(!jobs.record_failure("/missing.md", "e", 0, 30, 3).unwrap());
        });
    }

    // reset_retries flips error → pending / attempts 0 / due now; non-error
    // rows are untouched.
    #[test]
    fn reset_retries_flips_error_to_pending() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/a.md", "/src", None).unwrap();
            fail_to_error(jobs, "/a.md", 3);

            assert!(jobs.reset_retries("/a.md").unwrap());
            let job = jobs.get_by_path("/a.md").unwrap().expect("must exist");
            assert_eq!(job.status, "pending");
            assert_eq!(job.attempts, 0);
            let now: i64 = db
                .with_conn(|conn| {
                    conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| {
                        r.get(0)
                    })
                })
                .unwrap()
                .unwrap();
            assert!(
                job.next_attempt_at.abs_diff(now) <= 5,
                "next_attempt_at must be ~now, got {} vs {}",
                job.next_attempt_at,
                now
            );
            // The reset job is claimable again.
            let claimed = jobs.claim_due(now + 1, 10).unwrap();
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].path, "/a.md");

            // A non-error row is not touched.
            jobs.enqueue_index("/b.md", "/src", None).unwrap();
            assert!(
                !jobs.reset_retries("/b.md").unwrap(),
                "a pending job must not be reset"
            );
            assert_eq!(
                jobs.get_by_path("/b.md").unwrap().unwrap().status,
                "pending"
            );
        });
    }

    // mark_done flips to done; mark_deleted_row removes the row; both report
    // false for missing paths.
    #[test]
    fn mark_done_and_mark_deleted_row() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/a.md", "/src", None).unwrap();
            jobs.enqueue_delete("/b.md").unwrap();

            assert!(jobs.mark_done("/a.md").unwrap());
            assert_eq!(jobs.get_by_path("/a.md").unwrap().unwrap().status, "done");
            assert!(!jobs.mark_done("/missing.md").unwrap());

            assert!(jobs.mark_deleted_row("/b.md").unwrap());
            assert_eq!(jobs.get_by_path("/b.md").unwrap(), None);
            assert!(!jobs.mark_deleted_row("/b.md").unwrap());
        });
    }

    // list: no filter → all; status and source-prefix filters, combined.
    #[test]
    fn list_filters_by_status_and_source() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/src1/a.md", "/src1", None).unwrap();
            jobs.enqueue_index("/src1/b.md", "/src1", None).unwrap();
            jobs.enqueue_index("/src2/c.md", "/src2", None).unwrap();
            jobs.mark_done("/src1/a.md").unwrap();

            assert_eq!(jobs.list(None, None).unwrap().len(), 3);

            let pending = jobs.list(Some("pending"), None).unwrap();
            assert_eq!(pending.len(), 2);
            assert!(pending.iter().all(|j| j.status == "pending"));

            let src1 = jobs.list(None, Some("/src1")).unwrap();
            assert_eq!(src1.len(), 2);
            assert!(src1.iter().all(|j| j.source_path == "/src1"));

            let both = jobs.list(Some("done"), Some("/src1")).unwrap();
            assert_eq!(both.len(), 1);
            assert_eq!(both[0].path, "/src1/a.md");

            // The source filter is a PREFIX: "/src" matches both sources.
            assert_eq!(jobs.list(None, Some("/src")).unwrap().len(), 3);

            // Unknown status / source → empty.
            assert!(jobs.list(Some("bogus"), None).unwrap().is_empty());
            assert!(jobs.list(None, Some("/nope")).unwrap().is_empty());
        });
    }

    // list: LIKE wildcards in the source prefix match literally.
    #[test]
    fn list_source_prefix_escapes_like_wildcards() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            jobs.enqueue_index("/docs/a_b.md", "/docs/a_b", None)
                .unwrap();
            jobs.enqueue_index("/docs/aXb.md", "/docs/aXb", None)
                .unwrap();

            // Literal underscore: must NOT match the "aXb" source.
            let rows = jobs.list(None, Some("/docs/a_b")).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].source_path, "/docs/a_b");
        });
    }

    // get_by_path: round-trip and absence.
    #[test]
    fn get_by_path() {
        let db = in_memory_db();
        with_jobs(&db, |jobs| {
            assert_eq!(jobs.get_by_path("/a.md").unwrap(), None);
            jobs.enqueue_index("/a.md", "/src", Some("h")).unwrap();
            let job = jobs.get_by_path("/a.md").unwrap().expect("must exist");
            assert_eq!(job.content_hash.as_deref(), Some("h"));
            assert_eq!(jobs.get_by_path("/other.md").unwrap(), None);
        });
    }

    // The DAO works over a transaction: commit and rollback paths.
    #[test]
    fn enqueue_inside_transaction() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let jobs = DocumentJobDao::new(ConnectionOrTx::Transaction(&*tx));
            jobs.enqueue_index("/docs/tx.md", "/docs", None)?;
            Ok(())
        })
        .expect("commit");
        assert_eq!(count_jobs(&db), 1);

        db.exec_tx(|tx| -> Result<(), DbError> {
            let jobs = DocumentJobDao::new(ConnectionOrTx::Transaction(&*tx));
            jobs.enqueue_index("/docs/tx-rollback.md", "/docs", None)?;
            // A genuine failure after the partial write (CHECK violation).
            tx.execute(
                "INSERT INTO facts (predicate, status) VALUES ('p', 'bogus')",
                [],
            )?;
            Ok(())
        })
        .expect_err("closure error must surface");
        assert_eq!(
            count_jobs(&db),
            1,
            "the rolled-back enqueue must not be visible"
        );
    }
}
