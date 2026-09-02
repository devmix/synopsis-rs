-- Migration 5-search-text: per-chunk search text (search-text-embedding,
-- task 1.1, design D1/D2).
--
-- Both search legs (FTS5 lexical + embedding semantic) operate on
-- `search_text` = breadcrumb + "\n\n" + body for sectioned chunks, so
-- heading terms are searchable and embedded. `chunk_text` stays the pure
-- source slice (the byte-offset invariant `content[start..end] ==
-- chunk_text` is preserved); `search_text` defaults to it.
--
-- Explicit, justified deviation from the oracle v5 schema: the Go oracle
-- overwrote chunk_text with the breadcrumb-prefixed text, which breaks the
-- offset semantics; Rust keeps both columns instead.

-- New column; existing rows backfilled from chunk_text (a no-op on a
-- from-scratch build; keeps the migration self-contained).
ALTER TABLE chunks ADD COLUMN search_text TEXT NOT NULL DEFAULT '';
UPDATE chunks SET search_text = chunk_text WHERE search_text = '';

-- Re-point the external-content FTS5 index to search_text. The triggers
-- reference the old column, so they are dropped and re-created atomically
-- in this migration.
DROP TABLE chunks_fts;
DROP TRIGGER chunks_fts_ai;
DROP TRIGGER chunks_fts_ad;
DROP TRIGGER chunks_fts_au;

CREATE VIRTUAL TABLE chunks_fts USING fts5(
    search_text,
    content='chunks',
    content_rowid='id'
);

-- Rebuild the index from the (backfilled) content table: a no-op on a
-- from-scratch build, but keeps a forward-only upgrade of a v4 database
-- correct (the index would otherwise stay empty for existing chunks).
INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild');

CREATE TRIGGER chunks_fts_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;

CREATE TRIGGER chunks_fts_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, search_text) VALUES('delete', old.id, old.search_text);
END;

CREATE TRIGGER chunks_fts_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, search_text) VALUES('delete', old.id, old.search_text);
    INSERT INTO chunks_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;

PRAGMA user_version = 5;
