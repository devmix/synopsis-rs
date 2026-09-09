# Tasks: registry-as-model-source-of-truth

Order reflects dependencies: 1.1 → 1.2 → 1.3 → 1.4 → 1.5 → 1.6.

The code change is staged so every task leaves the workspace green and each
task's reading scope stays small (function-level pointers below are the
reading list — do not read whole files beyond them).

## 1.1 config: add the registry dimension resolver (additive)

**Goal.** Add `Config::resolved_vector_dim(&self, onnx: &OnnxConfig) ->
Result<i32, ConfigError>` to the `config` crate. It resolves the embedding
dimension from the `onnx.yaml` registry: local mode → model name
(`embeddings.local.model_name`, empty → `onnx.models.default`), entry lookup
via `OnnxConfig::model_for_name`, return the entry's `vector_dim`. Unknown
model, empty name with no registry default, or `vector_dim <= 0` →
`ConfigError::Validation` naming the model. Api mode → the existing
`api.vector_dim` field value (unchanged; api mode is unsupported by the
build). Unrecognized mode → `Ok(0)` (as the old accessor does). Nothing is
removed in this task — the old `vector_dim()` accessor and the struct fields
stay.

**Design references.** `design.md` D4; delta spec
`specs/config-format/spec.md` ("Embeddings model selection").

**File scope (exact).**

- `crates/config/src/preset.rs`
  - Reading list: the `LocalEmbedding` / `ApiEmbedding` / `EmbeddingsConfig`
    structs, `Config::validate`, the `Config::vector_dim` accessor, the
    module doc header.
  - Add the `resolved_vector_dim` method (documented — `missing_docs =
    deny`) near the existing `vector_dim` accessor. Import
    `crate::onnx::OnnxConfig` (same crate).
- `crates/config/tests/preset.rs`
  - Reading list: one existing test that builds a `Config` and one that
    builds an `OnnxConfig` fixture (or `crates/config/tests/onnx.rs` for the
    fixture style).
  - Add tests: name → entry dim; empty name → `models.default` entry dim;
    unknown model → `Err`; entry with `vector_dim <= 0` → `Err`; api mode →
    the `api.vector_dim` value.

**Dependencies.** None (first task).

**Acceptance criteria (machine-checked).**

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean; `cargo test` green.
- `cargo test -p config` includes the new resolver tests.
- The old `vector_dim()` accessor and the struct fields are untouched
  (`git diff` in `crates/config/src/preset.rs` is additive only).

## 1.2 cli: ANN index dimension from the registry resolver

**Goal.** `vectors_index_config` derives the index dimension from
`Config::resolved_vector_dim(config, onnx)` instead of `config.vector_dim()`.
The ANN index and (later, task 1.3) the provider both read the same registry
entry.

**Design references.** `design.md` D4; task 1.1.

**File scope (exact).**

- `crates/cli/src/serve/bootstrap.rs`
  - Reading list: `vectors_index_config`, `open_vectors_engine`, the
    `Bootstrap` struct (it has the `onnx: OnnxConfig` field).
  - `vectors_index_config(config: &Config)` →
    `vectors_index_config(config: &Config, onnx: &OnnxConfig)`; dim =
    `config.resolved_vector_dim(onnx)` (map `ConfigError` into
    `VectorsError::InvalidArgument`). Update the doc comment ("the embedding
    dimension is authoritative" → it comes from the registry entry).
  - `open_vectors_engine`: pass `&boot.onnx`.
- `crates/cli/src/serve/server.rs`
  - Reading list: `recreate_vectors_engine` only.
  - Pass `&boot.onnx` to `bootstrap::vectors_index_config`.
- `crates/cli/src/loadtest/mod.rs`
  - Reading list: `test_bootstrap` (builds a `Bootstrap` with
    `OnnxConfig::default()` and a 4-dim `FakeEmbed`).
  - Replace `onnx: OnnxConfig::default()` with a registry fixture:
    `models.default = "bge-m3-int8"` + one entry named `bge-m3-int8` with
    `vector_dim: 4` (matching the fake provider), so the index dim resolves.
- `crates/cli/tests/serve_bootstrap.rs`
  - Reading list: the `vectors_index_config` / `open_vectors_engine` test
    section (the "task 3.9" block) and any test helper that builds a
    `Bootstrap` and calls `open_vectors_engine`.
  - Pass an `OnnxConfig` fixture (entry with the dim the test expects) to
    `vectors_index_config`; make the test-bootstraps' `onnx` field carry the
    matching registry entry.
- `crates/cli/tests/serve_server.rs`
  - Reading list: the test bootstrap helper (`test_bootstrap_with_embed`).
  - Its `onnx: OnnxConfig::default()` fed `recreate_vectors_engine` — give it
    the same registry-entry fixture (added to the scope during review: the
    signature change made the fixture mandatory for that test).

**Dependencies.** Task 1.1.

**Acceptance criteria (machine-checked).**

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean; `cargo test` green.
- `rg -n "vectors_index_config\(&" crates/cli/` — every call site passes two
  arguments.
- The index dimension in the loadtest and the serve-bootstrap tests equals
  the registry fixture entry's `vector_dim`.

## 1.3 embedding: provider dimension from the registry entry

**Goal.** In the registry flow, the provider's vector dimension comes from
the registry entry's `vector_dim` (a non-positive value is an
`EmbeddingError::Config` naming the model) instead of the config field. The
config/registry mismatch check stays (the config field still exists until
task 1.5 — a disagreement is still an error, as today). The
explicit-`model_path` flow is untouched in this task.

**Design references.** `design.md` D1/D4; delta spec
`specs/embedding/spec.md` ("Provider model resolution").

**File scope (exact).**

- `crates/embedding/src/lib.rs`
  - Reading list: `resolve_from_registry`, `resolve_model`,
    `resolve_explicit_path` (for context only — do not change),
    `positive_dim` / `DEFAULT_VECTOR_DIM` (for context only), the
    `new_onnx_provider` doc comment, and the unit tests at the bottom of the
    file that call `resolve_model` / build `LocalEmbedding` fixtures
    (`local_cfg`).
  - `resolve_from_registry`: dim = the entry's `vector_dim`; `<= 0` →
    `EmbeddingError::Config` naming the model and the declared value. Keep
    the mismatch check against `positive_dim(cfg.vector_dim)` (the field
    still exists).
  - Doc comment: in the registry flow the provider dimension comes from the
    registry entry and must match the config's `vector_dim` (mismatch is a
    config error); the explicit-path flow stays config-driven (word it for
    this intermediate state).
  - Tests: add a registry-entry `vector_dim <= 0` → `Config` error test;
    keep the existing mismatch test green.

**Dependencies.** None (independent of 1.2; ordered after it).

**Acceptance criteria (machine-checked).**

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean; `cargo test` green.
- `cargo test -p embedding` includes the non-positive-registry-dim test.
- `resolve_explicit_path` and `positive_dim` are untouched
  (`git diff --stat` shows `crates/embedding/src/lib.rs` only).

## 1.4 cli: drop all uses of the removed config fields

**Goal.** Remove every use of `vector_dim` / `model_path` / `tokenizer_path`
from the `cli` crate. The fields still exist in the `config` crate (removed
in task 1.5) — literals simply omit them (`Default::default()` fills the
rest). The benchmark switches to the registry flow; the startup health check
reduces to logging (both dimensions now derive from the same registry
entry).

**Design references.** `design.md` D3/D5/D6; task 1.3 (the provider dim is
already the registry dim, so the benchmark's registry flow works).

**File scope (exact).**

- `crates/cli/src/model.rs`
  - Reading list: `benchmark`, `run_production_benchmark`.
  - `run_production_benchmark`: drop the `model_path` parameter; build
    `LocalEmbedding { model_name: model.name.clone(), ..Default::default() }`
    and call `new_onnx_provider` (registry flow; `ensure_model` is a no-op —
    the caller's `is_installed` gate already ran, so no download). Update
    the doc comment ("explicit model path, no auto-download" → registry
    flow, installed models only).
  - `benchmark`: drop the `let model_path = manager.model_dir(...)` line and
    the argument.
- `crates/cli/src/serve/health.rs`
  - Reading list: the embedding-provider health check block and the test
    module (`local_config`, `ConstProvider`, the health tests).
  - Replace the "provider dimension differs from the config" comparison with
    an info log of the live provider dimension (keep the other health
    components as-is).
  - Test helper `local_config(dim)` → `local_config()` (a local-mode config
    with `model_name: "test"`); update the call sites.
- `crates/cli/src/loadtest/mod.rs`
  - `test_bootstrap`: drop the `config.embeddings.local.vector_dim = 4;`
    line (the registry fixture from task 1.2 carries the dim).
- `crates/cli/src/serve/bootstrap.rs`
  - Reading list: the `ensure_model` step.
  - Remove the "explicit model path (skipping auto-download)" skip-branch and
    its doc line (pulled forward from task 1.5 during review: the rg gate
    forbids `local.model_path` in the cli crate; behavior unchanged except
    the log line).
- `crates/cli/tests/serve_server.rs`
  - Reading list: the `LocalEmbedding` test literal (near the mock provider
    impls).
  - Reduce the literal to `model_name` only.
- `crates/cli/tests/serve_bootstrap.rs`
  - Reading list: the `LocalEmbedding` struct literals and the YAML fixture
    strings that set `vector_dim` / `model_path` / `tokenizer_path`.
  - Drop those lines from literals and fixtures (the registry entries in the
    fixtures carry the dims; a YAML that still sets the removed keys parses
    fine, but the fixtures are cleaned anyway).

**Dependencies.** Tasks 1.1, 1.2, 1.3.

**Acceptance criteria (machine-checked).**

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean; `cargo test` green.
- `rg -n "local\.vector_dim|local\.model_path|local\.tokenizer_path|api\.vector_dim"
  crates/cli/` — no matches (the `EmbeddingProvider::vector_dim()` trait
  method calls stay — they are not config fields).
- `model benchmark` still errors for a non-installed model (the
  `is_installed` gate is untouched).

## 1.5 config + embedding: remove the fields and the legacy flow

**Goal.** Final removal: the config schema fields, their validation, the old
accessor, and the embedding crate's explicit-`model_path` flow with its
`DEFAULT_VECTOR_DIM` fallback. After this task the registry is the only
source of model metadata.

**Design references.** `design.md` D1/D2/D3; both delta specs in this
change.

**File scope (exact).**

- `crates/config/src/preset.rs`
  - Reading list: the `LocalEmbedding` / `ApiEmbedding` structs,
    `Config::validate`, the old `Config::vector_dim` accessor, the module
    doc header.
  - `LocalEmbedding`: remove `model_path`, `tokenizer_path`, `vector_dim` —
    one field left, `model_name`.
  - `ApiEmbedding`: remove `vector_dim`.
  - `Config::validate`: remove the two `vector_dim must be positive` rules
    and the local-mode "model_path or model_name is required" rule (an empty
    `model_name` selects the registry default).
  - Remove the old `vector_dim()` accessor; update the module doc header
    (it references the accessor).
- `crates/config/tests/preset.rs`
  - Reading list: the tests that assert on `embeddings.local.vector_dim` /
    `embeddings.api.vector_dim`, `validate_local_rejects_zero_vector_dim`,
    `vector_dim_follows_embeddings_mode`, and the local-mode validation
    case that sets `vector_dim` to satisfy the removed rule.
  - Delete/adjust those tests (the resolver tests from task 1.1 stay).
  - Add a parse test: a YAML preset that still sets `vector_dim` /
    `model_path` / `tokenizer_path` under `embeddings.local` parses
    successfully (unknown keys ignored — the "Removed keys ignored"
    scenario).
- `crates/embedding/src/lib.rs`
  - Reading list: `resolve_model`, `resolve_explicit_path`,
    `resolve_from_registry` (the mismatch check), `positive_dim`,
    `DEFAULT_VECTOR_DIM`, the crate header doc, the `new_onnx_provider` doc
    + doc-test example, the `local_cfg` test helper.
  - Delete `resolve_explicit_path`; `resolve_model` becomes the registry
    flow directly (no branch).
  - Delete the config/registry mismatch check (unrepresentable now).
  - Delete `positive_dim` and `DEFAULT_VECTOR_DIM`.
  - Docs: the crate header and `new_onnx_provider` no longer describe a
    `model_path` override or a config-driven dimension — the registry entry
    is the source; the doc-test example builds
    `LocalEmbedding { model_name: ... }` only.
  - `local_cfg` test helper: drop the `vector_dim` parameter.
- `crates/cli/tests/serve_bootstrap.rs`
  - Reading list: the `write_config` YAML fixture (task 1.4 left its
    `vector_dim` keys in place with a doc comment because `Config::validate`
    still required them) and the `#[ignore]`d e2e test
    `bootstrap_full_with_pre_installed_model`.
  - Now that the validation rules are gone, drop the `vector_dim` keys from
    the `write_config` fixture and its doc comment. Verify the ignored e2e
    test still compiles and its YAML is clean (do not require running the
    ignored test — it is ignored for environment reasons).
- `crates/cli/src/model.rs`, `crates/cli/src/serve/health.rs`,
  `crates/cli/tests/serve_server.rs`
  - With `LocalEmbedding` reduced to one field, `..Default::default()` in
    the literals becomes `clippy::needless_update` (warn-level, gate runs
    `-D warnings`) — drop the `..Default::default()` from those literals
    (added to the scope during review; one-line mechanical fixes).

**Dependencies.** Tasks 1.1–1.4 (no user of the fields remains).

**Acceptance criteria (machine-checked).**

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean; `cargo test` green (full workspace).
- `rg -n "pub vector_dim|local\.vector_dim|api\.vector_dim|\.model_path|\.tokenizer_path"
  crates/config/src/preset.rs` — no matches (the `resolved_vector_dim`
  resolver and the registry `ModelInfo.vector_dim` field access remain —
  they are not config fields).
- `rg -n "resolve_explicit_path|DEFAULT_VECTOR_DIM|positive_dim"
  crates/embedding/src/` — no matches.
- `rg -n "cfg\.model_path|cfg\.tokenizer_path" crates/` — no matches.
- `LocalEmbedding` has exactly one field (`model_name`).
- The `write_config` fixture in `crates/cli/tests/serve_bootstrap.rs`
  contains no `vector_dim` key.

## 1.6 Presets and documentation

**Goal.** Update the shipped presets and all documentation to the new
config shape: `embeddings.local` carries only `model_name`; dimension and
file locations come from the `onnx.yaml` registry. The default preset's
effective dimension is unchanged (bge-small-en-v1.5 → 384, now from the
registry).

**Design references.** `design.md` D2/D6; `specs/config-format/spec.md`
delta of this change (the "Removed keys ignored" scenario).

**File scope (exact).**

- `workspace/configs/config.default.yaml`
  - `embeddings.local`: keep only `model_name`; drop `vector_dim` and the
    commented `model_path` line.
  - `embeddings.api`: drop `vector_dim`.
  - Rewrite the "Local dev override … vector_dim 384 … deliberate local
    deviation" comment: the deviation is now just the model choice
    (bge-small-en-v1.5 instead of the frozen bge-m3-int8); the dimension
    (384) comes from the registry entry.
- `workspace/configs/config.demo.yaml`
  - Same field removals (`vector_dim: 384` local, `vector_dim: 3072` api,
    commented `model_path`).
- `workspace/configs/README.md`
  - Remove references to `vector_dim` / `model_path` / `tokenizer_path` in
    the embeddings section; state that the registry is the source of the
    model dimension.
- `site/docs/reference/config-schema.mdx`
  - Remove the `embeddings.local.vector_dim`, `embeddings.local.model_path`,
    `embeddings.local.tokenizer_path`, `embeddings.api.vector_dim` rows;
    adjust the `model_name` row (empty → registry default; no path
    fallback).
- `site/docs/guides/configuration.mdx`
  - The example block (drop `model_path` / `vector_dim` lines), the field
    table rows (drop the four removed fields; `model_name` row: empty →
    `models.default` from onnx.yaml), the "Shipped-preset deviation" note
    (dimension now from the registry), and the "Switching to a model with a
    different `vector_dim`" sentence (reword: switching to a model with a
    different dimension — the registry entry's `vector_dim`).
- `site/docs/guides/model-management.mdx`
  - Remove/adjust any `model_path` / `vector_dim` config references; the
    registry (`onnx.yaml`) is where a custom model's dimension and files
    are declared.
- `site/docs/guides/troubleshooting.mdx`
  - Adjust the dimension-mismatch guidance: the dimension no longer comes
    from the main config; a mismatch means the registry entry's
    `vector_dim` differs from the stored index (rebuild via
    `auto_rebuild_vectors` / the documented path).

**Dependencies.** Task 1.5 (docs describe the final code behavior).

**Acceptance criteria (machine-checked).**

- `rg -n "vector_dim|model_path|tokenizer_path"
  workspace/configs/config.default.yaml workspace/configs/config.demo.yaml`
  — no matches.
- `rg -n "embeddings\.(local|api)\.(vector_dim|model_path|tokenizer_path)"
  site/docs/` — no matches.
- `cargo test` — still green (no code changes in this task).
- The default preset still names `bge-small-en-v1.5` and the registry
  (`workspace/configs/onnx.yaml`) entry for it declares `vector_dim: 384`.

## 1.7 config: drop the pre-registry model-name fallback from apply_defaults

**Goal.** `apply_defaults` still fills an empty `embeddings.local.model_name`
with the hardcoded `"bge-m3-int8"` (a pre-registry leftover), which shadows
the spec contract "empty `model_name` → `models.default` from onnx.yaml":
in production the resolver's `models.default` branch is unreachable because
`apply_defaults` runs first. Remove the fallback so an empty name stays
empty and the registry default (shipped: `bge-small-en-v1.5`) is selected at
resolution time, exactly as the config-format spec delta requires. This is
the user-approved Option A (2026-09-09): the registry is the single source
of truth, including the default model.

**File scope.**

- `crates/config/src/preset.rs`
  - Reading list: the module-level doc comment (top of file — the line that
    lists `no local model set → "bge-m3-int8"` as a semantic rule),
    `Config::apply_defaults` (doc comment bullet "the local model fallback —
    `embeddings.local.model_name` becomes `"bge-m3-int8"` in **both** modes,
    applied regardless of `mode`" and the `// Local embedding (this fallback
    applies in both modes)` block), and `Config::validate` doc (the
    "empty `model_name`" sentence — verify it stays accurate).
  - Remove the `if local.model_name.is_empty() { local.model_name =
    "bge-m3-int8"… }` block (and its section comment) from `apply_defaults`.
    An empty `model_name` now survives defaulting and is resolved via
    `models.default` in `resolved_vector_dim` / the embedding provider /
    `ModelManager::ensure_model` (all three already handle the empty name).
    Update the doc comments that describe the removed fallback (module docs,
    `apply_defaults` docs) so no line still claims the fallback exists.
- `crates/config/tests/preset.rs`
  - Reading list: the survival test asserting the fixture's
    `bge-small-en-v1.5` (comment "not replaced by bge-m3-int8"), the
    `Config::default()` local-validation test whose comment says the name is
    "filled by apply_defaults", the big defaults test asserting
    `cfg.embeddings.local.model_name == "bge-m3-int8"` after
    `apply_defaults`, and the `custom-model` survival assertion.
  - Update the big defaults test: after `apply_defaults` an empty
    `model_name` stays `""` (comment: the registry `models.default` is
    applied at resolution time, not at defaulting time).
  - Fix the stale comments (the "filled by apply_defaults" comment must say
    the empty name is resolved against `models.default` at resolution time;
    the "not replaced by bge-m3-int8" comments can drop the legacy name).
  - Add one test: a config with an empty `model_name` that passed through
    `apply_defaults` resolves its dimension via `resolved_vector_dim` to the
    registry `models.default` entry's `vector_dim` (ties defaulting +
    resolver together end to end, no ONNX runtime needed).

**Dependencies.** Tasks 1.1–1.6 (the resolver, the empty-name branch, and
the docs describing "empty → `models.default`" already exist).

**Acceptance criteria (machine-checked).**

- `rg -n "bge-m3-int8" crates/config/src/preset.rs` — the only allowed
  match is the `model_name` field doc example (e.g. `"bge-m3-int8"`); no
  match in `apply_defaults` or the module docs (the hardcoded fallback and
  its doc mentions are gone).
- `rg -n "bge-m3-int8" crates/config/tests/preset.rs` — matches only in
  registry fixtures / explicit-name tests, never as an apply_defaults
  outcome (the big defaults test asserts `""`).
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` — all pass.
- The new defaulting+resolver test passes: empty `model_name` +
  `apply_defaults` + registry `models.default` → the default entry's
  `vector_dim`.

## 1.8 cli: drop the pre-registry model-name fallback from the ensure_model wrapper

**Goal.** `crates/cli/src/serve/bootstrap.rs` keeps
`const DEFAULT_MODEL_NAME: &str = "bge-m3-int8"`, and the cli-side
`ensure_model` wrapper substitutes it for an empty
`embeddings.local.model_name` before calling
`ModelManager::ensure_model` — shadowing the registry `models.default`
exactly the way the config-crate fallback removed in task 1.7 did
(`ModelManager::ensure_model` already resolves an empty name via
`self.default_name`, `crates/embedding/src/model.rs`). Remove the wrapper
fallback so the registry is the single source of truth for the default
model (user-approved Option A, 2026-09-09).

**File scope.**

- `crates/cli/src/serve/bootstrap.rs`
  - Reading list: the `DEFAULT_MODEL_NAME` constant (top of file) and
    `ensure_model` (the name-selection `if` and the `tracing::info!` line).
  - Delete the `DEFAULT_MODEL_NAME` constant. Pass the trimmed config name
    straight to `manager.ensure_model` (empty/whitespace-only → the manager
    resolves `models.default` itself). Log the name actually used: when the
    config name is empty, log the resolved registry default
    (`manager.default_model()`) so the log line stays meaningful.
- `crates/cli/tests/serve_bootstrap.rs`
  - Reading list: the `ensure_model` tests (api-mode no-op, unknown model,
    download failure, and any test asserting the empty-name fallback).
  - If a test asserts the old hardcoded fallback name for an empty config
    name, update it to assert the registry `models.default` name instead
    (the test fixtures already declare a `models.default` — reuse it).
    Adjust other `ensure_model` tests only if they rely on the removed
    constant.

**Dependencies.** Task 1.7 (the config crate no longer fills the name, so
the wrapper is now the only place an empty name could be shadowed).

**Acceptance criteria (machine-checked).**

- `rg -n "DEFAULT_MODEL_NAME" crates/` — no matches.
- `rg -n "bge-m3-int8" crates/cli/src/` — no matches outside test modules
  and fixtures (the production wrapper is clean).
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` — all pass, including the
  `ensure_model` tests.
