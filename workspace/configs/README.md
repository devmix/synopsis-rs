# Config presets

Default configuration presets for the `synopsis` binary, resolved relative to
the executable / CWD. Without `--config` the binary uses `config.default.yaml`
(preset `default`) and `onnx.yaml` (the ONNX model/runtime registry).

| File | Provenance | Notes |
|---|---|---|
| `onnx.yaml` | Carried over unchanged | `runtime.platforms[*]` + `models.entries[*]` (bge-m3-int8, bge-small-en-v1.5, paraphrase-multilingual-MiniLM-L12-v2). Unchanged. |
| `config.default.yaml` | The default preset | Deliberate deviation: `embeddings.local.model_name` is `bge-m3-int8` with `vector_dim: 1024` (Rust default per the frozen stack) instead of `bge-small-en-v1.5` (dim 384). All other sections follow the same structure. |
| `prompts/entity-linker/{system,user}.tmpl` | Rust minijinja templates | Minijinja templates with a fixed prompt text and data shape (field paths `entity_a.*` / `entity_b.*` instead of `.EntityA.*` / `.EntityB.*`; `join`/`truncate` are minijinja functions; context numbering uses a registered `enumerate` filter). Byte-identical to the embedded defaults in `crates/graph/src/templates/entity-linker/`. |
| `prompts/ner/{system,user}.tmpl` | Rust minijinja templates | Same minijinja engine as the entity-linker templates. Byte-identical to the embedded defaults in `crates/ingestion/src/ner/templates/`. |

## Prompt override semantics

Templates are loaded from `paths.prompts_path` (default `configs/prompts`): a
present `{system,user}.tmpl` file overrides the embedded default and is noted
in the run's notes; a missing file silently falls back to the embedded default.
Because the shipped files are byte-identical to the embedded defaults, the
template-source SHA-256 hashes (the decision-cache key) are the same either way.
