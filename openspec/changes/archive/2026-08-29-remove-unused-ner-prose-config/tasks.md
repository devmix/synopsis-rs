# Tasks: Remove unused `ingestion.ner.prose` config section

- [x] 1.1 Remove `ProseNerConfig` (struct + field + default + doc) from preset.rs, drop the
      test block in config/tests/preset.rs, and delete `prose:` from the 3 YAML presets
      (config.default.yaml, config.demo.yaml, tests/data/config.default.yaml). Keep
      `NerStage::Prose` in ontology.rs untouched. (commit 9986acf)

## 1.1 Remove dead `ingestion.ner.prose` config section

**Goal:** Delete the unused `ProseNerConfig` config section so no dead config remains.

**Scope файлов:**
- `crates/config/src/preset.rs` — remove `pub prose: ProseNerConfig` from `NerConfig`;
  remove `pub struct ProseNerConfig`; remove its default in `NerConfig::default()`; fix doc
  comments referencing prose (e.g. "NER configuration (prose and/or LLM providers)" and
  "Rule / POS-based prose NER settings").
- `crates/config/tests/preset.rs` — remove the test block reading `cfg.ingestion.ner.prose`
  (asserts on enable_pos / enable_ner / custom_patterns / entity_types / min_confidence /
  location_min_confidence).
- `workspace/configs/config.default.yaml` — remove the `prose:` subsection under
  `ingestion.ner`.
- `workspace/configs/config.demo.yaml` — same.
- `crates/config/tests/data/config.default.yaml` — same (test fixture must stay
  deserializable; `NerConfig` has no `deny_unknown_fields`, so removal is required only for
  cleanliness/contract accuracy, but do remove it).

**Out of scope (do NOT touch):**
- `crates/config/src/ontology.rs` `NerStage::Prose` and
  `crates/ingestion/src/error.rs` `IngestionError::ProseNerDeferred` — the deferred-stage
  error path; a separate concept from the config section.

**Dependencies:** none.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test -p config` all pass.
- `grep -rn "ProseNerConfig" --include=*.rs .` returns 0 matches.
- `grep -rn "prose:" --include=*.yaml .` returns 0 matches (no `prose:` subsection anywhere).
- `NerStage::Prose` and `ProseNerDeferred` remain present and unchanged in ontology.rs /
  error.rs (the deferred-stage error still works).
- `cargo test --workspace` passes (no other crate referenced `ProseNerConfig`).

**Oracle reference:** n/a (pure removal of dead Rust config; the Go oracle's `prose_ner.go`
was never ported and is irrelevant).
