# vector-index Specification

## MODIFIED Requirements

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
