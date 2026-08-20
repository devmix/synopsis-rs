-- Migration 1-init: squashed final v5 schema state for a fresh Rust database.
--
-- Derived mechanically (2026-08-18) from the full schema dump of
-- fixtures/knowledge.db (sqlite_master + PRAGMA table_info; provenance in
-- fixtures/README.md), with Go artifacts excluded by this explicit list:
--   _schema_migrations               -- Go migration-tracking table. NOT created here:
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
-- Semantic cross-check against ../synopsis/migrations/001..005.sql (read-only, files NOT
-- copied; task 1.1 revision 3): 001 base schema; 002 unique idx_documents_original_path
-- (its row dedup is a data migration — nothing to replay on an empty DB); 003 drops
-- documents.domain and idx_documents_domain (so documents has NO domain column below,
-- while entities/facts keep theirs); 004 extracted_at backfill (data migration, no DDL);
-- 005 app_kv. The dump reflects exactly that final state.
--
-- Deliberate deviation from the oracle (human decision 2026-08-20, db-module task 1.14
-- revision 2): fact_sources.document_id is INTEGER with an FK to documents(id) ON DELETE
-- CASCADE (like entity_sources.document_id), not the oracle's TEXT. The oracle is
-- self-inconsistent (schema TEXT vs Go `int` field relying on type affinity). The fix
-- lands in the squashed init migration BEFORE any database was deployed, so the
-- forward-only rule is not violated in spirit.

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
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id       INTEGER NOT NULL,
    chunk_text   TEXT NOT NULL,
    sequence_num INTEGER NOT NULL,
    start_offset INTEGER,
    end_offset   INTEGER,
    created_at   DATETIME DEFAULT CURRENT_TIMESTAMP,
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
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    chunk_text,
    content='chunks',
    content_rowid='id'
);

CREATE TRIGGER chunks_fts_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, chunk_text) VALUES (new.id, new.chunk_text);
END;

CREATE TRIGGER chunks_fts_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text) VALUES('delete', old.id, old.chunk_text);
END;

CREATE TRIGGER chunks_fts_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text) VALUES('delete', old.id, old.chunk_text);
    INSERT INTO chunks_fts(rowid, chunk_text) VALUES (new.id, new.chunk_text);
END;

CREATE UNIQUE INDEX idx_documents_original_path ON documents(original_path);

CREATE TABLE app_kv (
    key        TEXT PRIMARY KEY,
    value      TEXT,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
