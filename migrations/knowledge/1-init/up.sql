-- Migration 1-init: the single squashed init migration for a fresh Rust
-- KNOWLEDGE database (the cache database has its own migration tree,
-- `migrations/cache`; task 1.9, storage-layout-restructure).
--
-- This is the ONE and ONLY knowledge migration (search-text-embedding task
-- 5.1 consolidated the former five forward-only migrations — 1-init through
-- 5-search-text — into this single init migration). It builds the full final
-- v5 schema shape in one step: the base tables/indexes/FTS triggers (with the
-- `search_text` re-point folded in), `document_jobs` + its due index, and
-- `usearch_vectors_log` (composite (segment_id, chunk_id) PK + flags/segment
-- indexes).
--
-- `PRAGMA user_version` is NOT set here: `rusqlite_migration::to_latest` sets
-- it to the migration count (one migration → user_version 1). The schema
-- SHAPE is the full v5 shape; only the internal counter is 1 (human decision
-- 2026-09-02, option B). Future migrations continue from 2.
--
-- Derived mechanically (2026-08-18) from the full schema dump of
-- fixtures/knowledge.db (sqlite_master + PRAGMA table_info; provenance in
-- fixtures/README.md), with legacy artifacts excluded by this explicit list:
--   _schema_migrations               -- legacy migration-tracking table. NOT created here:
--                                      PRAGMA user_version is the single source of truth
--                                      about schema state (design D6).
--   chunks_vec,                      -- legacy SQLite-vec0 vector store (384-dim) and its
--   chunks_vec_info,                 -- shadow tables. Rust rebuilds vectors from chunk
--   chunks_vec_chunks,               -- text; old vec0 data is never read or migrated.
--   chunks_vec_rowids,
--   chunks_vec_vector_chunks00
-- SQLite-internal objects are also absent by design (the engine creates them as needed):
-- sqlite_sequence and the FTS5 shadow tables of chunks_fts.
--
-- Semantic cross-check against the legacy 001–005 migrations (task 1.1
-- revision 3): 001 base schema; 002 unique idx_documents_original_path (its
-- row dedup is a data migration — nothing to replay on an empty DB); 003
-- drops documents.domain and idx_documents_domain (so documents has NO domain
-- column below, while entities/facts keep theirs); 004 extracted_at backfill
-- (data migration, no DDL); 005 app_kv (MOVED to the cache migration
-- `migrations/cache/1-init/up.sql` by task 1.9, storage-layout-restructure:
-- it is a cache, not knowledge).
-- The DDL below reflects the final state minus that table.
--
-- Deliberate deviation from the legacy v5 schema (human decision 2026-08-20,
-- db-module task 1.14 revision 2): fact_sources.document_id is INTEGER with
-- an FK to documents(id) ON DELETE CASCADE (like entity_sources.document_id),
-- not the legacy TEXT type. The legacy v5 schema is self-inconsistent (schema
-- TEXT vs an `int` field relying on type affinity). The fix lands in the
-- squashed init migration BEFORE any database was deployed, so the
-- forward-only rule is not violated in spirit.
--
-- Folded-in `search_text` (formerly the forward-only search-text migration,
-- search-text-embedding task 1.1, design D1/D2): both search legs (FTS5 lexical
-- + embedding semantic) operate on `search_text` = breadcrumb + "\n\n" + body for
-- sectioned chunks, so heading terms are searchable and embedded. `chunk_text`
-- stays the pure source slice (the byte-offset invariant `content[start..end] ==
-- chunk_text` is preserved); `search_text` defaults to it. Explicit, justified
-- deviation from the legacy v5 schema: the legacy implementation overwrote
-- chunk_text with the breadcrumb-prefixed text, which breaks the offset
-- semantics; Rust keeps both columns instead.
--
-- Folded-in `metadata_json` (chunk-metadata-persistence design D1): the
-- per-chunk metadata bag as raw JSON (nullable, no default — a chunk without
-- chunk-specific metadata stores NULL). Rust stores it as `Option<String>`,
-- parsed on demand (the `documents.metadata_json` pattern). Restores the
-- field from the original Rust design, dropped earlier to match the frozen
-- v5 shape; explicit, justified deviation from the legacy v5 schema (the
-- legacy `chunks` table has no metadata column).
--
-- Folded-in Rust operational tables (formerly the forward-only document-jobs and
-- usearch-WAL migrations):
--   document_jobs            -- persistent state machine for document operations
--                              (document-jobs-queue change, task 1.1). New Rust
--                              operational table; the legacy implementation has
--                              no equivalent queue (it ingests synchronously), so
--                              no parity is required.
--   usearch_vectors_log      -- write-ahead log for the usearch ANN engine
--                              (usearch-wal-persistence, tasks 2.1/2.4). Composite
--                              (segment_id, chunk_id) PK; segment_id = 0 means
--                              "current/RAM operations". Vector payloads are NOT
--                              stored here — they live in the chunks table (the
--                              rebuild re-encodes chunk text). New Rust operational
--                              table; the legacy implementation has no equivalent
--                              (its vec0 store had no WAL), so no parity is required.

CREATE TABLE documents (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    source_type    TEXT NOT NULL, -- 'json', 'markdown', 'steam', 'unstructured'
    original_path  TEXT NOT NULL,
    metadata_json  TEXT,
    content_hash   TEXT,
    created_at     DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at     DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id        INTEGER NOT NULL,
    chunk_text    TEXT NOT NULL,
    sequence_num  INTEGER NOT NULL,
    start_offset  INTEGER,
    end_offset    INTEGER,
    created_at    DATETIME DEFAULT CURRENT_TIMESTAMP,
    search_text   TEXT NOT NULL DEFAULT '',
    metadata_json TEXT, -- per-chunk metadata bag as raw JSON (nullable; no
                        -- default — an empty bag stores NULL).
                        -- chunk-metadata-persistence design D1.
    FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE TABLE entities (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    type        TEXT NOT NULL, -- 'employee', 'department', 'policy', 'system'
    name        TEXT NOT NULL,
    domain      TEXT NOT NULL DEFAULT '', -- domain this entity belongs to
    description TEXT,
    confidence  FLOAT,
    metadata_json TEXT,
    created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(type, name, domain)
);

CREATE TABLE chunk_entities (
    chunk_id  INTEGER NOT NULL,
    entity_id INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, entity_id),
    FOREIGN KEY (chunk_id) REFERENCES chunks(id) ON DELETE CASCADE,
    FOREIGN KEY (entity_id) REFERENCES entities(id) ON DELETE CASCADE
);

CREATE TABLE facts (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    subject_entity_id INTEGER REFERENCES entities(id),
    predicate         TEXT NOT NULL,
    object_entity_id  INTEGER REFERENCES entities(id),
    domain            TEXT NOT NULL DEFAULT '', -- domain this fact belongs to
    metadata          TEXT, -- JSON metadata (threshold_amount, condition, etc.)
    status            TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'pending', 'approved', 'rejected')),
    valid_from        DATE,
    valid_to          DATE,
    weight            INTEGER NOT NULL DEFAULT 1,
    created_at        DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at        DATETIME DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (subject_entity_id, object_entity_id, predicate)
);

CREATE TABLE fact_sources (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id      INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    document_id  INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    quote        TEXT, -- exact quote from source
    extracted_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE entity_sources (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id   INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(entity_id, document_id)
);

CREATE TABLE entity_links (
    subject_entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    target_entity_id  INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    relation_type     TEXT NOT NULL DEFAULT 'same_entity',
    method            TEXT NOT NULL,
    confidence        FLOAT NOT NULL DEFAULT 1.0,
    evidence          TEXT,
    PRIMARY KEY (subject_entity_id, target_entity_id, relation_type),
    CHECK (subject_entity_id != target_entity_id)
);

CREATE INDEX idx_entity_links_subject ON entity_links(subject_entity_id);
CREATE INDEX idx_entity_links_target ON entity_links(target_entity_id);
CREATE INDEX idx_chunks_doc_id ON chunks(doc_id);
CREATE INDEX idx_chunks_seq ON chunks(doc_id, sequence_num);
CREATE INDEX idx_entities_name ON entities(name);
CREATE INDEX idx_entities_type ON entities(type);
CREATE INDEX idx_entities_domain ON entities(domain);
CREATE INDEX idx_facts_status ON facts(status);
CREATE INDEX idx_facts_predicate ON facts(predicate);
CREATE INDEX idx_facts_subject ON facts(subject_entity_id);
CREATE INDEX idx_facts_object ON facts(object_entity_id);
CREATE INDEX idx_facts_valid ON facts(valid_from, valid_to);
CREATE INDEX idx_facts_domain ON facts(domain);
CREATE INDEX idx_fact_sources_fact ON fact_sources(fact_id);
CREATE INDEX idx_entity_sources_entity_id ON entity_sources(entity_id);
CREATE INDEX idx_entity_sources_document_id ON entity_sources(document_id);

-- FTS5 full-text search over chunks: external content (content='chunks',
-- content_rowid='id') kept in sync by the triggers below. Shadow tables
-- (chunks_fts_data/idx/docsize/config) are created by SQLite automatically.
-- The index is over `search_text` (the re-point folded into this init
-- migration; search-text-embedding design D2), NOT chunk_text.
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    search_text,
    content='chunks',
    content_rowid='id'
);

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

CREATE UNIQUE INDEX idx_documents_original_path ON documents(original_path);

-- document_jobs: persistent state machine for document operations (folded in
-- from the former document-jobs migration; document-jobs-queue task 1.1).
-- Producers (file watcher, startup reconcile, CLI) enqueue one row per
-- document path; a single background worker claims due rows and runs the
-- per-document pipeline. Statuses: pending -> processing -> done|error
-- (bounded retries with backoff; the index serves the due query).
CREATE TABLE document_jobs (
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

CREATE INDEX idx_document_jobs_due ON document_jobs(status, next_attempt_at);

-- usearch_vectors_log: write-ahead log for the usearch ANN engine (folded in
-- from the former usearch-WAL migrations; usearch-wal-persistence tasks
-- 2.1/2.4). Composite (segment_id, chunk_id) PK; segment_id = 0 means
-- "current/RAM operations". Vector payloads are NOT stored here — they live in
-- the chunks table (the rebuild re-encodes chunk text).
CREATE TABLE usearch_vectors_log (
    segment_id  INTEGER NOT NULL,
    chunk_id    INTEGER NOT NULL,
    flags       INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (segment_id, chunk_id)
);

CREATE INDEX idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
CREATE INDEX idx_usearch_vectors_log_segment ON usearch_vectors_log(segment_id);
