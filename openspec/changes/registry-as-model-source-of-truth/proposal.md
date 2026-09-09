# Proposal: registry-as-model-source-of-truth

## Why

The main config duplicates model metadata that the `onnx.yaml` registry already
carries: `embeddings.local.vector_dim` repeats the registry entry's
`vector_dim` (the code contains an explicit mismatch error for when they
disagree), and `model_path` / `tokenizer_path` override file locations that the
registry's `files[]` list already resolves. Two sources of truth for one value
invite drift; the registry should be the single source of model metadata, and
the config should only select a model by name.

## What Changes

- **BREAKING** (config format): remove `vector_dim` from `embeddings.local`
  and from `embeddings.api`. The provider's vector dimension is resolved from
  the registry entry for the selected model; a registry entry with
  `vector_dim <= 0` is a configuration error.
- **BREAKING** (config format): remove `model_path` and `tokenizer_path` from
  `embeddings.local`. The section reduces to a single field, `model_name`
  (optional; empty → `models.default` from `onnx.yaml`).
- The embedding crate loses the explicit-`model_path` resolution flow: the
  registry is the only way to resolve a model, its files, its tokenizer, and
  its dimension. The `DEFAULT_VECTOR_DIM = 1024` fallback is removed.
- Startup: the ANN index dimension and the provider dimension both derive from
  the same registry entry, so the startup "config dim ≠ provider dim" health
  check becomes a structural no-op and is reduced to logging.
- `model benchmark` keeps its installed-models-only gate and switches from the
  explicit-path flow to the registry flow by model name (no implicit download).
- Shipped presets (`workspace/configs/config.default.yaml`,
  `config.demo.yaml`) drop the removed keys; the effective dimension of the
  default preset is unchanged (bge-small-en-v1.5 → 384, now from the registry).
- Docs updated in the same change: `site/docs/reference/config-schema.mdx`,
  `site/docs/guides/configuration.mdx`.
- Backward compatibility: an old config that still sets a removed key keeps
  starting (unknown keys do not break startup, per the config-format
  contract); the stale value is silently ignored — no deprecation warning
  (human decision, personal-use project, presets ship with the binary).

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `config-format`: `embeddings.local` carries only `model_name`;
  `embeddings.api` no longer carries `vector_dim`; the `onnx.yaml` registry is
  declared the single source of truth for model dimension and file locations.
- `embedding`: the provider is built exclusively from a registry model name
  (dimension, model file, tokenizer all resolved from the registry entry);
  the explicit-`model_path` override flow is removed.

## Frozen contracts touched

- **Config format** (frozen): fields removed from `embeddings.local` /
  `embeddings.api`. This is a relaxation — unknown keys already do not break
  startup, so old configs keep working. Parity is confirmed by the config
  parse tests, the serve-bootstrap tests, and the shipped presets booting with
  the same effective dimension as before (384 for the default preset).
- **CLI surface**: unchanged — `model benchmark` keeps its
  installed-models-only behavior; no flags or subcommands added/removed.
- **MCP tools / data schema**: not touched.

## Non-goals

- No implementation of `embeddings.mode: api` (it remains unsupported by this
  build; only its `vector_dim` field is removed).
- No new validation phase for `onnx.yaml` — entries stay lazily validated at
  resolution time (a bad `vector_dim` on the selected entry fails at startup
  with a config error, as today).
- No deprecation warning for removed config keys.
- No change to the ONNX inference path itself (CLS pooling and truncation to
  the resolved dimension stay as-is).
- No migration of existing vector indexes: switching to a model with a
  different dimension still goes through the existing
  `auto_rebuild_vectors` path.
- No change to the `onnx.yaml` format itself.
