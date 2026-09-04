# db-storage Specification

## Purpose

The Synopsis data storage layer: the SQLite connection (WAL, a fixed set of PRAGMAs), migrations via `PRAGMA user_version` (the sole source of truth), transactions, DAO operations over the v5-schema tables, and FTS5 search over chunks with bm25 ranking.

## Requirements

### Requirement: Connection and migrations

The `db` crate opens the SQLite database with the PRAGMA settings: WAL, synchronous=NORMAL, cache_size=-64000, mmap_size=268435456, foreign_keys=ON, busy_timeout=5000. The schema is created by a single squashed init migration (the final v5 state), embedded into the binary at compile time; `PRAGMA user_version` is the sole source of truth about the schema state (=1 after init); the `_schema_migrations` table is NOT created; a legacy knowledge.db is NOT opened and NOT migrated. Future migrations are numbered directories `<id>-<slug>/up.sql`, forward-only; shipped migrations are never edited.

#### Scenario: Fresh-database initialization
- **WHEN** the db opens a nonexistent database file
- **THEN** the full v5 schema is created (all tables, indexes, the FTS5 index and triggers), `PRAGMA user_version` = 1, and `_schema_migrations` is absent

#### Scenario: PRAGMA-parity
- **WHEN** the db opens a database
- **THEN** journal_mode=wal, synchronous=NORMAL, foreign_keys=ON, busy_timeout=5000, cache_size=-64000, mmap_size=268435456 (verified by a test)

#### Scenario: Reopening
- **WHEN** the db opens an already-initialized database (user_version=1)
- **THEN** the migrations are not re-run, the schema is not recreated, and the data is preserved

### Requirement: Transactions

Transactions are executed through the native rusqlite API (`Connection::transaction()`), the closure pattern with automatic rollback on an error or a panic inside the block; manual `BEGIN`/`COMMIT` statements are not used. DAO methods work uniformly with a connection and a transaction through a common executor abstraction.

#### Scenario: Successful transaction
- **WHEN** the transaction closure block completes successfully
- **THEN** the changes are committed (COMMIT)

#### Scenario: Error in a transaction
- **WHEN** the closure block returns an error
- **THEN** all changes are rolled back (ROLLBACK) and the database remains in its original state

#### Scenario: Panic in a transaction
- **WHEN** the closure block panics
- **THEN** the transaction is rolled back automatically (rusqlite Drop semantics) and the panic propagates outward

### Requirement: DAO operations over the v5 schema

The DAO layer covers the v5-schema tables: documents, chunks, entities, facts, links (chunk_entities, entity_links, entity_sources, fact_sources), app_kv. Operation behavior is fixed by semantics: CRUD, pagination with filters (domain via json_each, source_type, name), batch operations (IN-lists with placeholders, batches ≤ 500 rows), orphan cleanup (does not delete EntityType or fact references), GetOrCreate/CreateOrIgnore — atomic via UNIQUE constraints and `ON CONFLICT` (fixing the TOCTOU race). The SQLite parameter limit (32766) is not exceeded (batches ≤ 500×2 parameters).

#### Scenario: Document CRUD
- **WHEN** the DAO creates, reads, updates, and deletes a document
- **THEN** all operations return correct data; re-reading a deleted document yields None

#### Scenario: Pagination with filters
- **WHEN** the DAO requests a page of documents/entities with domain/source_type/name filters
- **THEN** only the items satisfying the filters are returned, in the fixed order, with correct offset/limit

#### Scenario: Atomic GetOrCreate
- **WHEN** two GetOrCreate calls with identical keys (type, name, domain) run concurrently
- **THEN** exactly one record is created and both calls return the same ID (no race)

#### Scenario: Batch operations
- **WHEN** the DAO performs a batch operation (GetByIDs, LinkBatch, DeleteByIDs) with a large list
- **THEN** the operation completes correctly without exceeding the SQLite parameter limit (batches ≤ 500 rows)

#### Scenario: Orphan cleanup
- **WHEN** the DAO deletes orphaned entities/facts
- **THEN** EntityType and the entities/facts referenced by other records are not deleted

### Requirement: FTS5 search over chunks

Chunk search uses the FTS5 index (built into the bundled SQLite, no cgo) with bm25 ranking and an optional domain filter via json_each. Results are returned with correct bm25 scores and chunk_id, sorted by relevance. Behavior is fixed (verified against the knowledge.db fixture).

#### Scenario: FTS5 search without a filter
- **WHEN** the search for 'knowledge' runs over all chunks
- **THEN** 17 hits are returned and the top-3 chunk_ids match the recorded fixture (checked against the knowledge.db fixture)

#### Scenario: FTS5 search with a domain filter
- **WHEN** a search runs with a domain restriction
- **THEN** only chunks of documents in the given domain are returned, ranked by bm25

#### Scenario: Index synchronization
- **WHEN** a chunk is created, updated, or deleted
- **THEN** the FTS5 index is synchronized automatically (the ai/ad/au triggers) and the search reflects the current state

### Requirement: Concurrent access

The `db` crate supports concurrent reads and does not block them with write transactions: connection pool + WAL (several connections share one DB). Reads run in parallel; a write transaction on one connection does not block reads on others. A nested `exec_tx` (a transaction inside a transaction on the same thread) returns an explicit error, not a deadlock and not a silent independent transaction.

#### Scenario: Parallel reads
- **WHEN** several threads simultaneously execute read queries through `with_conn`
- **THEN** all queries complete correctly, without deadlocks and without blocking each other

#### Scenario: Read during a write transaction
- **WHEN** one thread performs a write transaction through `exec_tx` while another thread performs a read
- **THEN** the read is not blocked for the duration of the transaction (WAL + a separate pooled connection)

#### Scenario: Nested transaction
- **WHEN** `exec_tx` is called inside the closure of another `exec_tx` on the same thread
- **THEN** `DbError::NestedTransaction` is returned and there is no deadlock

### Requirement: vec0 excluded

The `db` crate SHALL contain no vec0-table operations (SearchVector, UpsertVector, FormatVector, DeleteVectorsByChunkIDs, etc.) — vector search lives in the `vectors` crate (ADR 0003/0004, usearch engine); vectors are rebuilt from chunk text, the old vec0 is never read.

#### Scenario: Absence of vec0 code
- **WHEN** the db crate source is checked
- **THEN** it contains no references to vec0 tables or vec0 operations (grep check in CI)

### Requirement: ChunkDao and FTS over search_text
The `Chunk` row type exposes both `chunk_text` and `search_text`. `ChunkDao::create` and `ChunkDao::update` accept a `search_text` value and store it alongside `chunk_text`. The FTS5 index operates on `search_text`, so full-text matches are made against the section-context text, while row reads still return `chunk_text` as the chunk body and the byte offsets are unchanged.

#### Scenario: Create stores both fields
- **WHEN** a chunk is created with a `chunk_text` and a `search_text`
- **THEN** both are stored and a subsequent read returns both values

#### Scenario: FTS matches on search_text
- **WHEN** a full-text query matches a term that appears only in a chunk's `search_text` (e.g. a heading term absent from `chunk_text`)
- **THEN** that chunk is returned by the FTS search

#### Scenario: Row read returns chunk_text
- **WHEN** a chunk row is read
- **THEN** the returned body is `chunk_text` (the pure source slice), with `search_text` available as a separate field
