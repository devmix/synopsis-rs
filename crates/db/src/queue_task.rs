//! Generic event task queue over the `queue_tasks` table (created by the
//! squashed init migration; event-queue-incremental-linking task 1.1,
//! ADR 0005).
//!
//! One queue for ALL background work: typed events (`doc:index`,
//! `doc:delete`, `entity:link`), one row per `(type, identity)` (the unique
//! index is the dedup/upsert key), and the same `pending -> processing ->
//! done | error` lifecycle as the former `document_jobs` (bounded retries
//! with the `30 * 2^(attempts-1)` backoff, manual re-queue via
//! [`QueueTaskDao::reset_retries`]). Producers (file watcher, startup
//! reconcile, the pipeline itself) enqueue via [`QueueTaskDao::enqueue`];
//! the single background worker claims due rows atomically via
//! [`QueueTaskDao::claim_due`] and dispatches by [`QueueTaskType`].
//!
//! `type` and `identity` are columns — queue mechanics (dedup, ordering,
//! filters, CLI display) never parse JSON. The `event` column holds only
//! the JSON residual payload ([`DocIndexPayload`], [`DocDeletePayload`],
//! [`EntityLinkPayload`]).
//!
//! Upsert semantics (design D4): `doc:*` events REPLACE the payload;
//! `entity:link` MERGES `entity_ids` (union, dedup) while the existing row
//! is `pending`/`processing`/`error`, and stores only the new ids when it
//! is `done`. Every upsert sets `next_attempt_at = now`, so a re-enqueued
//! event moves to the END of the claim order `(next_attempt_at, id)`
//! (design D5).

use std::str::FromStr;

use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::escape_like;

/// Shared `SELECT` list for the `queue_tasks` row queries (column order is
/// the contract of [`row_to_task`]).
const SELECT_TASK: &str = "SELECT id, type, identity, event, status, attempts, \
      max_attempts, last_error, next_attempt_at, created_at, updated_at FROM queue_tasks";

/// One queued event (one row of the `queue_tasks` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueTask {
    /// Row id (autoincrement; the claim-order tie-breaker).
    pub id: i64,
    /// Event type: `'doc:index'`, `'doc:delete'` or `'entity:link'`.
    pub task_type: String,
    /// Dedup key: the document path for `doc:*` events, the document id as
    /// a decimal string for `entity:link`.
    pub identity: String,
    /// The JSON residual payload (see the payload structs below).
    pub event: String,
    /// State: `'pending'`, `'processing'`, `'done'` or `'error'`.
    pub status: String,
    /// Number of failed attempts so far.
    pub attempts: i32,
    /// Failure cap: the task flips to `error` once `attempts` reaches it.
    pub max_attempts: i32,
    /// Last failure message, if any (kept across resets, like
    /// `document_jobs.last_error`).
    pub last_error: Option<String>,
    /// Unix seconds before which the task is not due (0 = due immediately).
    pub next_attempt_at: i64,
    /// Creation timestamp (Unix seconds).
    pub created_at: i64,
    /// Last-update timestamp (Unix seconds).
    pub updated_at: i64,
}

/// Event types of the `queue_tasks` queue (design D3: `{group}:{name}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueTaskType {
    /// (Re)index a document from its source path.
    DocIndex,
    /// Delete the document at its source path.
    DocDelete,
    /// Incrementally link the entities created/updated by one document.
    EntityLink,
}

impl QueueTaskType {
    /// The `queue_tasks.type` value of this event type.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DocIndex => "doc:index",
            Self::DocDelete => "doc:delete",
            Self::EntityLink => "entity:link",
        }
    }
}

impl AsRef<str> for QueueTaskType {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for QueueTaskType {
    type Err = UnknownTaskType;

    /// Parse a `queue_tasks.type` value; unknown values are rejected.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "doc:index" => Ok(Self::DocIndex),
            "doc:delete" => Ok(Self::DocDelete),
            "entity:link" => Ok(Self::EntityLink),
            other => Err(UnknownTaskType(other.to_owned())),
        }
    }
}

/// An unrecognized `queue_tasks.type` value (rejected by
/// [`QueueTaskType::from_str`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown queue task type: {0}")]
pub struct UnknownTaskType(String);

/// Failure classes of the queue-task DAO. SQLite-level failures stay
/// [`DbError`] (the executor's error type); this type adds the JSON payload
/// handling that is specific to the event queue (design D3).
#[derive(Debug, Error)]
pub enum QueueTaskError {
    /// A SQLite operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The JSON payload could not be serialized or parsed.
    #[error("queue task payload JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// JSON payload of a `doc:index` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocIndexPayload {
    /// The source directory the document belongs to.
    pub source_path: String,
    /// SHA-256 of the document content at enqueue time, if any.
    pub content_hash: Option<String>,
}

/// JSON payload of a `doc:delete` event (the delete needs only the path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocDeletePayload {
    /// The source path of the document to delete.
    pub source_path: String,
}

/// JSON payload of an `entity:link` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityLinkPayload {
    /// The entity ids created/updated by the document's index run(s).
    pub entity_ids: Vec<i64>,
}

/// State machine over the `queue_tasks` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`].
pub struct QueueTaskDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> QueueTaskDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Enqueue (upsert on `(type, identity)`) an event with the JSON
    /// `payload` due at `now` (Unix seconds).
    ///
    /// A new row is inserted as `pending` (`attempts=0`, `max_attempts=3`,
    /// `created_at=updated_at=next_attempt_at=now`). An existing row is
    /// reset to `pending` with `attempts=0` and `next_attempt_at=now` — a
    /// re-enqueued event moves to the END of the claim order — and its
    /// payload is updated per design D4: `doc:*` REPLACE, `entity:link`
    /// MERGE (`entity_ids` union, dedup) while the row is
    /// `pending`/`processing`/`error`, only the new ids when it is `done`.
    pub fn enqueue(
        &self,
        ty: QueueTaskType,
        identity: &str,
        payload: &impl Serialize,
        now: i64,
    ) -> Result<(), QueueTaskError> {
        let new_event = serde_json::to_string(payload)?;
        let existing = match self.exec.query_row(
            "SELECT status, event FROM queue_tasks WHERE type = ?1 AND identity = ?2",
            params![ty.as_str(), identity],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            Ok(value) => Some(value),
            Err(DbError::Sqlite {
                source: rusqlite::Error::QueryReturnedNoRows,
            }) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some((status, event)) = existing {
            // Existing row: reset it to pending due at `now` and update the
            // payload per design D4.
            let event = match ty {
                // doc:* — REPLACE: the file on disk is the source of truth.
                QueueTaskType::DocIndex | QueueTaskType::DocDelete => new_event,
                // entity:link — MERGE (union, dedup) or only the new ids
                // once the row is done (the old ids were already linked).
                QueueTaskType::EntityLink => {
                    let new_ids: EntityLinkPayload = serde_json::from_str(&new_event)?;
                    let old_ids: EntityLinkPayload = serde_json::from_str(&event)?;
                    let entity_ids = if status == "done" {
                        new_ids.entity_ids
                    } else {
                        let mut ids = old_ids.entity_ids;
                        for id in new_ids.entity_ids {
                            if !ids.contains(&id) {
                                ids.push(id);
                            }
                        }
                        ids
                    };
                    serde_json::to_string(&EntityLinkPayload { entity_ids })?
                }
            };
            self.exec.execute(
                "UPDATE queue_tasks SET status = 'pending', attempts = 0, \
                 next_attempt_at = ?3, updated_at = ?3, event = ?4 \
                 WHERE type = ?1 AND identity = ?2",
                params![ty.as_str(), identity, now, event],
            )?;
        } else {
            self.exec.execute(
                "INSERT INTO queue_tasks \
                 (type, identity, event, status, attempts, max_attempts, next_attempt_at, \
                  created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 'pending', 0, 3, ?4, ?4, ?4)",
                params![ty.as_str(), identity, new_event, now],
            )?;
        }
        Ok(())
    }

    /// Atomically claim up to `batch` due `pending` tasks
    /// (`next_attempt_at <= now`), ordered by `(next_attempt_at, id)`, flip
    /// them to `processing` and return exactly the rows claimed in this
    /// call (a task is claimed at most once per claim; stale `processing`
    /// rows are not re-returned — a startup timeout reset re-queues them).
    pub fn claim_due(&self, now: i64, batch: i64) -> Result<Vec<QueueTask>, QueueTaskError> {
        let mut claimed = self.exec.query(
            "UPDATE queue_tasks \
             SET status = 'processing', updated_at = ?3 \
             WHERE id IN ( \
                 SELECT id FROM queue_tasks \
                 WHERE status = 'pending' AND next_attempt_at <= ?1 \
                 ORDER BY next_attempt_at, id LIMIT ?2) \
             RETURNING id, type, identity, event, status, attempts, max_attempts, \
              last_error, next_attempt_at, created_at, updated_at",
            params![now, batch, now],
            row_to_task,
        )?;
        // `RETURNING` has no ordering guarantee: sort for a deterministic
        // claim order (due time, then id as the tie-breaker — design D5).
        claimed.sort_by(|a, b| {
            a.next_attempt_at
                .cmp(&b.next_attempt_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(claimed)
    }

    /// Mark task `id` as `done`, stamping `updated_at` to `now` (Unix
    /// seconds). Returns `true` if a row was updated, `false` if no task has
    /// `id`.
    pub fn mark_done(&self, id: i64, now: i64) -> Result<bool, QueueTaskError> {
        let changed = self.exec.execute(
            "UPDATE queue_tasks SET status = 'done', updated_at = ?2 \
             WHERE id = ?1",
            params![id, now],
        )?;
        Ok(changed > 0)
    }

    /// Record one failed attempt for task `id`: `attempts += 1`,
    /// `last_error = err`. While the new attempt count is below
    /// `max_attempts` the task goes back to `pending` with
    /// `next_attempt_at = now + 30 * 2^(attempts-1)` (the `document_jobs`
    /// schedule); at the cap it flips to `error` (no new backoff). Returns
    /// `true` if a row was updated, `false` if no task has `id`.
    pub fn mark_failed(&self, id: i64, err: &str, now: i64) -> Result<bool, QueueTaskError> {
        // `attempts + 1` is the NEW attempt count: SQLite evaluates the SET
        // expressions against the old row, so the backoff exponent
        // (new-1) is the OLD `attempts`. `30 << attempts` is the shift form
        // of `30 * 2^attempts` — the bundled SQLite build has no math
        // functions, so POWER() is unavailable.
        let changed = self.exec.execute(
            "UPDATE queue_tasks SET \
             attempts = attempts + 1, last_error = ?2, updated_at = ?3, \
             status = CASE WHEN attempts + 1 >= max_attempts THEN 'error' ELSE 'pending' END, \
             next_attempt_at = CASE WHEN attempts + 1 >= max_attempts \
                                    THEN next_attempt_at ELSE ?3 + (30 << attempts) END \
             WHERE id = ?1",
            params![id, err, now],
        )?;
        Ok(changed > 0)
    }

    /// Re-queue failed tasks (CLI `queue reset-retries`): `error ->
    /// pending`, `attempts = 0`, `next_attempt_at = now`. `source` filters
    /// `json_extract(event, '$.source_path') LIKE 'source%' ESCAPE '\'`
    /// (doc events only — `entity:link` payloads have no `source_path`);
    /// `identity` filters `identity = ?` exactly. Returns the number of
    /// rows reset.
    pub fn reset_retries(
        &self,
        source: Option<&str>,
        identity: Option<&str>,
    ) -> Result<usize, QueueTaskError> {
        let source_prefix: Option<String> = source.map(|s| format!("{}%", escape_like(s)));
        Ok(self.exec.execute(
            "UPDATE queue_tasks SET status = 'pending', attempts = 0, \
             next_attempt_at = strftime('%s','now'), updated_at = strftime('%s','now') \
             WHERE status = 'error' \
               AND (?1 IS NULL OR json_extract(event, '$.source_path') LIKE ?1 ESCAPE '\\') \
               AND (?2 IS NULL OR identity = ?2)",
            params![source_prefix, identity],
        )?)
    }

    /// Tasks ordered by `id`, optionally filtered by exact `status` and/or
    /// a `source` prefix (escaped with [`escape_like`], matched as a
    /// literal `LIKE 'prefix%'` on `json_extract(event,
    /// '$.source_path')` — doc events only).
    pub fn list(
        &self,
        source: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<QueueTask>, QueueTaskError> {
        let source_prefix: Option<String> = source.map(|s| format!("{}%", escape_like(s)));
        Ok(self.exec.query(
            &format!(
                "{SELECT_TASK} WHERE (?1 IS NULL OR status = ?1) \
                 AND (?2 IS NULL OR json_extract(event, '$.source_path') LIKE ?2 ESCAPE '\\') \
                 ORDER BY id"
            ),
            params![status, source_prefix],
            row_to_task,
        )?)
    }

    /// Queue size grouped by status, ordered by status (an empty vec when
    /// the queue is empty).
    pub fn status_counts(&self) -> Result<Vec<(String, i64)>, QueueTaskError> {
        Ok(self.exec.query(
            "SELECT status, COUNT(*) FROM queue_tasks GROUP BY status ORDER BY status",
            [],
            |row| {
                let status: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((status, count))
            },
        )?)
    }

    /// Delete the `entity:link` row of document `doc_id` (its identity is
    /// the doc id as a decimal string) — the cascade of a document
    /// deletion (design D11). Returns the number of rows deleted.
    pub fn delete_entity_link(&self, doc_id: i64) -> Result<usize, QueueTaskError> {
        Ok(self.exec.execute(
            "DELETE FROM queue_tasks WHERE type = 'entity:link' AND identity = ?1",
            [doc_id.to_string()],
        )?)
    }

    /// Delete a task row by `(type, identity)`. Used by the worker to remove
    /// a `doc:delete` row after successful processing (the row is not kept
    /// as `done` — it is a one-shot operation), and by the reconcile to
    /// cancel a stale `doc:delete` when the file is back on disk. Returns
    /// `true` if a row was deleted, `false` if no matching row existed.
    pub fn delete(&self, ty: QueueTaskType, identity: &str) -> Result<bool, QueueTaskError> {
        let changed = self.exec.execute(
            "DELETE FROM queue_tasks WHERE type = ?1 AND identity = ?2",
            params![ty.as_str(), identity],
        )?;
        Ok(changed > 0)
    }
}

/// Map a `queue_tasks` row (in [`SELECT_TASK`] order) to a [`QueueTask`].
fn row_to_task(row: &Row<'_>) -> rusqlite::Result<QueueTask> {
    Ok(QueueTask {
        id: row.get(0)?,
        task_type: row.get(1)?,
        identity: row.get(2)?,
        event: row.get(3)?,
        status: row.get(4)?,
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
    fn with_tasks<T>(db: &Db, f: impl FnOnce(&QueueTaskDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&QueueTaskDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Row count of `queue_tasks` (test helper).
    fn count_tasks(db: &Db) -> i64 {
        db.with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM queue_tasks", [], |r| r.get(0)))
            .unwrap()
            .unwrap()
    }

    /// A `doc:index` payload with the given source and no content hash.
    fn doc_payload(source: &str) -> DocIndexPayload {
        DocIndexPayload {
            source_path: source.to_owned(),
            content_hash: None,
        }
    }

    /// The `entity_ids` of an `entity:link` row.
    fn link_ids(task: &QueueTask) -> Vec<i64> {
        let payload: EntityLinkPayload = serde_json::from_str(&task.event).unwrap();
        payload.entity_ids
    }

    /// The single task with `identity` (test helper).
    fn get_task(tasks: &QueueTaskDao<'_>, identity: &str) -> QueueTask {
        tasks
            .list(None, None)
            .unwrap()
            .into_iter()
            .find(|t| t.identity == identity)
            .unwrap_or_else(|| panic!("no task with identity {identity}"))
    }

    /// Set a task's `next_attempt_at` directly (a state the DAO only writes
    /// through `mark_failed`/`reset_retries`).
    fn set_next_attempt_at(db: &Db, id: i64, next_attempt_at: i64) {
        let changed: usize = db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE queue_tasks SET next_attempt_at = ?1 WHERE id = ?2",
                    params![next_attempt_at, id],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(changed, 1, "the task must exist");
    }

    /// Set a task's `max_attempts` directly (the DAO never writes it).
    fn set_max_attempts(db: &Db, id: i64, max_attempts: i32) {
        let changed: usize = db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE queue_tasks SET max_attempts = ?1 WHERE id = ?2",
                    params![max_attempts, id],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(changed, 1, "the task must exist");
    }

    /// Drive task `id` to the `error` status: `max_attempts` recorded
    /// failures (direct `mark_failed` — no claims, so no other row is
    /// touched; the claim->failure cycle itself is covered by the backoff
    /// test below).
    fn fail_to_error(tasks: &QueueTaskDao<'_>, id: i64, max_attempts: i32, start_now: i64) {
        let mut now = start_now;
        for _ in 0..max_attempts {
            tasks.mark_failed(id, "boom", now).unwrap();
            now += 10_000;
        }
    }

    // The squashed init migration: queue_tasks + both indexes exist.
    #[test]
    fn fresh_db_has_queue_tasks_table_and_indexes() {
        let db = in_memory_db();
        let (table, due_index, identity_index): (i64, i64, i64) = db
            .with_conn(|conn| {
                let count = |name: &str, kind: &str| -> i64 {
                    conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                        params![kind, name],
                        |r| r.get(0),
                    )
                    .unwrap()
                };
                (
                    count("queue_tasks", "table"),
                    count("idx_queue_tasks_due", "index"),
                    count("idx_queue_tasks_identity", "index"),
                )
            })
            .unwrap();
        assert_eq!(table, 1, "queue_tasks table must exist");
        assert_eq!(due_index, 1, "idx_queue_tasks_due must exist");
        assert_eq!(identity_index, 1, "idx_queue_tasks_identity must exist");
    }

    // enqueue inserts a pending row due at `now` with the JSON payload.
    #[test]
    fn enqueue_inserts_pending_row() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/docs/a.md",
                    &DocIndexPayload {
                        source_path: "/docs".to_owned(),
                        content_hash: Some("h1".to_owned()),
                    },
                    100,
                )
                .unwrap();

            let row = tasks.list(None, None).unwrap();
            assert_eq!(row.len(), 1);
            let t = &row[0];
            assert_eq!(t.task_type, "doc:index");
            assert_eq!(t.identity, "/docs/a.md");
            assert_eq!(t.status, "pending");
            assert_eq!(t.attempts, 0);
            assert_eq!(t.max_attempts, 3);
            assert_eq!(t.next_attempt_at, 100);
            assert_eq!(t.created_at, 100);
            assert_eq!(t.updated_at, 100);
            assert!(t.last_error.is_none());
            let v: serde_json::Value = serde_json::from_str(&t.event).unwrap();
            assert_eq!(v["source_path"], "/docs");
            assert_eq!(v["content_hash"], "h1");
        });
    }

    // Re-enqueue of a claimed doc:index row: the payload is REPLACED (not
    // merged) and the row is reset to pending with next_attempt_at = now.
    #[test]
    fn enqueue_doc_index_replaces_payload_and_resets_row() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/a.md",
                    &DocIndexPayload {
                        source_path: "/s1".to_owned(),
                        content_hash: Some("h1".to_owned()),
                    },
                    100,
                )
                .unwrap();
            let claimed = tasks.claim_due(1_000, 10).unwrap();
            assert_eq!(claimed[0].status, "processing");

            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/a.md",
                    &DocIndexPayload {
                        source_path: "/s2".to_owned(),
                        content_hash: Some("h2".to_owned()),
                    },
                    5_000,
                )
                .unwrap();

            let t = get_task(tasks, "/a.md");
            assert_eq!(t.status, "pending", "re-enqueue must reset the status");
            assert_eq!(t.attempts, 0, "re-enqueue must reset attempts");
            assert_eq!(
                t.next_attempt_at, 5_000,
                "re-enqueue must set the due time to now"
            );
            let v: serde_json::Value = serde_json::from_str(&t.event).unwrap();
            assert_eq!(v["source_path"], "/s2", "the payload must be replaced");
            assert_eq!(v["content_hash"], "h2");
        });
    }

    // Re-enqueue of a pending entity:link row: entity_ids is the UNION
    // (dedup); the same merge applies to an error row.
    #[test]
    fn enqueue_entity_link_merges_pending_and_error() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "7",
                    &EntityLinkPayload {
                        entity_ids: vec![1, 2, 3],
                    },
                    100,
                )
                .unwrap();
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "7",
                    &EntityLinkPayload {
                        entity_ids: vec![3, 4],
                    },
                    200,
                )
                .unwrap();

            let t = get_task(tasks, "7");
            assert_eq!(t.status, "pending");
            assert_eq!(t.next_attempt_at, 200);
            assert_eq!(
                link_ids(&t),
                vec![1, 2, 3, 4],
                "pending merge: union, dedup"
            );

            // The same merge applies once the row is in error.
            let id = t.id;
            fail_to_error(tasks, id, 3, 10_000);
            assert_eq!(get_task(tasks, "7").status, "error");
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "7",
                    &EntityLinkPayload {
                        entity_ids: vec![4, 5],
                    },
                    90_000,
                )
                .unwrap();
            let t = get_task(tasks, "7");
            assert_eq!(t.status, "pending");
            assert_eq!(t.attempts, 0);
            assert_eq!(
                link_ids(&t),
                vec![1, 2, 3, 4, 5],
                "error merge: union, dedup"
            );
        });
    }

    // Re-enqueue of a DONE entity:link row: only the NEW ids are stored
    // (the old ones were already linked).
    #[test]
    fn enqueue_entity_link_done_stores_only_new_ids() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "9",
                    &EntityLinkPayload {
                        entity_ids: vec![1, 2],
                    },
                    100,
                )
                .unwrap();
            let claimed = tasks.claim_due(1_000, 10).unwrap();
            assert!(tasks.mark_done(claimed[0].id, 1_000).unwrap());

            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "9",
                    &EntityLinkPayload {
                        entity_ids: vec![2, 3],
                    },
                    2_000,
                )
                .unwrap();

            let t = get_task(tasks, "9");
            assert_eq!(t.status, "pending");
            assert_eq!(
                link_ids(&t),
                vec![2, 3],
                "done re-enqueue: only the new ids"
            );
        });
    }

    // Re-enqueue moves the row to the END: it is claimed after an older
    // pending row with a smaller next_attempt_at.
    #[test]
    fn reenqueue_moves_row_to_end_of_claim_order() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/b.md", &doc_payload("/s"), 200)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 300)
                .unwrap();

            let claimed = tasks.claim_due(10_000, 10).unwrap();
            assert_eq!(claimed.len(), 2);
            assert_eq!(
                claimed[0].identity, "/b.md",
                "the older row is claimed first"
            );
            assert_eq!(
                claimed[1].identity, "/a.md",
                "the re-enqueued row moves to the end"
            );
        });
    }

    // Claim order is (next_attempt_at, id): the smallest due time first,
    // insertion order (id) as the tie-breaker.
    #[test]
    fn claim_due_orders_by_due_time_then_id() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/b.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/c.md", &doc_payload("/s"), 50)
                .unwrap();

            let claimed = tasks.claim_due(10_000, 10).unwrap();
            assert_eq!(
                claimed
                    .iter()
                    .map(|t| t.identity.as_str())
                    .collect::<Vec<_>>(),
                vec!["/c.md", "/a.md", "/b.md"]
            );
        });
    }

    // claim_due returns only DUE pending rows, flips them to processing,
    // does not re-claim them, and honors the batch limit.
    #[test]
    fn claim_due_only_due_pending_rows() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/b.md", &doc_payload("/s"), 100)
                .unwrap();
            set_next_attempt_at(&db, get_task(tasks, "/b.md").id, 999_999_999);

            let claimed = tasks.claim_due(1_000, 10).unwrap();
            assert_eq!(claimed.len(), 1, "only the due pending row is claimed");
            assert_eq!(claimed[0].identity, "/a.md");
            assert_eq!(claimed[0].status, "processing");

            assert!(
                tasks.claim_due(1_000, 10).unwrap().is_empty(),
                "already-claimed rows must not be re-claimed"
            );
            let later = tasks.claim_due(999_999_999, 10).unwrap();
            assert_eq!(later.len(), 1);
            assert_eq!(later[0].identity, "/b.md");

            // Batch limit: at most `batch` rows per claim.
            for path in ["/c.md", "/d.md", "/e.md"] {
                tasks
                    .enqueue(QueueTaskType::DocIndex, path, &doc_payload("/s"), 100)
                    .unwrap();
            }
            let batched = tasks.claim_due(1_000, 2).unwrap();
            assert_eq!(batched.len(), 2, "at most `batch` rows per claim");
            assert_eq!(tasks.list(None, Some("pending")).unwrap().len(), 1);
        });
    }

    // Backoff sequence 30/60/120 (30 * 2^(attempts-1)), then `error` at the
    // attempt cap (max_attempts raised to 4 to observe all three backoffs).
    #[test]
    fn mark_failed_backoff_30_60_120_then_error() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 1_000)
                .unwrap();
            let id = get_task(tasks, "/a.md").id;
            set_max_attempts(&db, id, 4);

            for (failure, now, backoff, expected_attempts) in
                [(1, 1_000, 30, 1i32), (2, 2_000, 60, 2), (3, 3_000, 120, 3)]
            {
                let claimed = tasks.claim_due(now, 10).unwrap();
                assert_eq!(claimed.len(), 1, "cycle {failure} must claim the task");
                assert!(tasks.mark_failed(id, &format!("e{failure}"), now).unwrap());
                let t = get_task(tasks, "/a.md");
                assert_eq!(t.attempts, expected_attempts);
                assert_eq!(t.status, "pending", "below the cap the task stays pending");
                assert_eq!(
                    t.next_attempt_at,
                    now + backoff as i64,
                    "failure {failure}: next_attempt_at = now + {backoff}"
                );
                assert_eq!(
                    t.last_error.as_deref(),
                    Some(format!("e{failure}").as_str())
                );
            }

            // The 4th failure hits the cap: error, no new backoff.
            let claimed = tasks.claim_due(4_000, 10).unwrap();
            assert_eq!(claimed.len(), 1);
            assert!(tasks.mark_failed(id, "e4", 4_000).unwrap());
            let t = get_task(tasks, "/a.md");
            assert_eq!(t.attempts, 4);
            assert_eq!(t.status, "error");
            assert_eq!(
                t.next_attempt_at, 3_120,
                "the last (pre-cap) backoff is kept"
            );
            assert_eq!(t.last_error.as_deref(), Some("e4"));

            // An error row is not due anymore.
            assert!(tasks.claim_due(10_000_000, 10).unwrap().is_empty());
        });
    }

    // mark_done flips to done; both mark_done and mark_failed report false
    // for a missing id.
    #[test]
    fn mark_done_and_missing_ids() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 100)
                .unwrap();
            let id = get_task(tasks, "/a.md").id;

            assert!(tasks.mark_done(id, 100).unwrap());
            assert_eq!(get_task(tasks, "/a.md").status, "done");
            assert!(!tasks.mark_done(999, 100).unwrap(), "missing id: false");
            assert!(
                !tasks.mark_failed(999, "e", 0).unwrap(),
                "missing id: false"
            );
        });
    }

    // reset_retries flips error -> pending / attempts 0 / due ~now, by
    // identity, by source prefix, and unfiltered; non-error rows are
    // untouched.
    #[test]
    fn reset_retries_by_identity_and_source() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/src1/a.md",
                    &doc_payload("/src1"),
                    100,
                )
                .unwrap();
            fail_to_error(tasks, get_task(tasks, "/src1/a.md").id, 3, 1_000);
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/src2/b.md",
                    &doc_payload("/src2"),
                    100,
                )
                .unwrap();
            fail_to_error(tasks, get_task(tasks, "/src2/b.md").id, 3, 100_000);

            // By identity: only /src1/a.md is reset.
            assert_eq!(tasks.reset_retries(None, Some("/src1/a.md")).unwrap(), 1);
            let a = get_task(tasks, "/src1/a.md");
            assert_eq!(a.status, "pending");
            assert_eq!(a.attempts, 0);
            let wall: i64 = db
                .with_conn(|conn| {
                    conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| {
                        r.get(0)
                    })
                })
                .unwrap()
                .unwrap();
            assert!(
                a.next_attempt_at.abs_diff(wall) <= 5,
                "next_attempt_at must be ~now, got {} vs {}",
                a.next_attempt_at,
                wall
            );
            assert_eq!(get_task(tasks, "/src2/b.md").status, "error");

            // By source prefix: only /src2/b.md is reset.
            assert_eq!(tasks.reset_retries(Some("/src2"), None).unwrap(), 1);
            let b = get_task(tasks, "/src2/b.md");
            assert_eq!(b.status, "pending");
            assert_eq!(b.attempts, 0);
            assert_eq!(get_task(tasks, "/src1/a.md").status, "pending");

            // Unfiltered: both (re-driven to error) are reset at once.
            fail_to_error(tasks, a.id, 3, 2_000_000_000);
            fail_to_error(tasks, b.id, 3, 3_000_000_000);
            assert_eq!(tasks.reset_retries(None, None).unwrap(), 2);
            assert_eq!(get_task(tasks, "/src1/a.md").status, "pending");
            assert_eq!(get_task(tasks, "/src2/b.md").status, "pending");
        });
    }

    // list: no filter -> all (ordered by id); status and source-prefix
    // filters, combined; LIKE wildcards in the prefix match literally;
    // entity:link rows have no source_path and never match the source
    // filter.
    #[test]
    fn list_filters_by_status_and_source() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/src1/a.md",
                    &doc_payload("/src1"),
                    100,
                )
                .unwrap();
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/src1/b.md",
                    &doc_payload("/src1"),
                    100,
                )
                .unwrap();
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/src2/c.md",
                    &doc_payload("/src2"),
                    100,
                )
                .unwrap();
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "42",
                    &EntityLinkPayload {
                        entity_ids: vec![1],
                    },
                    100,
                )
                .unwrap();
            let a_id = get_task(tasks, "/src1/a.md").id;
            tasks.mark_done(a_id, 100).unwrap();

            let all = tasks.list(None, None).unwrap();
            assert_eq!(all.len(), 4);
            assert_eq!(
                all.iter().map(|t| t.identity.as_str()).collect::<Vec<_>>(),
                vec!["/src1/a.md", "/src1/b.md", "/src2/c.md", "42"],
                "ordered by id"
            );

            let pending = tasks.list(None, Some("pending")).unwrap();
            assert_eq!(pending.len(), 3);
            assert!(pending.iter().all(|t| t.status == "pending"));

            let src1 = tasks.list(Some("/src1"), None).unwrap();
            assert_eq!(src1.len(), 2);

            let both = tasks.list(Some("/src1"), Some("done")).unwrap();
            assert_eq!(both.len(), 1);
            assert_eq!(both[0].identity, "/src1/a.md");

            // The source filter is a PREFIX: "/src" matches both sources,
            // but never the entity:link row (no source_path in the payload).
            assert_eq!(tasks.list(Some("/src"), None).unwrap().len(), 3);

            // Unknown status / source -> empty.
            assert!(tasks.list(None, Some("bogus")).unwrap().is_empty());
            assert!(tasks.list(Some("/nope"), None).unwrap().is_empty());
        });
    }

    // status_counts: one entry per status present, ordered by status.
    #[test]
    fn status_counts_groups_by_status() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(QueueTaskType::DocIndex, "/a.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/b.md", &doc_payload("/s"), 100)
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "/c.md", &doc_payload("/s"), 100)
                .unwrap();

            tasks.mark_done(get_task(tasks, "/a.md").id, 100).unwrap();
            fail_to_error(tasks, get_task(tasks, "/b.md").id, 3, 1_000);

            assert_eq!(
                tasks.status_counts().unwrap(),
                vec![
                    ("done".to_owned(), 1),
                    ("error".to_owned(), 1),
                    ("pending".to_owned(), 1),
                ]
            );
        });
    }

    // delete_entity_link removes only the entity:link row of the document
    // (a doc:* row with the same identity string is untouched).
    #[test]
    fn delete_entity_link() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::EntityLink,
                    "7",
                    &EntityLinkPayload {
                        entity_ids: vec![1],
                    },
                    100,
                )
                .unwrap();
            tasks
                .enqueue(QueueTaskType::DocIndex, "7", &doc_payload("/s"), 100)
                .unwrap();

            assert_eq!(tasks.delete_entity_link(7).unwrap(), 1);
            assert_eq!(
                tasks.delete_entity_link(7).unwrap(),
                0,
                "second call: no row"
            );
            assert_eq!(
                tasks.delete_entity_link(8).unwrap(),
                0,
                "unknown doc id: no row"
            );

            let rows = tasks.list(None, None).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].task_type, "doc:index");
        });
    }

    // delete removes a task by (type, identity); a different type with the
    // same identity is untouched; a missing row returns false.
    #[test]
    fn delete_by_type_and_identity() {
        let db = in_memory_db();
        with_tasks(&db, |tasks| {
            tasks
                .enqueue(
                    QueueTaskType::DocDelete,
                    "/docs/a.md",
                    &DocDeletePayload {
                        source_path: "/docs".to_owned(),
                    },
                    100,
                )
                .unwrap();
            tasks
                .enqueue(
                    QueueTaskType::DocIndex,
                    "/docs/a.md",
                    &doc_payload("/docs"),
                    100,
                )
                .unwrap();

            assert!(
                tasks
                    .delete(QueueTaskType::DocDelete, "/docs/a.md")
                    .unwrap(),
                "the doc:delete row must be deleted"
            );
            assert!(
                !tasks
                    .delete(QueueTaskType::DocDelete, "/docs/a.md")
                    .unwrap(),
                "second call: no row"
            );
            assert!(
                !tasks
                    .delete(QueueTaskType::DocDelete, "/docs/missing.md")
                    .unwrap(),
                "unknown identity: no row"
            );

            let rows = tasks.list(None, None).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].task_type, "doc:index");
        });
    }

    // QueueTaskType: as_str/AsRef round-trip, from_str accepts the three
    // values and rejects unknown ones.
    #[test]
    fn queue_task_type_parsing() {
        assert_eq!(
            QueueTaskType::from_str("doc:index").unwrap(),
            QueueTaskType::DocIndex
        );
        assert_eq!(
            QueueTaskType::from_str("doc:delete").unwrap(),
            QueueTaskType::DocDelete
        );
        assert_eq!(
            QueueTaskType::from_str("entity:link").unwrap(),
            QueueTaskType::EntityLink
        );
        assert!(
            QueueTaskType::from_str("bogus").is_err(),
            "unknown value rejected"
        );
        assert!(QueueTaskType::from_str("").is_err(), "empty value rejected");
        assert!(
            QueueTaskType::from_str("doc:index ").is_err(),
            "whitespace is not trimmed"
        );
        assert_eq!(QueueTaskType::DocIndex.as_ref(), "doc:index");
        assert_eq!(QueueTaskType::DocDelete.as_ref(), "doc:delete");
        assert_eq!(QueueTaskType::EntityLink.as_ref(), "entity:link");
    }

    // The DAO works over a transaction: commit and rollback paths.
    #[test]
    fn enqueue_inside_transaction() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), QueueTaskError> {
            let tasks = QueueTaskDao::new(ConnectionOrTx::Transaction(&*tx));
            tasks.enqueue(
                QueueTaskType::DocIndex,
                "/docs/tx.md",
                &doc_payload("/docs"),
                100,
            )?;
            Ok(())
        })
        .expect("commit");
        assert_eq!(count_tasks(&db), 1);

        db.exec_tx(|tx| -> Result<(), QueueTaskError> {
            let tasks = QueueTaskDao::new(ConnectionOrTx::Transaction(&*tx));
            tasks.enqueue(
                QueueTaskType::DocIndex,
                "/docs/tx-rollback.md",
                &doc_payload("/docs"),
                100,
            )?;
            // A genuine failure after the partial write (NOT NULL violation).
            tx.execute("INSERT INTO queue_tasks (type) VALUES (NULL)", [])?;
            Ok(())
        })
        .expect_err("closure error must surface");
        assert_eq!(
            count_tasks(&db),
            1,
            "the rolled-back enqueue must not be visible"
        );
    }
}
