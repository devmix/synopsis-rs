# Tasks: add-llm-reasoning-effort

Order reflects dependencies: `1.1 → 1.2`; `2.1` (prompt) is independent;
`2.2` (presets) needs `1.1`; `2.3` (docs) needs all. The prompt nudge (2.1)
lands before the presets apply `low` (2.2) so `low` is only ever used with the
recall-safe prompt in place.

Each task is self-contained for a fresh agent (~100k context): goal, exact
file scope, dependencies, machine-checkable acceptance criteria, and reference
paths. Read `design.md` and `openspec/config.yaml` first. The diff per task is
small (well under ~500 lines).

## 1. The `reasoning_effort` field

- [x] 1.1 config: add `reasoning_effort` to `LlmConfig` (build-green)

  **Goal.** Add an optional `reasoning_effort` string field to the shared
  `LlmConfig` struct (the per-consumer LLM settings used by NER and the
  linker). The field is additive: an absent key deserializes to `""` (the
  struct already derives `Deserialize` with a struct-level
  `#[serde(default)]`), and `""` means "not set" (the field will not be sent
  to the LLM). Adding the field breaks the six exhaustive `LlmConfig` test
  literals at compile time ("missing field"), so this task also fixes all six
  (one-line additions) to keep the workspace green. Add a config parse test.

  **Design references.** `design.md` D2/D7; delta spec
  `specs/config-format/spec.md` ("LLM reasoning effort config").

  **File scope (exact).**

  - `crates/config/src/preset.rs`
    - Reading list: the `LlmConfig` struct (line ~676-695).
    - Add `pub reasoning_effort: String` after `max_retries`, with a doc
      comment (required — `missing_docs = "deny"`), e.g. "Reasoning effort sent
      to the model (`"low"`/`"medium"`/`"high"`); empty = not set (the field is
      not sent)." No per-field serde attribute is needed (the struct-level
      `#[serde(default)]` handles absent keys).
  - The six exhaustive `LlmConfig` test literals — add
    `reasoning_effort: String::new(),` to each (a value is fine where the test
    wants to exercise the field):
    - `crates/llm/src/client.rs` — `valid_config` (line ~611)
    - `crates/llm/tests/client.rs` — `valid_config` (line ~25)
    - `crates/ingestion/src/ner/llm.rs` — `llm_config` (line ~302)
    - `crates/ingestion/src/ner/composite.rs` — `llm_config` (line ~402)
    - `crates/graph/src/linker.rs` — the test `LlmConfig` (line ~969)
    - `crates/graph/tests/llm_linker_pipeline.rs` — the test `LlmConfig`
      (line ~153)
  - `crates/config/tests/` (the preset YAML parse tests)
    - Add a test: a preset YAML that sets `ner.llm.reasoning_effort: low`
      parses to `LlmConfig.reasoning_effort == "low"`; and a preset without the
      key parses to `""`.

  **Dependencies.** None (first task).

  **Acceptance criteria (machine-checked).**

  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
    clean; `cargo test` green (full workspace).
  - `rg -n "reasoning_effort" crates/config/src/preset.rs` — exactly one match
    (the new field).
  - `crates/config/tests/` includes the new parse tests (set → value, absent →
    `""`).
  - All six test constructors compile (the workspace builds — the missing-field
    error is gone).

- [x] 1.2 llm client: send `reasoning_effort` when set

  **Goal.** When `LlmConfig.reasoning_effort` is non-empty, the chat-completions
  request body includes a top-level `reasoning_effort` field; when empty, the
  field is omitted and the body is byte-identical to the pre-change shape. No
  client-side validation of the effort value — it is passed through verbatim.

  **Design references.** `design.md` D1/D3/D7; delta spec
  `specs/llm-client/spec.md` ("Reasoning effort control"). Reference wire
  shape (pre-change body): a recorded NER request — the pre-change body
  carries exactly `model`, `messages`, `temperature`, `seed`, `max_tokens`,
  `response_format`.

  **File scope (exact).**

  - `crates/llm/src/client.rs`
    - Reading list: the `RequestBody` struct (line ~525-538),
      `build_request_body` (line ~401-425), the `valid_config` / `config_with`
      test helpers (line ~610 / ~624), and the existing test harness that
      captures the serialized request body.
    - `RequestBody`: add `reasoning_effort: String` annotated with
      `#[serde(skip_serializing_if = "String::is_empty")]`, with a doc comment
      ("Reasoning effort; omitted when empty.").
    - `build_request_body`: set `reasoning_effort:
      self.config.reasoning_effort.clone()` in the `RequestBody` literal.
    - Tests: add a serialization test — a client built with
      `reasoning_effort: "low"` produces a body whose JSON contains
      `"reasoning_effort":"low"`; a client with `reasoning_effort: ""` produces
      a body with **no** `reasoning_effort` key (field set exactly: model,
      messages, temperature, seed, max_tokens, response_format). Follow the
      existing test pattern in this file for capturing the serialized body.
      Build the set-value config via `config_with` (mutating an existing
      `valid_config`).

  **Dependencies.** Task 1.1.

  **Acceptance criteria (machine-checked).**

  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
    clean; `cargo test` green.
  - `cargo test -p llm` includes the new serialization test (set → present,
    empty → absent).
  - The serialized body with empty `reasoning_effort` contains no
    `reasoning_effort` key.

## 2. Apply: prompt, presets, docs

- [x] 2.1 NER prompt: reconcile divergence + soft exhaustiveness directive

  **Goal.** (a) Reconcile the pre-existing prompt divergence: remove the
  "Description must be concise: 200-500 characters" line from the embedded
  prompt so it matches the workspace override (design D6). (b) Add one soft
  exhaustiveness sentence to the NER system prompt's entity-description
  section so `low`-effort extraction still returns every explicitly-named
  entity (including entities referenced only via a `[[wiki-link]]` or a bare
  name), added to BOTH the workspace override and the embedded default (the
  two must stay in sync — the embedded copy is the fallback).

  **Design references.** `design.md` D5/D6; delta spec
  `specs/entity-extraction/spec.md` ("NER prompt exhaustiveness directive").

  **File scope (exact).**

  - `workspace/configs/prompts/ner/system.tmpl` (the runtime override)
    - Reading list: the "Entity descriptions" (or equivalent) section.
    - Add one sentence, e.g. "Extract every explicitly-named entity in the
      chunk, including entities referenced only via a `[[wiki-link]]` or a bare
      name; do not omit an entity just because it is mentioned briefly." Exact
      wording is free; it must be a **soft** instruction, not a penalty or hard
      constraint (a strong/penalty nudge was measured to truncate the response
      to invalid JSON).
  - `crates/ingestion/src/ner/templates/system.tmpl` (the embedded default /
    fallback)
    - Remove the "Description must be concise: 200-500 characters" line (it is
      already removed in the working tree — keep it removed) so this copy
      matches the workspace override.
    - Add the SAME exhaustiveness sentence at the equivalent point (keep the
      two files in sync for the new content).
  - `crates/ingestion` (the NER prompt render test)
    - Add a test that the rendered NER system prompt contains the
      exhaustiveness directive — the workspace-override render path, and the
      embedded-fallback path if both are reachable from a test.

  **Dependencies.** None (independent — prompt content only).

  **Acceptance criteria (machine-checked).**

  - `rg -n "Extract every|explicitly-named|wiki-link"
    workspace/configs/prompts/ner/system.tmpl
    crates/ingestion/src/ner/templates/system.tmpl` — both files contain the
    directive.
  - The "200-500 characters" line is removed from the embedded copy (the two
    prompt files now match on that section); it stays absent from the workspace
    copy.
  - `cargo test -p ingestion` includes the new render test and is green.

- [x] 2.2 shipped presets: `low` on NER + linker, NER `max_tokens` 8192

  **Goal.** Point the shipped presets at low reasoning effort for both LLM
  consumers and set NER's token budget. NER `max_tokens` → 8192 (safety bound
  that kills the long-reasoning hang; 8192 is well above the observed
  `low`-effort output of ~1355 tokens). The linker keeps its existing
  `max_tokens`. The `config.default.yaml` working tree already carries the
  local LLM endpoint / model / logging (committed as part of this change) —
  keep those as-is and only add `reasoning_effort` + set `max_tokens`.

  **Design references.** `design.md` D4; delta spec
  `specs/config-format/spec.md` ("LLM reasoning effort config" — the shipped
  presets set `low`).

  **File scope (exact).**

  - `workspace/configs/config.default.yaml`
    - Reading list: the `ingestion.ner.llm` block (~line 44-59) and the
      `linker.llm` block (~line 72-81).
    - `ingestion.ner.llm`: add `reasoning_effort: low`; set
      `max_tokens: 8192`.
    - `linker.llm`: add `reasoning_effort: low` (leave its `max_tokens`
      unchanged).
    - Keep the file's existing local endpoint / model / logging values as-is
      (they are already in the working tree and committed with this change).
  - `workspace/configs/config.demo.yaml`
    - Same two blocks: `ner.llm.reasoning_effort: low` +
      `ner.llm.max_tokens: 8192`; `linker.llm.reasoning_effort: low`.
  - `workspace/configs/README.md`
    - If it documents the LLM section, note the new `reasoning_effort` key and
      the NER 8192 bound.

  **Dependencies.** Task 1.1 (the key is a real field).

  **Acceptance criteria (machine-checked).**

  - `rg -n "reasoning_effort: low" workspace/configs/config.default.yaml
    workspace/configs/config.demo.yaml` — two matches per file (ner + linker).
  - `rg -n "max_tokens: 8192" workspace/configs/config.default.yaml
    workspace/configs/config.demo.yaml` — the NER block uses 8192.
   - `cargo test` green (the serve-bootstrap test loads the default preset and
     still boots).

   **Revision history.**

   - **R1 (orchestrator, pre-review):** the working-tree diff of
     `config.default.yaml` included an out-of-scope **embedding-model swap**
     (`bge-small-en-v1.5` → `bge-m3-int8`) that reversed the documented shipped
     default (AGENTS.md + `workspace/configs/README.md`: the preset deliberately
     uses `bge-small-en-v1.5`, "do not 'fix' it") and changes the embedding
     dimension 384 → 1024. **Decision (user, option a):** revert the swap — keep
     `bge-small-en-v1.5` active (`bge-m3-int8` commented out, matching committed
     HEAD) and commit only the intended changes (local endpoint/model/logging +
     `reasoning_effort: low` on NER & linker + NER `max_tokens: 8192`). No
     README/AGENTS.md change needed.

- [x] 2.3 docs: `reasoning_effort` + low-effort behavior

  **Goal.** Document the new `reasoning_effort` config key and the low-effort
  behavior in the site docs.

  **Design references.** `design.md` D4/D5; the three delta specs.

  **File scope (exact).**

  - `site/docs/reference/config-schema.mdx`
    - Add a row for `reasoning_effort` (string, optional, empty = not sent;
      per-consumer LLM config: `ner.llm`, `linker.llm`).
  - `site/docs/guides/` (the NER / LLM / troubleshooting guide that documents
    the LLM config)
    - Note that the shipped presets set `reasoning_effort: low` for NER and the
      linker (gpt-oss is a reasoning model; `low` bounds the long-reasoning
      hang) and that NER `max_tokens` is 8192.
    - If a guide describes NER extraction completeness, note the exhaustiveness
      directive in the prompt.

  **Dependencies.** Tasks 1.1, 1.2, 2.1, 2.2 (docs describe the final
  behavior).

  **Acceptance criteria (machine-checked).**

  - `rg -n "reasoning_effort" site/docs/reference/config-schema.mdx` — at least
    one match.
  - `rg -n "reasoning_effort" site/docs/guides/` — at least one match.
  - `cargo test` green (no code changes; docs only).
