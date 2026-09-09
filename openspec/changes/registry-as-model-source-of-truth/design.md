# Design: registry-as-model-source-of-truth

## Context

Two files currently describe the same model metadata. `onnx.yaml` (the
registry, `config::onnx::OnnxConfig`) carries per-model `vector_dim` and the
`files[]` list (model + tokenizer) for every entry. The main preset repeats
`embeddings.local.vector_dim` and offers `model_path` / `tokenizer_path`
overrides of the file locations. The resolution code in
`crates/embedding/src/lib.rs` treats the config value as primary and the
registry value as a cross-check (`resolve_from_registry` errors on
mismatch), and `resolve_explicit_path` bypasses the registry entirely.

Consumers of the dimension:

- `Config::vector_dim()` (`crates/config/src/preset.rs`) — mode-based
  accessor; consumed by `vectors_index_config`
  (`crates/cli/src/serve/bootstrap.rs`) for the ANN index and by the startup
  health check (`crates/cli/src/serve/health.rs`).
- `OnnxProvider` (`crates/embedding/src/provider.rs`) — CLS pooling truncates
  the model output to the resolved dimension (`embedding_row` accepts an
  output *wider* than the dimension), so the session output shape is NOT a
  reliable dimension source.
- `model benchmark` (`crates/cli/src/model.rs`) — already synthesizes
  `vector_dim` from the registry entry and builds the explicit path from
  `manager.model_dir(name)`, i.e. it already targets registry models.

`embeddings.mode: api` is unsupported by this build (bootstrap returns
`CliError::Unsupported`); `embeddings.api.vector_dim` is validated but never
consumed.

## Goals / Non-Goals

**Goals:**

- The `onnx.yaml` registry is the single source of truth for model metadata:
  dimension, model file location, tokenizer location.
- `embeddings.local` reduces to one optional field, `model_name`.
- The dimension flows from one place to the provider and the ANN index, so a
  config/registry mismatch becomes unrepresentable.

**Non-Goals:**

- No `api` mode implementation; no new `onnx.yaml` validation phase; no
  deprecation warning for removed keys; no inference-path changes; no index
  migration (dimension switches keep using `auto_rebuild_vectors`). See
  proposal.md.

## Decisions

### D1 — Registry is the sole source of model metadata

The provider's dimension, model file, and tokenizer are resolved from the
registry entry for the selected model name. `vector_dim`, `model_path`, and
`tokenizer_path` are removed from the config schema.

- **Alternative A (optional override):** keep `vector_dim` as an optional
  override that must match the registry. Rejected: it preserves the redundant
  value and the drift class this change removes; no practical override exists
  (the dimension is a property of the model, not of the deployment).
- **Alternative B (derive from the ONNX session output shape):** rejected —
  the provider's CLS pooling deliberately truncates an output that may be
  *wider* than the embedding dimension, so the shape is not the dimension.
- **Alternative C (registry for dim, keep path overrides):** rejected — the
  path overrides exist only to serve models outside the registry; keeping
  them keeps a second source of truth for file locations. Custom models now
  get a registry entry (a few YAML lines) instead of a config escape hatch.
  This is the deliberate cost of the simplification (human decision).

### D2 — Removal is breaking and immediate; no deprecation window

The frozen `config-format` contract is changed: fields are removed from
`embeddings.local` / `embeddings.api`. Justification: personal-use project,
the presets ship with the binary and are updated in this same change, and the
removal is a relaxation — unknown keys already do not break startup, so an
old config keeps working (stale values silently ignored). A deprecation
warning would require tracking unknown keys in sections that are parsed with
`#[serde(default)]` and no deny/unknown capture; not worth it for a
single-user config file. **This is the explicit frozen-contract decision
required by the project rules.**

### D3 — The explicit-`model_path` flow is deleted, not re-pointed

`resolve_explicit_path` and the `model_path` branch of `resolve_model`
(`crates/embedding/src/lib.rs`) are removed; the registry flow is the only
path. The `DEFAULT_VECTOR_DIM = 1024` fallback (`positive_dim`) dies with it —
a missing dimension is now always a configuration error, never a silent
default. `model benchmark` switches to the registry flow by name; its
`is_installed` gate stays, so `ensure_model` finds the model installed and
performs no download (behavior preserved: benchmarking a non-installed model
still errors with "run 'synopsis model download' first").

### D4 — Dimension resolution lives in the `config` crate

`Config::vector_dim()` (a mode-based accessor over the removed field) is
replaced by a resolver that takes the registry: a function on `Config`
accepting `&OnnxConfig` (e.g. `vector_dim(&self, onnx: &OnnxConfig) -> i32`)
that, for local mode, resolves the model name (empty → `models.default`),
looks up the entry, and returns its `vector_dim` (≤ 0 → validation error
naming the model). The `config` crate is the base tier and already owns both
`preset.rs` and `onnx.rs`, so no new dependency direction is introduced.
The `embedding` crate resolves the same value internally for the provider
(it already receives `&OnnxConfig`); both consumers read the same entry, so
the ANN index dimension and the provider dimension are structurally equal.

### D5 — Startup health check reduces to logging

The "config dim ≠ provider dim" check in `crates/cli/src/serve/health.rs`
compares two values that now derive from the same registry entry. The
comparison is replaced by an info log of the live provider dimension.

### D6 — Tests and fixtures

- `crates/config/tests/preset.rs`: drop `vector_dim` assertions and the
  zero-dim validation tests; add resolver tests (name → entry dim, empty name
  → default, missing entry / non-positive dim → error, removed keys ignored
  on parse).
- `crates/cli/tests/serve_bootstrap.rs`: drop the
  `ensure_model_skips_with_explicit_model_path` test and the
  `model_path`/`vector_dim` YAML fixtures; the registry-flow fixtures stay.
- Shipped presets lose the removed keys; the default preset's effective
  dimension is unchanged (384, now from the registry).

Reference fixtures/contracts for this design: `openspec/specs/config-format/spec.md`
(unknown-keys rule, onnx.yaml format), `openspec/specs/embedding/spec.md`
(model manager, provider), and the existing test suites listed above. No
recorded MCP/CLI fixtures are affected (no tool or CLI surface change).

## Risks / Trade-offs

- **Lost escape hatch:** a custom ONNX model can no longer be pointed at via
  config; it needs a registry entry. Accepted (D1-C, human decision).
- **Silently ignored stale keys:** an old config with `vector_dim: 768`
  keeps running with the registry dimension and no warning. Accepted (D2);
  the shipped presets are updated in the same change.
- **Registry entry without `tokenizer.json`:** was already an error in the
  registry flow (`tokenizer.json not found in model directory`); now it is
  the only flow, so such an entry is simply unusable. No behavior regression
  for the three shipped entries (all carry the tokenizer).
- **`model benchmark` path:** relies on `ensure_model` being a no-op for
  installed models (verified in `ModelManager::ensure_model`); the
  `is_installed` gate keeps the command from downloading.
