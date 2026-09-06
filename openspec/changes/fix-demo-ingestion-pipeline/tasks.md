# Tasks: fix the demo ingestion pipeline

Change: `fix-demo-ingestion-pipeline`

## Change header (read first)

- **Goal:** fix four demo-ingestion-pipeline issues: (1) truncated LLM
  responses → explicit non-retryable `LlmError::Truncated` + demo NER
  `max_tokens` 4096→16384; (2) worker/runner `eprintln!` → tracing +
  per-job/per-cycle progress; (3) missing global-pool merge into the
  effective domain schema; (4) commit the approved demo `logging.level`
  debug→info.
- **Context (read these first):**
  - `openspec/changes/fix-demo-ingestion-pipeline/proposal.md`
  - `openspec/changes/fix-demo-ingestion-pipeline/design.md` (D1, D2, D3)
  - `openspec/config.yaml` (frozen stack, artifact rules), `AGENTS.md`
    (gates)
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test --workspace`.
- **Order:** 1.1 → 1.2 → 1.3 (no shared files; 1.3 is the largest).

- [ ] **1.1** — LLM truncation detection + demo config bump.

  **Goal:** a budget-exhausted LLM response (`finish_reason == "length"`,
  non-empty content) becomes an explicit non-retryable
  `LlmError::Truncated { max_tokens }` instead of an opaque downstream JSON
  parse error; the demo preset gets a sufficient NER token budget.

  **File scope:**

  - `crates/llm/src/error.rs` — new `Truncated { max_tokens: i32 }` variant
    (non-retryable; `#[error(...)]` message names `max_tokens` and
    `finish_reason=length`); update the module doc (`:1–11`), the
    `is_retryable` doc (`:75–77`), and the tests
    (`is_retryable_classifies_every_variant`,
    `display_messages_carry_diagnostics`).
  - `crates/llm/src/client.rs` — `parse_chat_completion(body: &str)` →
    `parse_chat_completion(body: &str, max_tokens: i32)` (`:478`): after the
    empty-content check, `choice.finish_reason.as_deref() == Some("length")`
    → `Err(LlmError::Truncated { max_tokens })`. `classify_and_parse`
    (`:456`, a method on `LlmClient`) passes `self.config.max_tokens`. New
    tests: truncated → `Truncated` (max_tokens in the Display);
    `finish_reason:"stop"` → Ok(content); absent `finish_reason` →
    Ok(content).
  - `workspace/configs/config.demo.yaml` — NER `max_tokens: 4096 → 16384`
    (`:60`; the linker's `:87 max_tokens: 2048` is unchanged); keep the
    working-tree `logging.level: "debug" → "info"` (`:138`) — commit both.

  **Acceptance criteria (machine-checkable):**

  - `cargo test -p llm` green, including the new tests: (a) 200 + non-empty
    content + `finish_reason:"length"` → `LlmError::Truncated` with the
    configured max_tokens, `is_retryable() == false`; (b)
    `finish_reason:"stop"` → Ok(content); (c) absent `finish_reason` →
    Ok(content).
  - `grep -n "Truncated" crates/llm/src/error.rs` shows the variant and the
    non-retryable doc.
  - `config.demo.yaml`: `max_tokens: 16384` in the `ner.llm` section,
    `level: "info"` in `logging`; the linker section unchanged (2048).
  - Workspace fmt/clippy/test green.

- [ ] **1.2** — Worker/runner logging via tracing + progress visibility.

  **Goal:** every document-job outcome and every worker cycle with work is
  visible in the configured tracing log; no `eprintln!` remains in the
  worker/runner.

  **File scope:**

  - `crates/db/src/document_job.rs` — new
    `pub fn status_counts(&self) -> Result<BTreeMap<String, i64>, DbError>`:
    a single `SELECT status, COUNT(*) FROM document_jobs GROUP BY status`;
    new test (enqueue 2 index + 1 delete, mark one done and one failed →
    counts match).
  - `crates/ingestion/src/worker.rs` — replace all 8 `eprintln!`s
    (`:77`, `:90`, `:113`, `:134`, `:140`, `:148`, `:151`, `:157`):
    success → `tracing::info!` (path, op); failure below the cap →
    `tracing::warn!` (path, attempt, backoff, error); at the cap →
    `tracing::error!` (path, attempts, error); missing row → `warn!`;
    success-record failure → `error!`; unknown op → `error!`; orphan cleanup
    failure → `warn!`. After each cycle that processed ≥1 job →
    `tracing::info!` (processed count + queue counts by status via
    `status_counts()`).
  - `crates/ingestion/src/runner/mod.rs` — the 2 `eprintln!`s (`:402`,
    `:418`) → `tracing::warn!`; update the stale module doc comment
    (`:46–47`).

  **Acceptance criteria (machine-checkable):**

  - `grep -rn "eprintln" crates/ingestion/src/worker.rs
    crates/ingestion/src/runner/mod.rs` → no matches.
  - `cargo test -p db -p ingestion` green, including the new
    `status_counts` DAO test.
  - Existing worker tests stay green (behavior unchanged apart from
    logging).
  - Workspace fmt/clippy/test green.

- [ ] **1.3** — Global pool merge into the effective domain schema.

  **Goal:** implement the pinned two-layer effective domain schema
  (`config-format` spec "XML ontologies"): every domain config handed to the
  pipeline = the domain definitions + the `global.xml` pool, shadowed by
  entity id / relation predicate / rule id, domain wins silently.

  **File scope:**

  - `crates/config/src/domain.rs` — new
    `pub fn effective_domain(domain: &DomainConfig, pool: &GlobalConfig) -> DomainConfig`
    (import `crate::ontology::GlobalConfig`; `DomainConfig` derives `Clone`):
    entities = domain ++ (pool minus shadowed ids); relations = domain ++
    (pool minus shadowed predicates); extraction regex rules = domain ++
    (pool minus shadowed rule ids); order: domain first, pool additions
    after; everything else unchanged. Unit tests: entity shadowing,
    relation shadowing, rule shadowing, empty pool (identity), pool-only
    additions, order preservation.
  - `crates/cli/src/serve/bootstrap.rs` — `discover_domains` (`:298`): after
    loading, if `global` is `Some(pool)` → replace every domain config with
    `effective_domain(&cfg, &pool)`; new/updated test: a fixture directory
    with `global.xml` + `domains/` → returned domains contain the pool
    entities/relations/rules (shadowed ones only from the domain).
  - (Tests only, no production code) `crates/ingestion/src/ner/prompts.rs` —
    a test asserting that a merged config renders pool types into the LLM
    NER system prompt + JSON schema (use the existing `sample_domain()`
    helper in the test module).

  **Acceptance criteria (machine-checkable):**

  - `cargo test -p config -p cli -p ingestion` green, including:
    (a) `effective_domain` shadowing/order/identity tests; (b) a bootstrap
    test with a global pool; (c) an ingestion test where the rendered system
    prompt + JSON schema for a domain containing a pool-only entity include
    that type.
  - `grep -n "effective_domain" crates/cli/src/serve/bootstrap.rs` shows the
    application in `discover_domains`.
  - Workspace fmt/clippy/test green.
