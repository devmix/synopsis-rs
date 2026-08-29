# Proposal: Remove unused `ingestion.ner.prose` config section

## Problem

`crates/config/src/preset.rs` defines `ProseNerConfig` and the `NerConfig.prose` field — a
config section for a "prose" (statistical / POS) NER provider that was **never implemented
in Rust**. It was deferred by human decision 2026-08-23: the Go-only `tsawler/prose` stack
(cgo bindings to go-prose models) has no maintained Rust equivalent, and the second ONNX
stack was rejected. The field is read by **no production code** — only by a single config
test. Three YAML presets (`config.default.yaml`, `config.demo.yaml`, and the
`tests/data/config.default.yaml` fixture) carry an empty `ingestion.ner.prose:` subsection.
This is dead config.

## Decision

Remove the dead `ingestion.ner.prose` config section entirely: the struct, the field, its
default, the test that asserts its defaults, and the `prose:` YAML subsections. Keep
`NerStage::Prose` in `ontology.rs` — it is a **separate concept** (the NER *stage method*
name in ontology XML) that yields `IngestionError::ProseNerDeferred`, a deliberate,
oracle-word-parity deferred error. It is not coupled to the config section and stays.

Update the frozen `config-format` spec to drop `ner.prose` from the preset section list
(`ner.prose/llm` → `ner.llm`).

## Non-goals

- Implementing a real NER provider. Adding `gliner2-rs` / `redact-ner` (both ONNX Runtime
  based, requiring un-deferring of the `ort` Rust bindings) is explicitly **deferred to a
  future, separate change** per user direction 2026-08-29.
- Removing `NerStage::Prose` (kept for the deferred-stage error path).

## Why not replace (this time)

The named analogs (`redact-ner`, `gliner*`) are ONNX Runtime based and require the `ort`
Rust bindings, which the frozen stack **defers** (only embeddings use ONNX today; the
embedding change explicitly deferred the bindings crate). Un-deferring for NER is a
frozen-stack decision and a larger feature. The user chose pure removal now, with the real
NER provider tracked as future work.
