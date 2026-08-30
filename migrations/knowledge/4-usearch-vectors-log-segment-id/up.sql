-- Migration 4-usearch-vectors-log-segment-id: add segment_id to WAL
-- (usearch-wal-persistence, task 2.4).
--
-- Per-segment WAL tracking: each segment has its own WAL entries.
-- segment_id = 0 means "current/RAM operations".
-- segment_id = N means "operations for segment N at snapshot time".
--
-- This enables cumulative filtering:
-- - DISK_0: filter by WHERE segment_id <= 0
-- - DISK_1: filter by WHERE segment_id <= 1
-- - DISK_N: filter by WHERE segment_id <= N

-- Add segment_id column with default 0 (existing rows are RAM operations)
ALTER TABLE usearch_vectors_log ADD COLUMN segment_id INTEGER NOT NULL DEFAULT 0;

-- Drop old primary key constraint (chunk_id only)
-- and create new composite primary key (segment_id, chunk_id)
-- Note: SQLite doesn't support DROP CONSTRAINT, so we recreate the table

-- Create new table with correct schema
CREATE TABLE IF NOT EXISTS usearch_vectors_log_new (
    segment_id  INTEGER NOT NULL,
    chunk_id    INTEGER NOT NULL,
    flags       INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (segment_id, chunk_id)
);

-- Copy existing data
INSERT INTO usearch_vectors_log_new (segment_id, chunk_id, flags, created_at)
SELECT segment_id, chunk_id, flags, created_at FROM usearch_vectors_log;

-- Drop old table
DROP TABLE usearch_vectors_log;

-- Rename new table
ALTER TABLE usearch_vectors_log_new RENAME TO usearch_vectors_log;

-- Create indexes
CREATE INDEX IF NOT EXISTS idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
CREATE INDEX IF NOT EXISTS idx_usearch_vectors_log_segment ON usearch_vectors_log(segment_id);
