# vector-index Specification

## Purpose

A local ANN index of embeddings on a disk-backed/quantized engine: storing chunk vectors, kNN search within a laptop RAM budget, cascading consistency with the SQLite chunk store, and the fixture format for machine parity.

## Requirements

### Requirement: Index lifecycle

The `vectors` crate provides creation, opening, persistence, and recreation of the ANN index in the data directory. The index is created for a fixed dimensionality (1024 by default for bge-m3); opening a non-existent index is a distinguishable error; recreation atomically replaces the contents. Re-opening an existing index does not rebuild it. UsearchEngine supports a two-layer RAM/DISK architecture with per-segment WAL in SQLite for insert durability between rebuilds (ADR 0004).

#### Scenario: Creating a new index
- **WHEN** an index is created in an empty data directory
- **THEN** an empty store with the given dimensionality is created, ready to accept vectors

#### Scenario: Opening an existing index
- **WHEN** a previously saved index is opened
- **THEN** the RAM layer is restored from the snapshot, the DISK layers as read-only mmap views, and all inserted vectors are available to search

#### Scenario: Opening a non-existent index
- **WHEN** an index absent from the data directory is opened
- **THEN** an explicit "index does not exist" error is returned

#### Scenario: Recreating the index
- **WHEN** an index recreation is performed
- **THEN** the WAL is cleared, the DISK layer files are removed, and the RAM is replaced by a new set of vectors — with no garbage accumulation

#### Scenario: Flush on RAM overflow
- **WHEN** the size of the RAM index reaches `vectors.usearch.max_segment_vectors`
- **THEN** the RAM layer is saved as a new DISK segment (read-only mmap), the WAL of the RAM segment is cleared, and the RAM is reset to empty

#### Scenario: Search with WAL filtering
- **WHEN** a search is performed on an index with DISK layers
- **THEN** the results of each layer are filtered by the stale set (WAL DEL records: removed and superseded keys); duplicates between layers are resolved in favor of the fresher layer

#### Scenario: Compaction
- **WHEN** the share of stale vectors in the DISK layers exceeds `vectors.usearch.compaction_stale_threshold`
- **THEN** a background compaction packs the live vectors into new segments (monotonic ids), the segment catalog is changed atomically, and the WAL is cleared after the switch

#### Scenario: Persistence across restart
- **WHEN** the index is restarted after a crash
- **THEN** WAL rows of non-existent segments are removed (self-healing), and garbage files in the catalog are cleaned up; RAM inserts since the last flush/shutdown-save are lost (a documented window, repair — consumer reconciliation or `rebuild`)

### Requirement: Insertion and kNN search

The `vectors` crate accepts ready-made `(chunk_id, vector)` pairs — the crate does NOT call the embedding model (neither the query path nor the insert path loads the model). Insertion is streaming (in batches). Search returns the top-k nearest `(chunk_id, distance)` by the L2 metric; ranking is by distance. The efSearch search parameter is a runtime setting with a default from ADR 0003 (efSearch=200); the IVF-only field nprobes persists as a vestigial/unused config field (present for tolerant parsing, not used by the HNSW engine; the lance engine it belonged to was removed, `post-migration-lance-removal`, 2026-08-31).

#### Scenario: Insertion and search
- **WHEN** a set of vectors is inserted and a search is performed with a query vector
- **THEN** up to k `(chunk_id, distance)` pairs are returned, sorted by distance ascending

#### Scenario: Dimension mismatch
- **WHEN** a vector of a dimensionality different from the index's is inserted or searched
- **THEN** an explicit dimension error is returned

#### Scenario: Empty index
- **WHEN** a search is performed on an empty index
- **THEN** an empty result is returned without an error

### Requirement: Cascading deletion and synchronization with SQLite

Vectors correspond to SQLite chunks one-to-one by chunk_id; both DBs (SQLite, the vector index) must always be in sync (human decision 2026-08-21). The `vectors` crate provides deletion of vectors by a list of chunk_ids (batched, idempotent to missing ids) and enumeration of all chunk_ids of the index for reconciliation. The cascade protocol on chunk deletion: first the vectors are deleted by chunk_id, then the chunk rows in SQLite — a failure between the steps leaves a recoverable state (orphan vectors are cleaned up by reconciliation; missing vectors are restored by re-embedding).

#### Scenario: Deleting chunk vectors
- **WHEN** vectors are deleted by the chunk_id of a deleted chunk
- **THEN** the vectors disappear from the index and are not returned by search; a repeat call with the same ids is not an error

#### Scenario: Cascade order
- **WHEN** a consumer deletes a document with chunks
- **THEN** vector deletion is performed BEFORE deleting the chunk rows in SQLite (the ordering contract is fixed in the crate documentation)

#### Scenario: Index reconciliation with SQLite
- **WHEN** a reconciliation is performed
- **THEN** enumerating the chunk_ids of the index makes it possible to find orphan vectors (ids absent from SQLite) for their deletion

### Requirement: Production gates

The ADR 0003 machine gates (search p95 latency < 10 ms, recall@10 ≥ 0.95 against exact-L2 brute force, RSS-delta ≤ ~2 GB) SHALL be confirmed by machine on corpora up to the N≈250K × 1024-dim class (s3b spike and CI-scale integration gates). At the extrapolation point N=1M the measured deviation was accepted by the human as a documented worst-case (decision 2026-08-21, option 1): p95 32–55 ms, recall@10 0.911–0.933 at default parameters; recall is limited by efSearch (the HNSW beam), not by partition coverage. efSearch remains a runtime setting for the accuracy/latency balance without index rebuild. The index is quantized and disk-backed (usearch HNSW, scalar quantization default bf16), parameters: M=16, efConstruction=100, metric L2sq.

#### Scenario: Recall gate on synthetic data
- **WHEN** a batch of queries with known brute-force ground truth is run on a seeded CI-scale synthetic corpus
- **THEN** recall@10 ≥ 0.95

#### Scenario: Latency gate
- **WHEN** search latency is measured on a warmed index in the release profile
- **THEN** p95 < 10 ms

### Requirement: SYNX fixture format (vectors.bin)

The `vectors` crate reads and writes the binary fixture format vectors.bin (the fixture format contract): magic "SYNX", version u32 LE = 1, dim u32 LE, count u64 LE, then count rows of `[u32 LE chunk_id][f32 LE × dim]`, sorted by chunk_id ascending. Reading supports streaming without loading the whole file into memory; a format violation (magic/version/truncated file) is an explicit error.

#### Scenario: Write/read round-trip
- **WHEN** a set of vectors is written in the SYNX format and read back
- **THEN** the data is identical and the rows are sorted by chunk_id ascending

#### Scenario: Corrupted file
- **WHEN** a file has a wrong magic, version, or a truncation in the middle of a row
- **THEN** an explicit format error indicating the reason is returned

### Requirement: Index configuration

Index parameters SHALL be configurable: a config struct in the `vectors` crate with defaults (M=16, efConstruction=100, efSearch=200, scalar quantization default bf16, metric L2sq, dimension 1024); the optional `vectors:` section in the config preset (additive config-format extension, decision 2026-08-21) passes overrides; a preset without the section gets the defaults. The IVF-only fields `num_partitions`/`nprobes` **persist as vestigial/unused config fields**: they are present in the config struct for tolerant parsing (serde defaults 256/32) but are NOT used by `UsearchEngine` (they do not apply to pure HNSW); they are not removed from the preset format. The `vectors.usearch:` section holds the two-layer persistence parameters of UsearchEngine (ADR 0004): `max_segment_vectors` (default 1000000), `compaction_stale_threshold` (default 30), `search_threads` (default 4).

#### Scenario: Defaults without the section
- **WHEN** the config preset has no `vectors` section
- **THEN** the defaults above apply

#### Scenario: Runtime parameter override
- **WHEN** the `vectors` section sets efSearch
- **THEN** search uses the overridden value without rebuilding the index

#### Scenario: Usearch config defaults
- **WHEN** the `vectors.usearch` section is absent from the config
- **THEN** defaults apply: max_segment_vectors=1000000, compaction_stale_threshold=30, search_threads=4

#### Scenario: Usearch config override
- **WHEN** the `vectors.usearch` section sets `max_segment_vectors: 50000`
- **THEN** the RAM flush happens at 50000 vectors

#### Scenario: Invalid usearch config
- **WHEN** `vectors.usearch.max_segment_vectors: 0` or `compaction_stale_threshold: 101` or `search_threads: 0`
- **THEN** a validation error is returned

#### Scenario: Vestigial IVF fields tolerated
- **WHEN** the `vectors` section sets `num_partitions` or `nprobes`
- **THEN** the values are parsed (tolerant parsing, defaults 256/32) but `UsearchEngine` ignores them (pure HNSW has no IVF partitions/probes)

### Requirement: ANN engine (usearch only)

The `vectors` crate SHALL provide exactly one ANN engine: `UsearchEngine` (usearch 2.x, C++/cxx FFI, disk-backed HNSW, scalar quantization default bf16, L2sq metric, two-layer RAM/DISK persistence with per-segment WAL per ADR 0004). The engine SHALL be compiled unconditionally — there are no Cargo engine features. The optional `vectors.engine` config field selects the engine at runtime: absent or `"usearch"` instantiates `UsearchEngine`; `"lance"` SHALL return an explicit error stating that the lance engine was removed; any other value SHALL return an explicit configuration error. The factory keeps its public signature (`create_vector_engine(engine_name, path, config, wal_db) -> Arc<dyn VectorIndex>`), so `search`/`ingestion`/`mcp` remain engine-agnostic. The index directory stays engine-tagged (`<vectors_path>/usearch`) so existing dataset data is unaffected.

#### Scenario: Default without engine field
- **WHEN** the preset does not set `vectors.engine`
- **THEN** a `UsearchEngine` is instantiated

#### Scenario: Explicit usearch selection
- **WHEN** `vectors.engine = "usearch"`
- **THEN** a `UsearchEngine` is instantiated with bf16 quantization and the L2sq metric

#### Scenario: Removed engine value
- **WHEN** `vectors.engine = "lance"`
- **THEN** the factory (and config validation) returns an explicit error stating that the lance engine was removed and that usearch is the only engine

#### Scenario: Invalid value
- **WHEN** `vectors.engine = "foo"`
- **THEN** the factory (and config validation) returns an explicit configuration error
