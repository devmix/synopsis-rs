# Design: Remove unused `ingestion.ner.prose` config section

## Approach

1. `crates/config/src/preset.rs`:
   - Delete `pub prose: ProseNerConfig` from `NerConfig`.
   - Delete `pub struct ProseNerConfig { ... }` (fields: enable_pos, enable_ner,
     custom_patterns, entity_types, min_confidence, location_min_confidence).
   - Remove the `prose: ProseNerConfig { ... }` entry from `NerConfig::default()`.
   - Fix the doc comments on `NerConfig` that reference "prose" as a sub-provider
     (e.g. "NER configuration (prose and/or LLM providers)" → "NER configuration (LLM
     provider)"; "Rule / POS-based prose NER settings" → remove).
2. `crates/config/tests/preset.rs`: delete the test block (≈ lines 90-96) that asserts
   `cfg.ingestion.ner.prose` defaults.
3. YAML: delete the `prose:` subsection under `ingestion.ner` in
   `workspace/configs/config.default.yaml`, `workspace/configs/config.demo.yaml`, and the
   test fixture `crates/config/tests/data/config.default.yaml`.
4. `crates/config/src/ontology.rs`: **NO change** — `NerStage::Prose` stays (deferred error).

## Contract impact

The frozen `config-format` spec mentions `ner.prose/llm`. The delta spec in this change
updates it to `ner.llm`. Backward compatibility with Go oracle configs is preserved: serde
ignores unknown keys, so a Go `config.default.yaml` containing `ner.prose:` still parses
without error.

## Risks

- **None functional**: `prose` NER was already unimplemented (deferred), so removal loses no
  working feature.
- **No new dependencies**; the frozen stack is untouched.
- `NerConfig` does **not** use `deny_unknown_fields`, so any leftover `prose:` in a YAML
  would not break parsing — but we remove it for cleanliness and contract accuracy.

## Future work (separate change, per user 2026-08-29)

Add a real NER provider via `gliner2-rs` or `redact-ner` (ONNX / `ort`). This requires
un-deferring ONNX bindings for NER (frozen-stack decision) and registering the model in
`onnx.yaml`. Tracked separately; not part of this change.
