-- Migration 2-document-jobs: persistent state machine for document
-- operations (document-jobs-queue change, task 1.1).
--
-- Producers (file watcher, startup reconcile, CLI) enqueue one row per
-- document path; a single background worker claims due rows and runs the
-- per-document pipeline. Statuses: pending -> processing -> done|error
-- (bounded retries with backoff; the index serves the due query).
--
-- New Rust operational table: the Go oracle has no equivalent queue (it
-- ingests synchronously), so no parity is required.
--
-- Idempotent by design (IF NOT EXISTS): re-running on a migrated database
-- is a no-op, and PRAGMA user_version (the sole schema-state authority,
-- design D6) advances to 2.

CREATE TABLE IF NOT EXISTS document_jobs (
  path TEXT PRIMARY KEY,
  source_path TEXT NOT NULL,
  op TEXT NOT NULL DEFAULT 'index',
  status TEXT NOT NULL DEFAULT 'pending',
  content_hash TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL DEFAULT 3,
  last_error TEXT,
  next_attempt_at INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE INDEX IF NOT EXISTS idx_document_jobs_due ON document_jobs(status, next_attempt_at);
