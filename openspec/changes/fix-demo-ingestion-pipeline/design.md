# Design: fix the demo ingestion pipeline

## D1 — Truncation detection in the llm client

**Problem.** `parse_chat_completion` (`crates/llm/src/client.rs:478`) returns
`choices[0].message.content` on 200 without checking `finish_reason`. A
budget-exhausted response (`finish_reason == "length"`, non-empty content)
returns a partial JSON → `LlmNer` fails with an opaque serde
`EOF while parsing a string`.

**Decision.**

- New variant in `crates/llm/src/error.rs`:

  ```rust
  /// The model stopped at the token budget (`finish_reason == "length"`)
  /// with non-empty content: the content is a truncated prefix and any
  /// downstream parsing of it is meaningless. Intentionally non-retryable:
  /// the same prompt with the same budget truncates again.
  #[error("LLM response truncated at max_tokens={max_tokens} \
           (finish_reason=length); raise max_tokens in the config")]
  Truncated {
      /// The configured `max_tokens` the response was truncated at.
      max_tokens: i32,
  },
  ```

  `is_retryable()` — the variant is **not** added to the retryable match
  (stays non-retryable). Update the module doc (`:1–11`), the
  `is_retryable` doc (`:75–77`), and the tests
  (`is_retryable_classifies_every_variant`,
  `display_messages_carry_diagnostics`).
- `client.rs`: `parse_chat_completion(body: &str)` →
  `parse_chat_completion(body: &str, max_tokens: i32)` (`:478`): after the
  empty-content check, `choice.finish_reason.as_deref() == Some("length")`
  → `Err(LlmError::Truncated { max_tokens })`. `classify_and_parse` (`:456`,
  a method on `LlmClient`) passes `self.config.max_tokens`; the call site at
  `:341` is unchanged (it calls the method).
- The empty-content + length case stays `EmptyContent` (that arm already
  reports `finish_reason`).

**Alternatives considered.**

- Retry with the same budget — rejected: deterministic truncation (fixed
  seed / low temperature), wastes calls.
- Auto-chunk the prompt to fit the budget — rejected: chunking is a frozen
  parsing contract; an explicit error + config fix is sufficient for a
  personal laptop server.
- Detect truncation only when downstream parsing fails — rejected: hides the
  failure class; the llm-client spec's error taxonomy requires an explicit
  error.

**Demo config.** `config.demo.yaml`: NER `max_tokens: 4096 → 16384` (`:60`;
the linker's `max_tokens: 2048` at `:87` is unchanged). `gpt-oss-20b` is a
reasoning model; reasoning tokens consume the same budget and chunks are
≤ ~8k characters. Also commit the approved `logging.level: "debug" → "info"`
(`:138`).

**Note (verified 2026-09-05).** `gpt-oss-20b` supports `reasoning_effort` in
the OpenAI API, but the user's backend (LM Studio / llama.cpp
OpenAI-compatible endpoint) does not implement it — hence no config option;
the budget bump is the lever.

## D2 — Worker/runner logging via tracing + progress visibility

**Problem.** `worker.rs` uses `eprintln!` in 7 places (`:77` orphan cleanup
failure, `:90` unknown op, `:113` success-record failure, `:134`/`:140`/
`:148`/`:151`/`:157` failure path) and `runner/mod.rs` in 2 places (`:402`
missing domain config, `:418` NER build failure) — invisible in the
configured tracing log; no per-job success log; no queue-state summary.

**Precedent.** The frozen stack says "tracing (logging in `cli` only —
library crates stay logger-less)", but `crates/ingestion` already depends on
`tracing = { workspace = true }` and `ingester/mod.rs` already emits
`tracing::info!/warn!/error!` (commit `eaf738a`, `document-jobs-queue` 1.8).
The worker fix follows that established precedent — no new dependency.

**Decision.**

- `worker.rs`:
  - success → `tracing::info!` (path, op);
  - failure below the cap → `tracing::warn!` (path, attempt, backoff,
    error);
  - at the cap → `tracing::error!` (path, attempts, error);
  - missing row → `warn!`; success-record failure → `error!`; unknown op →
    `error!`; orphan cleanup failure → `warn!`;
  - after each cycle that processed ≥1 job → `tracing::info!` (processed
    count + queue counts by status via the new
    `DocumentJobDao::status_counts()`).
- `runner/mod.rs`: the 2 `eprintln!`s → `tracing::warn!`; update the stale
  module doc comment (`:46–47`, "Warnings use `eprintln!` (crate convention;
  no logger in the frozen stack)").
- New DAO method in `crates/db/src/document_job.rs`:
  `pub fn status_counts(&self) -> Result<BTreeMap<String, i64>, DbError>` —
  a single `SELECT status, COUNT(*) FROM document_jobs GROUP BY status`.

**Alternatives considered.**

- Return a report struct from `run_once` and log in the cli — rejected:
  splits the logging path from the ingester precedent and widens the worker
  API surface for no gain.
- Log the summary every tick — rejected: noisy when the queue is idle; log
  only when work happened.

## D3 — Global pool merge into the effective domain schema

**Problem.** `openspec/specs/config-format/spec.md` ("XML ontologies") pins a
two-layer effective domain schema: domain definitions + the `global.xml`
pool (`<entity>`, `<relation>`, `<extraction>`), shadowing by id/predicate,
domain wins silently. No code implements the merge —
`crates/config/src/domain.rs:79` defers "resolution against the global pool"
to a "`graph` concern", but no code exists. Consequences: the LLM NER
prompt/JSON schema renders only the domain's own types; regex NER never sees
the pool's extraction rules (e.g. the `email` rule); the demo DB has zero
pool entities.

**Decision.**

- Pure function in `crates/config/src/domain.rs`:
  `pub fn effective_domain(domain: &DomainConfig, pool: &GlobalConfig) -> DomainConfig`
  (import `crate::ontology::GlobalConfig`; `DomainConfig` already derives
  `Clone`):
  - entities = `domain.entities ++ (pool.entities minus shadowed ids)`;
  - relations = `domain.relations ++ (pool.relations minus shadowed
    predicates)`;
  - extraction regex rules = `domain.extraction.regex_rules ++
    (pool.extraction.regex_rules minus shadowed rule ids)`;
  - order: domain first, pool additions after;
  - everything else (name, version, description, confidence) unchanged;
  - domain wins silently (no warning) — per spec.
- Applied in `crates/cli/src/serve/bootstrap.rs::discover_domains` (`:298` —
  the single place `global` and `domains` meet): after loading, if `global`
  is `Some(pool)` → replace every domain config with
  `effective_domain(&cfg, &pool)`.
- No downstream API changes: `Runner`, `RegexNer`, `LlmNer`, and the linker
  all receive the merged config via the existing `domains` map.
  `RegexNer::new` flattens `config.extraction.regex_rules` — pool rules
  (email) are automatically included.

**Alternatives considered.**

- Merge in `Runner::build_ner_provider` — rejected: other consumers (graph
  linker) also need the merged view; centralizing at discovery avoids
  per-callsite drift.
- Merge at `load_domain_config` time — rejected: per-file loading
  deliberately has no pool access (config design D6); the pool is in a
  separate `global.xml`.

**Cache impact.** The NER cache key is the SHA-256 of the rendered prompt +
params → the prompt changes → stale entries miss naturally. No cache
migration.

**Re-ingest.** Content hashes unchanged → unchanged documents are skipped.
Manual: `synopsis db clear` + restart. (Noted in the proposal; not
automated.)
