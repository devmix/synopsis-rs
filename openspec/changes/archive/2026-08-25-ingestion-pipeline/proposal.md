# Proposal: ingestion-pipeline

## Change name
`ingestion-pipeline`

## Why

Change 3/3 of the approved ingestion series (sources → ner → **pipeline**, human
decision 2026-08-23). Sources delivered parsing/chunking composites; ner delivered
extraction and resolution. What is missing is the orchestration that turns a
configured source directory into persisted knowledge: the Ingester
(`../synopsis/internal/ingestion/ingester.go`: parse → chunk → embed → NER →
transactional store with hash-dedup and rebuild) and the Runner
(`../synopsis/internal/ingestion/runner/runner.go`: multi-source execution,
incremental sync entry points, orphan cleanup, prune-deleted, cross-domain link
building). Every later consumer (CLI sync command, future serve mode) drives the
pipeline through these two pieces.

## What changes

1. **Progress tracking** (`crates/ingestion/src/progress.rs`) — `ProgressStats`
   counters + indicatif-backed tracker (frozen stack), port of `progress.go`.
2. **DB GC module** (`crates/db/src/gc.rs`, new) — `full_clear_doc_by_id`
   cascading delete (chunks, chunk_entities, entity_sources, fact_sources,
   facts) and `delete_orphaned_documents`; complements the existing
   `delete_orphaned_entity_ids` / `delete_orphaned_facts`. Pure SQL over
   `ConnectionOrTx`, DAO-style with tests.
3. **Ingester** (`crates/ingestion/src/ingester.rs`) — per-document pipeline:
   SHA-256 content-hash dedup (skip unchanged), chunk → batched embeddings →
   per-chunk NER → single SQLite transaction (document upsert + full-clear on
   update, chunks, entity resolution + chunk links, synthetic fact entities,
   facts with domain validation, fact_sources with rune-aware quote extraction,
   weight recompute). Vectors go to the lancedb-backed vectors engine AFTER the
   SQLite commit (see design D5 — deliberate deviation from the oracle's
   in-transaction vec0 writes).
4. **Backup + rebuild** — `VACUUM INTO` snapshot before indexing;
   rebuild clears all documents under the source root in one transaction.
5. **Runner** (`crates/ingestion/src/runner.rs`) — registry-driven multi-source
   execution: `IngestAll` (sequential sources + cleanup + link building),
   `IngestSource`, `SyncSource`/`IngestSourceByPath` (watcher entry points),
   `PruneDeleted`, `CleanupOrphanedData`, `BuildEntityLinks` (graph crate),
   source-type detection, domain-enriched parser wrapper, composite NER
   provider assembly per source, mutex serialization.
6. **E2E test** — full pipeline against in-memory SQLite + mock embedding
   provider + regex-only NER, anchored to oracle e2e scenarios.

## Non-goals

- **Watcher/scheduler (serve mode)** — explicitly out of scope (human decision
  2026-08-23): the Runner exposes only the sync API; notify/cron wiring comes
  with the CLI/serve change.
- **`ReEmbedChunks`** (model-dimension migration) — deferred to the CLI change:
  its only caller is a CLI command; the vectors engine already provides
  `rebuild()` when needed.
- No MCP tools, no CLI commands — those changes consume this pipeline.
- No new migrations (llm_ner_cache precedent carries; all GC work targets the
  frozen v5 shape).

## Risks

- Cross-store consistency: SQLite transactions cannot cover lancedb writes —
  mitigated by write ordering + orphan reconciliation (design D5).
- `VACUUM INTO` needs the DB file path; in-memory test DBs skip backup
  (oracle behaves the same).
- Runner is the first ingestion consumer of embedding + vectors + graph —
  dependency additions must respect D1 (all three are permitted base deps).
