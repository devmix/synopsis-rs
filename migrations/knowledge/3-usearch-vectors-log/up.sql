-- Migration 3-usearch-vectors-log: write-ahead log for the usearch ANN
-- engine (usearch-wal-persistence, task 2.1).
--
-- Tracks vector operations by chunk_id (flags: ADD=1, DEL=2, UPD=4) so the
-- in-RAM index can be reconciled/replayed after a restart. Vector payloads
-- are NOT stored here — they live in the chunks table (design decision:
-- WAL without vectors; the rebuild re-encodes chunk text).
--
-- New Rust operational table: the Go oracle has no equivalent (its vec0
-- store had no WAL), so no parity is required.
--
-- Idempotent by design (IF NOT EXISTS): re-running on a migrated database
-- is a no-op, and PRAGMA user_version (the sole schema-state authority,
-- design D6) advances to 3.

CREATE TABLE IF NOT EXISTS usearch_vectors_log (
    chunk_id    INTEGER PRIMARY KEY,
    flags       INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
