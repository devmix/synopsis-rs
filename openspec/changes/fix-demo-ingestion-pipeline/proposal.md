# Proposal: fix the demo ingestion pipeline

## What

Four fixes to the ingestion/NER pipeline, surfaced by running `synopsis serve
--preset demo` on the edtech dataset:

1. **Truncated LLM responses fail with an opaque error.** When the model
   exhausts the token budget (`finish_reason == "length"`), the llm client
   returns the truncated content as a success; downstream the NER JSON parser
   fails with incomprehensible `EOF while parsing a string` errors. Fix: an
   explicit non-retryable truncation error naming the configured
   `max_tokens` + bump the demo preset's NER `max_tokens` 4096 → 16384.

2. **Worker failures bypass the logger.** `DocumentWorker` (7 places) and
   `Runner` (2 places) use `eprintln!` — invisible in the configured tracing
   pipeline — and there is no success/progress visibility at all. Fix: route
   through `tracing` (precedent: `ingester/mod.rs` from
   `document-jobs-queue` 1.8) + per-job outcome logs + a per-cycle queue-state
   summary.

3. **The global ontology pool is not merged into the effective domain
   schema.** The `config-format` spec pins a two-layer effective schema
   (domain + `global.xml` pool, shadowing, domain wins silently). No code
   implements the merge, so the NER prompt/JSON schema sees only the domain's
   own entities and the regex NER never sees the pool's extraction rules. The
   demo DB has zero `employee`/`department`/`policy` entities. Fix: a pure
   merge function in `config` + application in the bootstrap's
   `discover_domains`.

4. **Demo preset hygiene:** commit the approved `logging.level: debug → info`
   tweak together (already modified in the working tree).

## Why

Evidence from the demo run (2026-09-05):

- Knowledge DB after a full run: entities = hr:grade(4), hr:salary(1),
  it:service(1), product:feature(20), product:product(6),
  product:market_segment(1) — **zero pool entities** although `global.xml`
  defines employee/department/policy and the HR documents mention them.
- Logs: `EOF while parsing a string` serde errors from LLM NER; the backend
  (LM Studio / llama.cpp, `gpt-oss-20b` — a reasoning model) hit
  `max_tokens: 4096` (reasoning tokens consume the same budget).
- Worker failure lines appear as bare `worker: ...` console text, not through
  the tracing pipeline; successful jobs are invisible.

## Scope

- `crates/llm/src/error.rs`, `crates/llm/src/client.rs` — truncation
  detection + `LlmError::Truncated` variant.
- `workspace/configs/config.demo.yaml` — NER `max_tokens` 4096→16384;
  `logging.level` debug→info.
- `crates/ingestion/src/worker.rs`, `crates/ingestion/src/runner/mod.rs` —
  tracing instead of `eprintln!`, per-job + per-cycle progress logs.
- `crates/db/src/document_job.rs` — `status_counts()` DAO method for the
  cycle summary.
- `crates/config/src/domain.rs` — `effective_domain()` merge function.
- `crates/cli/src/serve/bootstrap.rs` — apply the merge in
  `discover_domains`.
- Tests for all of the above.

## Frozen contracts touched

**None.**

- The `llm-client` delta is an *additive* requirement (truncation
  detection); the pinned request/response wire contract is unchanged.
- The `entity-extraction` and `pipeline` deltas are additive requirements
  implementing the already-pinned `config-format` two-layer schema spec — a
  behavior correction, not a contract change.
- No MCP tool, CLI surface, data schema, or config format changes.

## Behavior change (explicit)

- Truncated LLM responses now fail the NER call with
  `LlmError::Truncated { max_tokens }` (non-retryable) instead of a
  downstream opaque JSON parse error.
- The worker logs per-job outcomes and a per-cycle queue summary via tracing.
- The NER prompt/JSON schema and the regex NER rules include the global pool
  types/rules for every domain; the demo dataset will extract
  `employee`/`department`/`policy` entities after re-ingest.
- Re-ingest is a manual step: `synopsis db clear` + restart (content hashes
  are unchanged, so documents would otherwise be skipped).

## Non-goals

- Do NOT change the NER prompt templates (`system.tmpl`/`user.tmpl`).
- Do NOT add a `reasoning_effort` config option (frozen config-format; the
  user's llama.cpp/LM Studio backend does not implement it — verified
  2026-09-05).
- Do NOT add per-file domain validation against the merged layer (the spec's
  shadowing nuance; the demo corpus has no cross-references to the pool in
  relations).
- No data migration; no explicit cache invalidation (the NER cache key
  includes the rendered prompt, so stale entries miss naturally).
- No changes to `crates/mcp`, `crates/search`, or the Go oracle.
