# data-schema Specification

## Purpose

The SQLite data schema and migration rules. Fixes that the v5 schema shape (5 migrations) is the structural contract of data continuity: the Rust binary builds its DB from scratch, a pre-existing `knowledge.db` is not opened and not migrated; the schema shape is preserved.
## Requirements
### Requirement: Compatibility with the v5 schema

The Rust binary builds knowledge.db from scratch (v5 schema after migrations 001–005). The tables `documents`, `chunks`, `entities`, `chunk_entities`, `facts`, `fact_sources`, `entity_sources`, `entity_links`, the FTS5 table `chunks_fts`, and the indexes are present and used identically: the same queries yield the same results. The `app_kv` table is **NOT** in the knowledge DB: it lives in the global cache DB (`<workspace_dir>/db/cache/cache.db`, created by `migrations/cache/1-init/up.sql`) alongside the LLM response caches (`llm_ner_cache`, `llm_linker_cache`).

#### Scenario: Fresh-DB startup
- **WHEN** the Rust server starts with a fresh knowledge.db built from scratch (sync finished)
- **THEN** the migrations do not rewrite data; catalog_overview returns counters computed from the same DB's data

#### Scenario: Read-query repeatability
- **WHEN** the same read queries are sent to the Rust binary (the same DB copy) through the MCP tools
- **THEN** the results match the recorded fixtures (except the ANN fields, see the recall gates)

#### Scenario: app_kv lives in the cache DB
- **WHEN** the knowledge DB and the cache DB are inspected after a fresh build
- **THEN** the knowledge DB has no `app_kv` table, and the cache DB (`migrations/cache/1-init/up.sql`) has `app_kv`

### Requirement: Migration discipline

Migrations are applied at startup in numbered order and are idempotent (IF NOT EXISTS). The shipped files 001–005 are never edited; schema changes in the Rust version are added only as new files continuing the numbering (006+), which are applied correctly to a v5 DB.

#### Scenario: New migration
- **WHEN** migration 006 is added to the Rust repo and the binary starts on a v5 DB
- **THEN** 006 is applied once and the data is not corrupted; a subsequent startup is a no-op

### Requirement: facts status lifecycle

The `facts` table carries a `status` column with the values `draft`, `pending`, `approved`, `rejected` (CHECK constraint). Only `approved` facts participate in read expansions (search expansion, dossiers).

#### Scenario: CHECK constraint
- **WHEN** writing a fact with status 'weird' is attempted
- **THEN** the write is rejected by the constraint

### Requirement: Vector storage and rebuild

Embedding vectors are NOT carried over from the old vec0 table `chunks_vec` into the new storage. The old table in a pre-existing file is ignored (or disabled) without errors. The vector index of the Rust version is an external HNSW store, rebuilt from the chunk text in `chunks` on rebuild; the index dimensionality matches the embedding model from the configuration.

#### Scenario: Vector rebuild
- **WHEN** the Rust binary runs a vector rebuild on a v5 DB (no vec0 data)
- **THEN** the index is built from the chunk text; semantic search returns recall@10 ≥ 0.95 against a brute-force ground truth on the same embedding model

### Requirement: document_jobs table (indexing queue)

The Rust binary keeps a queue of document operations in the `document_jobs` table (knowledge DB), created by the consolidated init migration `migrations/knowledge/1-init/up.sql` (the table was folded into the init migration at squash time, design D6; a separate forward-only migration `2-document-jobs` does not exist). The table is a single state machine for the watcher, the startup scan, and the background worker. Columns: `path TEXT PRIMARY KEY`, `source_path TEXT NOT NULL`, `op TEXT NOT NULL DEFAULT 'index'` (`index` | `delete`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `content_hash TEXT`, `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER`, `updated_at INTEGER`. Index `idx_document_jobs_due (status, next_attempt_at)` for due queries. The migration is idempotent (`IF NOT EXISTS`); a subsequent startup is a no-op.

#### Scenario: Migration application
- **WHEN** the binary starts on a knowledge.db without the `document_jobs` table
- **THEN** the init migration `1-init` creates the table and the index once; a subsequent startup does not change the schema

#### Scenario: Job state
- **WHEN** a document has not been indexed after `max_attempts` attempts
- **THEN** the row has `status='error'`, `attempts=max_attempts`, and `last_error` is populated; `index reset-retries` moves it to `pending` with `attempts=0`

### Requirement: search_text column (explicit v5 deviation)
The `chunks` table carries a `search_text TEXT NOT NULL` column (default = `chunk_text`) holding the search-oriented text: for Markdown chunks the heading breadcrumb (multi-line heading path) followed by the chunk body, or the body alone when the chunk has no breadcrumb. The FTS5 `chunks_fts` index is built over `search_text` (not `chunk_text`), and its `ai/ad/au` triggers reference `search_text`. This is an explicit, justified deviation from the v5 shape: the Rust database is always built from scratch (no pre-existing `knowledge.db` is opened or migrated), and the deviation improves RAG retrieval quality by giving both search legs the section context. The invariant-preserving `chunk_text` column and the byte offsets are unchanged.

#### Scenario: Fresh build includes search_text
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a `search_text` column, the `chunks_fts` index indexes `search_text`, and `PRAGMA user_version` = 1

#### Scenario: FTS index tracks search_text
- **WHEN** a chunk is inserted, updated, or deleted
- **THEN** the `chunks_fts` index is kept in sync via the `ai`/`au`/`ad` triggers over `search_text`

#### Scenario: chunk_text invariant preserved
- **WHEN** a chunk is stored
- **THEN** `chunk_text` remains the pure source slice and `start_offset`/`end_offset` still satisfy `content[start_offset..end_offset] == chunk_text`

### Requirement: chunk metadata_json column (explicit v5 deviation)
The `chunks` table SHALL carry a `metadata_json TEXT` (nullable) column holding the per-chunk metadata bag as raw JSON: the chunk-specific keys the chunker computed (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …). It SHALL be stored as raw text (the `documents.metadata_json` pattern), parsed on demand, and `NULL` SHALL mean "no chunk metadata". This is an explicit, justified deviation from the v5 shape (the v5 `chunks` table has no metadata column): the Rust database is always built from scratch (no pre-existing `knowledge.db` is opened or migrated), and the column restores a field that was in the original Rust design and surfaces the section context in search. The invariant-preserving `chunk_text`, the byte offsets, and the `search_text` re-point are unchanged; `PRAGMA user_version` stays 1.

#### Scenario: Fresh build includes metadata_json
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a nullable `metadata_json` column and `PRAGMA user_version` = 1

#### Scenario: Round-trip
- **WHEN** a chunk is created with a `metadata_json` value
- **THEN** a row read returns the same value, and a chunk created without one returns `NULL`
