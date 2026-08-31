# Port Verification Report

**Date:** 2026-08-31
**Scope:** Feature-by-feature verification that the Go oracle (`../synopsis`) surface is fully covered by the Rust implementation.
**Method:** Side-by-side inventory of both codebases (MCP tools, CLI, config, DB schema, ingestion, graph, search, jobs, HTTP, embeddings/LLM) + machine checks. Gaps are listed as follow-ups; they are NOT fixed by this report.

## 1. Machine checks

| Check | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| `cargo test` (full workspace) | PASS — 1444 passed, 0 failed, 8 ignored (44 test binaries) |
| Parity harness (`crates/parity-harness`) | Included in `cargo test`; fixture-based tool-response parity + recall/percentile unit tests green |

## 2. MCP tools — 12/12 present

Transport is a **deliberate deviation**: oracle uses legacy SSE (`GET /sse` + `POST /message?sessionId=`); Rust uses rmcp 3.x Streamable HTTP over the same axum listener (design D8, human decision 2026-08-18).

| # | Tool | Params parity | Notes |
|---|------|---------------|-------|
| 1 | `search` | `query` (req), `top_k` (10, 1–100), `domain` | Hybrid FTS5+vector RRF; output shape oracle-compatible |
| 2 | `catalog_overview` | none | Aggregate stats incl. graph node/edge counts |
| 3 | `catalog_documents` | `page_size`, `cursor`, `domain`, `source_type`, `name` | Cursor pagination (base64 offset) |
| 4 | `catalog_entities` | `page_size`, `cursor`, `type`, `domain`, `name` | |
| 5 | `search_entities_by_type` | `entity_type` (req), `domain`, `page_size`, `cursor` | |
| 6 | `search_facts` | `predicate`, `entity_name`, `status` (default `approved`), `domain`, `page_size`, `cursor` | |
| 7 | `get_document_context` | `document_id` (req), `include_chunks`, `include_entities`, `include_facts` | |
| 8 | `get_chunk_by_id` | `chunk_id` (req) | |
| 9 | `get_fact_by_id` | `fact_id` (req) | |
| 10 | `get_entity_dossier` | `entity_id` XOR `entity_name`, `domain`, `depth`, `include_facts`, `include_sources` | Entity resolution (id XOR name, domain disambiguation) implemented |
| 11 | `get_entity_relations` | `entity_id` XOR `entity_name`, `domain`, `depth` (1–10), `include_cross_domain` | BFS traversal, domain-bounded |
| 12 | `get_entity_links` | `entity_id` XOR `entity_name`, `domain` | Cross-domain links + provenance |

Registration order is pinned by `FROZEN_NAMES` in `crates/mcp/src/server.rs` (test-guarded).

## 3. CLI surface

| Oracle command | Rust equivalent | Status |
|---|---|---|
| `serve` (`-no-initial-sync`, `-port`, `-auto-rebuild-vectors`) | `serve` (`--no-initial-sync`, `--port`, `--auto-rebuild-vectors`) | Covered |
| `sync` | **removed** — replaced by `db clear` + serve restart (startup reconcile re-enqueues everything via the document-jobs queue) | Deliberate frozen-contract change, `remove-direct-ingest` (user decision 2026-08-29) |
| `model list\|download\|delete\|info\|benchmark` | same | Covered |
| `onnx-runtime install\|status\|uninstall` | same | Covered |
| `load-test` (`-scale`, `-seed`, `-iterations`, `-json`, `-no-fill`) | `load-test` (`--scale`, `--seed`, `--iterations`, `--json`, `--no-fill`) | Covered |
| — (no equivalent) | `queue status`, `queue reset-retries`, `db stats`, `db clear` | Additive (queue-based architecture) |

Global flags: oracle `-config/-preset/-db/-version` → Rust `--config/--preset/--dataset` (+ `--version`). `-db` (raw SQLite path override) replaced by `--dataset` (structured dataset layout) — deliberate storage-layout restructure. Usage errors exit 1 to match oracle (clap default 2 suppressed).

## 4. Configuration

All oracle config sections present: `database` (incl. `pragma` map), `embeddings` (local/api + `auto_rebuild_vectors`), `ingestion` (batch_size, chunking.markdown, chunking.json, ner, resolver), `linker`, `search` (rrf_k, top-k's, enable flags, timeout_ms, all boost knobs incl. `authority_boost` map), `graph`, `auto_update`, `scheduler.jobs` (orphan_cleanup compat entry), `logging`, `server`. `onnx.yaml` (runtime 1.28.0, 5 platforms, 3 model registry entries incl. `bge-m3-int8` 1024-dim) matches. XML ontology loading (`global.xml` + `domains/*.xml`, cross-domain-link methods, NER methods, extraction, confidence thresholds) matches.

Deliberate deltas:
- **Added:** `vectors` section (ANN engine config), `dataset` section, `paths.workspace_dir` layout.
- **Removed:** `ingestion.ner.prose` — the Go `tsawler/prose` cgo stack has no maintained Rust equivalent; never implemented; removed as dead config (`remove-unused-ner-prose-config`, human decision 2026-08-23). `NerStage::Prose` kept as a deferred-stage error for oracle-word parity.

## 5. Database schema

Oracle final v5 shape (9 tables + FTS5 + triggers) is structurally preserved: `documents`, `chunks`, `entities` (UNIQUE(type,name,domain)), `chunk_entities`, `facts` (status CHECK, UNIQUE(subject,object,predicate)), `fact_sources`, `entity_sources`, `entity_links` (PK + CHECK subject≠target), `chunks_fts` (external-content FTS5 + ai/ad/au triggers), all indexes.

Deliberate deltas:
- `app_kv` moved to the separate cache DB (with `llm_ner_cache`, `llm_linker_cache`) — storage-layout restructure.
- `chunks_vec` (vec0) **replaced** by the usearch ANN engine + `usearch_vectors_log` WAL table (ADR 0004); vectors rebuilt from chunk text, never read from vec0.
- `document_jobs` table added (queue architecture, replaces synchronous ingestion).
- `fact_sources.document_id` is INTEGER FK (oracle had a self-inconsistent TEXT column).
- Schema state: single squashed init migration, `PRAGMA user_version` sole authority (design D6); `_schema_migrations` deliberately not created.

## 6. Ingestion

- **Parsers:** all 5 oracle source types present — markdown, json, mediawiki, webpages, unstructured (registry maps `<source type>` → impl).
- **Chunkers:** markdown (headers/fixed/hybrid strategies, max_chunk_size/overlap_size), json (text_fields/combine_fields/max_objects); plus a dedicated mediawiki chunker (improvement over oracle routing).
- **NER:** RegexNER (domain regex rules) + LLMNER (OpenAI-compatible, cached) + CompositeNER with per-domain `auto_publish_threshold` filtering. ProseNER: deliberately not ported (see §4).
- **Entity resolution:** Jaro-Winkler similarity, union-find clustering, `similarity_threshold` 0.8 — matches oracle.
- **Added:** `.synignore` exclusion, SHA-256 content-hash dedup, document-jobs queue with backoff (30·2^(n-1)s), orphan GC in worker.

## 7. Graph + entity linking

- In-memory graph (petgraph DiGraph) derived from SQLite, rebuilt at startup — matches oracle's derived-graph architecture.
- BFS traversal: max_depth (5, hard max 10), max_nodes (1000), direction, relation-type filter, `follow_entity_links` as the only cross-domain key — matches oracle semantics, with oracle bugs fixed (cross-domain fact edges always blocked; deterministic numeric sort keys).
- **CEL linker:** six-function contract (design D5): `facts`, `has_fact`, `chunks`, `chunk_contains`, `neighbors`, `path_exists` + `A/B` entity variables (incl. `metadata_json`). Oracle's 7th convenience function `metadata(entity,key)` is **not ported** — metadata is accessed via the `metadata_json` variable; the bundled ontology's single expression does not use the function. Low risk; see follow-ups.
- Linking methods: `expression` (CEL), `equals` (min-words 2), `llm` (confidence 0.7, batch 5, decision-cached) — all present.

## 8. Search

Hybrid pipeline matches: lexical (FTS5/BM25) + semantic (vector) legs → RRF fusion (`1/(k+rank)`, k=20) → calibrated `0.7·rrf + 0.3·bm25` → enrich → rerank (deprecated 0.2 / official 1.5 / expired 0.1 / recent 1.2 within 90 days / authority map) → truncate → graph expansion (non-fatal). Degradation semantics match (one leg fails → survivor; both fail → error).

Deliberate deltas: legs run sequentially (oracle used goroutines); `search.timeout_ms` parsed but not plumbed (recorded deviation).

## 9. Jobs / scheduling

- Oracle's `orphan_cleanup` scheduler job (gocron, default disabled, 3600s) → Rust: universal `scheduler.jobs` config kept for compat; actual orphan GC runs inside the document worker (change `document-jobs-queue` task 1.9; gocron replaced).
- File watcher (notify, debounce 30s), initial sync on startup, retry-failed sweep — all present.
- Cross-domain linking runs during ingestion (incremental via `app_kv.last_linking_run`).

## 10. HTTP endpoints

| Oracle | Rust | Status |
|---|---|---|
| `GET /health` (uptime/metrics/components payload) | `GET /health` (status/version/sync_state/counters payload) | Deliberate payload redesign |
| `GET /sse`, `POST /message` | rmcp Streamable HTTP (all non-health paths) | Deliberate transport replacement (D8) |

## 11. Embeddings / LLM

- ONNX Runtime 1.28.0 as external `.so`/`.dylib` (5 platforms), download/verify mechanism ported from oracle (`onnx.yaml` registry, checksums, cache manifest).
- `bge-m3-int8` (1024-dim) default; `bge-small-en-v1.5`, `paraphrase-multilingual-MiniLM-L12-v2` in registry. Tokenizer from `tokenizer.json`; embedding cache present.
- LLM: OpenAI-compatible client (ureq, blocking → `spawn_blocking`) used by LLM NER and LLM cross-domain linker; prompts from `prompts_path` .tmpl files with embedded fallbacks; response formats json_object/json_schema; retries on 429/5xx. Fail-fast validation (oracle had silent defaults — deliberate hardening).

## 12. Not yet done (known, planned)

1. **Website** — oracle ships a Docusaurus 3.10.2 site (`site/`); not yet ported. Tracked as Phase 4 (separate large task).
2. **CI/CD** — `ci.yml` exists (fmt/clippy/test + 5-target zigbuild matrix + Alpine smoke); release pipeline (artifacts + checksums) not yet added. Tracked as Phase 3.

## 13. Gaps / follow-ups

| # | Item | Severity | Recommendation |
|---|------|----------|----------------|
| 1 | CEL `metadata(entity,key)` function not ported (design D5 six-function contract) | Low — bundled ontology doesn't use it; custom ontologies using it would fail at link time | Document in ontology authoring guide; add function later if needed |
| 2 | `search.timeout_ms` parsed but not plumbed (sequential legs) | Low — no timeout enforcement on hybrid search | Plumb per-leg deadline if real-world latency issues appear |
| 3 | ProseNER not available (no Rust equivalent of the cgo prose stack) | Medium for feature parity only — regex+LLM NER cover extraction; deferred provider tracked in `remove-unused-ner-prose-config` non-goals | Future change if an ONNX-based NER crate (gliner2-rs/redact-ner) is acceptable — frozen-stack decision |
| 4 | `sync` subcommand removed (frozen-contract change) | None — deliberate, `db clear` + restart replaces it | None |

## 14. Verdict

**The functional surface of the Go oracle is fully covered** — 12/12 MCP tools, all CLI subcommands (one deliberate removal), all config sections (one deliberate removal), the complete v5 DB schema (plus deliberate queue/WAL additions), all 5 ingestion source types, graph + CEL linking, hybrid search, scheduling, embeddings, and LLM features. All deviations are deliberate, recorded decisions (archived OpenSpec changes / ADRs), not omissions. Machine gates are green (1444 tests). Remaining known work is the website (Phase 4) and the release CI (Phase 3).
