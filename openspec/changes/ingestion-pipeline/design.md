# Design: ingestion-pipeline

Oracle references (read-only): `../synopsis/internal/ingestion/{ingester,types,progress}.go`,
`../synopsis/internal/ingestion/runner/{runner.go,linker_helper.go}`,
`../synopsis/internal/ingestion/e2e_test.go`, `../synopsis/internal/gc/`.

## D1 — Modules in `crates/ingestion`; GC SQL in `crates/db`

```
crates/ingestion/src/
  progress.rs    — ProgressStats + ProgressTracker (indicatif)
  ingester.rs    — Ingester: per-document pipeline + transactional store
  runner.rs      — Runner: multi-source orchestration, cleanup, links
crates/db/src/gc.rs — full_clear_doc_by_id, delete_orphaned_documents
```

D1 already permits ingestion → {config, db, vectors, llm, embedding?}: the
Runner takes collaborators by injection instead of depending on embedding
construction (see D3). New dependency edges: ingestion → vectors (engine writes),
ingestion → graph (BuildEntityLinks). Both are downward edges in D1's layering.

Alternatives: a separate `pipeline` crate (rejected — no independent consumer,
would just forward); keeping GC SQL inside ingester.rs (rejected — DAO-grade SQL
belongs beside the other DAOs and is reusable by future prune/GC commands).

## D2 — Ingester shape

```rust
pub struct Ingester<'a> { /* db handle, config, collaborators */ }
Ingester::new(db: &Db, cfg: &IngestionConfig, source: &dyn Source,
              embed: &dyn EmbeddingProvider, ner: Option<&dyn NerProvider>,
              resolver: &Resolver, vectors: &VectorEngineHandle) -> Result<Self>
pub fn ingest(&self, source_path: &str, rebuild: bool) -> Result<ProgressStats, IngestionError>
```

- Source composites from the sources change already fuse parser+chunker — the
  oracle's separate parser/chunker parameters collapse into one `&dyn Source`
  (KISS; the oracle only needed two because its Parser trait predates sources).
- Per document: SHA-256 content hash → `get_by_path` → unchanged ⇒ skipped;
  chunk (empty ⇒ skipped) → batched embeddings (`batch_size`, default 100,
  count-mismatch is an error) → per-chunk NER (provider absent or NER disabled
  ⇒ skip stage) → ONE SQLite transaction via `Db::exec_tx`:
  document create-or-update (+`full_clear_doc_by_id` on update), chunk inserts,
  resolver.add_entities + chunk_entity links, synthetic fact entities via
  lookup_or_create_with_stats, facts via create_or_ignore with
  validate-fact-domain guard, fact_sources with quotes, recompute_weights.
- Per-document errors are counted and logged, never abort the run (oracle
  behavior); context/cancellation is a runtime concern — no cancellation
  parameter in the trait surface (consistent with ner change D2).
- Stats mirror the oracle: files_processed, chunks_created, embeddings_created,
  entities_extracted, facts_created, fact_sources_created, documents_
  created/updated/skipped, errors, elapsed.

## D3 — Collaborator injection, not construction

The oracle's `NewRunner` constructs the ONNX embedding provider, domain
registry, prompt loader and cache store internally. We inject:

- `&dyn EmbeddingProvider` (mock in tests; real ONNX provider constructed in the
  CLI change),
- vectors engine handle (see D5),
- pre-loaded domain configs + global config (config crate already loads them),
- optional `LlmNerCache` handle (None = NER caching disabled).

Rationale: testability without HTTP/ONNX, no hidden I/O in constructors, and the
CLI change owns real-resource wiring. Deviation from oracle structure recorded.

## D4 — Runner API

```rust
pub struct Runner { /* mutex-serialized */ }
Runner::ingest_all(&self, rebuild: bool) -> SummaryStats
Runner::ingest_source(&self, src: &SourceConfig, rebuild: bool) -> Result<ProgressStats>
Runner::sync_source(&self, changed_path: &str) -> Result<ProgressStats>   // watcher entry point
Runner::ingest_source_by_path(&self, path: &str) -> Result<ProgressStats>
Runner::prune_deleted(&self) -> Result<usize>
Runner::cleanup_orphaned_data(&self) -> Result<OrphanCleanupStats>
Runner::build_entity_links(&self) -> Result<LinkResult>
```

- A std::sync::Mutex serializes all mutating entry points (oracle r.mu): the
  future watcher and a CLI sync must never write SQLite concurrently.
- `IngestAll`: enabled sources sequentially (per-source errors collected into
  `SummaryStats.errors`, never fatal) → cleanup_orphaned_data →
  build_entity_links (link errors appended to stats).
- Source-type detection when `type` empty: path-name heuristics
  (wiki|mediawiki → mediawiki, webpage → webpages, else unstructured) — port of
  `detectSourceType`.
- Domain-enriched parsing: wrapper around the registry Source that stamps
  `metadata["domain"] = src.domain` on every parsed document (port of
  `domainEnrichedParser`).
- NER provider assembly per source: domain configs resolved from `src.domain`
  names (missing domain logged+skipped), stages from `GlobalNerConfig.methods`,
  composite built via the ner change factory; NER disabled in config ⇒ None.
  Construction failure degrades to "no NER" with a warning (oracle parity).
- `find_source_for_path`: exact abs-path match wins, else longest prefix
  (works for just-deleted files).
- `PruneDeleted`: for each indexed document under an enabled source root whose
  file is gone — full_clear + delete in one transaction per document.
- `CleanupOrphanedData`: single transaction — delete_orphaned_entity_ids,
  delete_orphaned_facts, orphaned documents (new gc fn), then vector-orphan
  reconciliation through the vectors engine (D5).
- `BuildEntityLinks`: skip when no cross_domain_links config; incremental
  window from app_kv (`relations.KVKeyLastLinkingRun` analog — reuse the same
  key string), delegate to `graph::build_entity_links`, record run timestamp.
  The LLM-linker collaborator is injected the same way as embeddings (graph
  crate already exposes the entry point).

## D5 — Vectors: post-commit writes + reconciliation (deviation)

The oracle wrote vec0 rows inside the SQLite transaction. Our vectors live in
lancedb (vectors crate) — cross-store atomicity is impossible. Order of operations:

1. SQLite transaction commits (documents/chunks/entities/facts; chunk ids known).
2. `engine.insert_batch(chunk_id, vector)` for the document's chunks.
3. Failures after step 1 leave vector-less chunks; `CleanupOrphanedData`
   reconciles both directions: vectors whose chunk_id no longer exists are
   dropped (`delete_by_chunk_ids` against the live chunk-id set).

Recorded deviation: eventual consistency + reconciliation instead of the
oracle's in-transaction atomicity. The chunk row is the source of truth.

## D6 — Backup before indexing

`VACUUM INTO '<dbdir>/backups/<base>_backup_<ts>.db'` (WAL-consistent snapshot;
rusqlite supports it; path built internally, never user input — same caveat
comment as the oracle). Timestamp format `%Y-%m-%dT%H-%M-%S-<ms>`. Backup
failures are warnings, never fatal (oracle parity). In-memory/unnamed DBs skip
backup (PRAGMA database_list reports no file). Rebuild clears all documents
whose cleaned original_path starts with the cleaned source root in ONE
transaction before parsing.

## D7 — Quote extraction (rune-aware)

Port of `extractQuoteFromChunk`: case-insensitive first occurrence of subject OR
object name (earliest wins), ±60-rune window, fallback first 120 runes, trim to
line boundary when truncated, append "...". Pure functions + full oracle test
parity (ingester_test.go cases).

## D8 — Error handling

Reuses `IngestionError`. Document-level failures increment `stats.errors` and
continue; source-level failures (parse produced nothing but errors exist,
source type unknown, DB failure) fail that source but not the whole
`ingest_all`; cleanup/link failures are recorded in SummaryStats.errors.
