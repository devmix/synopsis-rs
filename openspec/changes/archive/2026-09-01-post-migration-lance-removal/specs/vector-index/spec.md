# vector-index Specification (delta)

## REMOVED Requirements

### Requirement: Выбор ANN-движка (lance | usearch)

**Reason:** the lance engine is removed (user decision 2026-08-31,
`post-migration-lance-removal`); usearch is the sole engine. Replaced by the
"ANN engine (usearch only)" requirement below.

## ADDED Requirements

### Requirement: ANN engine (usearch only)

The `vectors` crate SHALL provide exactly one ANN engine: `UsearchEngine` (usearch
2.x, C++/cxx FFI, disk-backed HNSW, scalar quantization default bf16, L2sq metric,
two-layer RAM/DISK persistence with per-segment WAL per ADR 0004). The engine SHALL
be compiled unconditionally — there are no Cargo engine features. The optional
`vectors.engine` config field selects the engine at runtime: absent or `"usearch"`
instantiates `UsearchEngine`; `"lance"` SHALL return an explicit error stating that
the lance engine was removed; any other value SHALL return an explicit configuration
error. The factory keeps its public signature (`create_vector_engine(engine_name,
path, config, wal_db) -> Arc<dyn VectorIndex>`), so `search`/`ingestion`/`mcp`
remain engine-agnostic. The index directory stays engine-tagged
(`<vectors_path>/usearch`) so existing dataset data is unaffected.

#### Scenario: Default without engine field
- **WHEN** the preset does not set `vectors.engine`
- **THEN** a `UsearchEngine` is instantiated

#### Scenario: Explicit usearch selection
- **WHEN** `vectors.engine = "usearch"`
- **THEN** a `UsearchEngine` is instantiated with bf16 quantization and the L2sq metric

#### Scenario: Removed engine value
- **WHEN** `vectors.engine = "lance"`
- **THEN** the factory (and config validation) returns an explicit error stating that
  the lance engine was removed and that usearch is the only engine

#### Scenario: Invalid value
- **WHEN** `vectors.engine = "foo"`
- **THEN** the factory (and config validation) returns an explicit configuration error

## MODIFIED Requirements

### Requirement: Производственные гейты

The ADR 0003 machine gates (search p95 latency < 10 ms, recall@10 ≥ 0.95 against
exact-L2 brute force, RSS-delta ≤ ~2 GB) SHALL be confirmed by machine on corpora up
to the
N≈250K × 1024-dim class (s3b spike and CI-scale integration gates). At the
extrapolation point N=1M the measured deviation was accepted by the human as a
documented worst-case (decision 2026-08-21, option 1): p95 32–55 ms, recall@10
0.911–0.933 at default parameters; recall is limited by efSearch (the HNSW beam), not
by partition coverage. efSearch remains a runtime setting for the
accuracy/latency balance without index rebuild. The index is quantized and
disk-backed (usearch HNSW, scalar quantization default bf16), parameters: M=16,
efConstruction=100, metric L2sq.

#### Scenario: Гейт recall на синтетике
- **WHEN** a batch of queries with known brute-force ground truth is run on a seeded
  CI-scale synthetic corpus
- **THEN** recall@10 ≥ 0.95

#### Scenario: Гейт латентности
- **WHEN** search latency is measured on a warmed index in the release profile
- **THEN** p95 < 10 ms

### Requirement: Конфигурация индекса

Index parameters SHALL be configurable: a config struct in the `vectors` crate with
defaults (M=16, efConstruction=100, efSearch=200, scalar quantization default bf16,
metric L2sq, dimension 1024); the optional `vectors:` section in the config preset
(additive config-format extension, decision 2026-08-21) passes overrides; a preset
without the section gets the defaults. The IVF-only fields `num_partitions`/`nprobes`
were removed with the lance engine (they do not apply to pure HNSW). The
`vectors.usearch:` section holds the two-layer persistence parameters of
UsearchEngine (ADR 0004): `max_segment_vectors` (default 1000000),
`compaction_stale_threshold` (default 30), `search_threads` (default 4).

#### Scenario: Дефолты без секции
- **WHEN** the config preset has no `vectors` section
- **THEN** the defaults above apply

#### Scenario: Переопределение runtime-параметров
- **WHEN** the `vectors` section sets efSearch
- **THEN** search uses the overridden value without rebuilding the index

#### Scenario: Usearch config defaults
- **WHEN** the `vectors.usearch` section is absent from the config
- **THEN** defaults apply: max_segment_vectors=1000000, compaction_stale_threshold=30,
  search_threads=4

#### Scenario: Usearch config override
- **WHEN** the `vectors.usearch` section sets `max_segment_vectors: 50000`
- **THEN** the RAM flush happens at 50000 vectors

#### Scenario: Invalid usearch config
- **WHEN** `vectors.usearch.max_segment_vectors: 0` or
  `compaction_stale_threshold: 101` or `search_threads: 0`
- **THEN** a validation error is returned
