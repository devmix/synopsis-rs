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

### Requirement: queue_tasks table (event queue)

The Rust binary SHALL keep a generic event queue in the `queue_tasks` table (knowledge DB), created by the consolidated init migration `migrations/knowledge/1-init/up.sql`. The `document_jobs` table no longer exists (human decision 2026-09-06: the project has no deployed instances, so the table is defined directly in the init migration instead of a separate forward migration; `PRAGMA user_version` stays 1). The queue is the single state machine for the watcher, the startup scan, and the background worker, covering document operations and entity-linking tasks.

Columns: `id INTEGER PRIMARY KEY AUTOINCREMENT`, `type TEXT NOT NULL` (`doc:index` | `doc:delete` | `entity:link`), `identity TEXT NOT NULL` (the document path for `doc:*` events; the document id as a decimal string for `entity:link`), `event TEXT NOT NULL` (JSON payload — `source_path` and `content_hash` for `doc:index`, `source_path` for `doc:delete`, an `entity_ids` array for `entity:link`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER NOT NULL`, `updated_at INTEGER NOT NULL`. Indexes: `idx_queue_tasks_due (status, next_attempt_at)` for due queries; unique `idx_queue_tasks_identity (type, identity)` — at most one row per event identity.

Enqueue is an upsert on `(type, identity)`: an existing row is reset to `pending` with `attempts=0` and `next_attempt_at` set to the current time — a re-enqueued event moves to the END of the claim order — and its payload is updated. Payload update semantics depend on the type: `doc:index` and `doc:delete` REPLACE the payload (the file on disk is the source of truth); `entity:link` MERGES the `entity_ids` arrays (union) when the existing row is `pending`, `processing`, or `error`, and stores only the new ids when the existing row is `done` (the old ids were already linked).

Claim order is `(next_attempt_at, id)`; the backoff schedule is `30 * 2^(attempts-1)` seconds and a task that fails `max_attempts` times lands in `error` status. The worker claims tasks ONE AT A TIME: a claim flips a single due `pending` row to `processing`, the worker processes it, then claims the next — so `processing` means "currently executing" (at most one row at a time), and a per-cycle cap (100 tasks) bounds one worker cycle so the owner thread does not starve the HTTP server. On serve startup, before the startup reconcile, rows left in `processing` by an unclean shutdown (crash, SIGKILL, power loss) are reset to `pending` with `attempts` and `last_error` preserved — an interrupted attempt is not a failed one.

#### Scenario: Migration application
- **WHEN** the binary starts on a fresh knowledge.db
- **THEN** the init migration `1-init` creates `queue_tasks` with both indexes once; the `document_jobs` table does not exist

#### Scenario: Task state
- **WHEN** a task has not succeeded after `max_attempts` attempts
- **THEN** the row has `status='error'`, `attempts=max_attempts`, and `last_error` is populated; `queue reset-retries` moves it to `pending` with `attempts=0`

#### Scenario: Re-enqueue moves to the end
- **WHEN** an event with the same `(type, identity)` is enqueued while older pending tasks exist
- **THEN** the existing row is reset to `pending` with `next_attempt_at` = now and is claimed after the older pending tasks

#### Scenario: Link-event merge
- **WHEN** an `entity:link` event is enqueued for a document whose `entity:link` row is `pending` with ids A
- **THEN** the row's `entity_ids` becomes the union of A and the new ids; no pending candidate is lost

#### Scenario: Doc-event replace
- **WHEN** a `doc:index` event is enqueued for a path whose row already exists
- **THEN** the payload is replaced with the new `source_path`/`content_hash` and the row is reset to `pending`

#### Scenario: One-at-a-time claim
- **WHEN** several due `pending` tasks exist and the worker starts a cycle
- **THEN** exactly one row is `processing` at any instant; the remaining due rows stay `pending` until claimed one by one, and one cycle processes at most 100 tasks

#### Scenario: Restart recovery
- **WHEN** the server starts and rows are in `processing` status (left by an unclean shutdown)
- **THEN** they are reset to `pending` with `attempts` and `last_error` preserved, before the startup reconcile runs, and the worker processes them in a later cycle

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

### Requirement: entity_aliases table and transactional merge

The knowledge DB SHALL contain an `entity_aliases` table added by a new
numbered migration (shipped migrations are never edited; `PRAGMA
user_version` remains the sole schema-state authority): columns `entity_id`
INTEGER NOT NULL referencing `entities(id)` with ON DELETE CASCADE and
`alias` TEXT NOT NULL, with a UNIQUE constraint on `(entity_id, alias)` and a
UNIQUE index on `alias` alone (an alias names exactly one entity
globally). The table stores every surface name that has resolved to an
entity (merged or aliased), so repeated surface forms resolve by lookup.

A transactional merge operation `merge_entities(into, from)` SHALL exist in
the db crate. Preconditions (violated → error, no partial change): both
entities exist; they have the same `type` and the same `domain`. Inside one
transaction the operation SHALL: re-point `facts` (both subject and object
positions) from `from` to `into`, dropping fact rows that would collide with
the existing UNIQUE (subject, object, predicate); re-point `chunk_entities`;
move `entity_sources` rows (ignoring (entity_id, document_id) collisions);
re-point `entity_links` (subject and target positions), dropping rows that
would become self-links and ignoring duplicates; record `from.name` and
`into.name` as aliases of `into` (ignoring collisions); delete the `from`
row.

#### Scenario: Fresh-DB schema
- **WHEN** a fresh knowledge DB is built from migrations
- **THEN** the `entity_aliases` table is present with the UNIQUE (entity_id, alias) constraint and the UNIQUE alias index

#### Scenario: Merge re-points dependent rows
- **WHEN** `merge_entities(into, from)` is called for two entities of the same type and domain
- **THEN** after the transaction, no row in `facts`, `chunk_entities`, `entity_sources`, or `entity_links` references the deleted entity; both names are aliases of the surviving entity; the surviving entity's id, name, and metadata are unchanged

#### Scenario: Merge precondition violation
- **WHEN** `merge_entities` is called with ids whose types or domains differ, or where one id does not exist
- **THEN** the operation returns an error and the database is unmodified (no alias rows, no re-pointed rows, no deleted entity)

#### Scenario: Alias is globally unique
- **WHEN** an alias string is already recorded for a different entity
- **THEN** it cannot be recorded for another entity (the UNIQUE alias index rejects it)
