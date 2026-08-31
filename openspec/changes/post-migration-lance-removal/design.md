# Design: post-migration-lance-removal

## Context

Lance (LanceDB, IvfHnswSq) was the original ANN engine (ADR 0003); usearch was added as
a candidate (`add-usearch-ann-engine`) and won after the persistence rework (ADR 0004,
`usearch-wal-persistence`). The comparison machinery — dual Cargo features, A/B
benchmark, engine config field — was transitional by construction (see
`crates/vectors/Cargo.toml` comments: "Both may be enabled at once during the
comparison period"). The user decision 2026-08-31 closes the comparison: remove lance
completely.

Go oracle reference: none. Lance is a Rust-side engine; the oracle's vec0 index was
never ported (deliberate, design D5/D6 of the original migration). This change has no
oracle parity surface — acceptance is machine gates + the sweep greps below.

## Decisions

### D1: Delete all `engine-*` Cargo features (usearch is unconditional)

- **Decision:** the `vectors` crate has no features; `UsearchEngine` is the only engine,
  compiled unconditionally. `cli` and `parity-harness` lose their
  `engine-lance`/`engine-usearch` features and the `[[test]] required-features` gates;
  the workspace dep reverts from `vectors = { path = ..., default-features = false }`
  to `vectors = { path = "crates/vectors" }`.
- **Why:** a feature flag whose only remaining value enables the only existing engine is
  dead machinery (KISS). The comparison period that justified dual features is over.
- **Alternative rejected:** keep `engine-usearch` as a default feature — adds
  `--no-default-features` build modes that no consumer uses, and keeps
  `required-features` test gates for no reason.
- **Alternative rejected:** keep both features — contradicts the user decision.

### D2: `vectors.engine` config field stays; `"lance"` becomes an explicit error

- **Decision:** the `vectors:` preset section and its `engine` field remain
  (config-format stability). Accepted values: absent (default) or `"usearch"`.
  `"lance"` → validation error: `the "lance" engine was removed; the only engine is
  "usearch"`. Unknown values → error (unchanged behavior). The `vectors` factory
  mirrors this (defense in depth, it is a public API): `""`/`"usearch"` →
  `UsearchEngine`, `"lance"` → `VectorError::EngineRemoved`, other →
  `VectorError::UnknownEngine`.
- **Why:** existing config files that set `vectors.engine: usearch` keep working; a
  file that set `lance` gets a loud, actionable error instead of a silent engine swap.
- **Alternative rejected:** delete the field entirely — `unknown keys are ignored` in
  preset parsing, so `engine: lance` would be silently ignored: a removed engine
  silently "working" is the worst failure mode.

### D3: Engine path layout unchanged (`<vectors_path>/usearch`)

- **Decision:** the factory keeps creating the index under the engine-tagged
  subdirectory; with one engine the tag is always `usearch`.
- **Why:** existing datasets (e.g. the edtech demo) already have data under
  `vectors/usearch`; moving the layout would orphan working data for zero benefit.
- **Alternative rejected:** drop the tag (index directly in `<vectors_path>`) — breaks
  existing dataset directories.

### D4: IVF-only config fields (`num_partitions`, `nprobes`) are removed

- **Decision:** `VectorIndexConfig` loses `num_partitions` and `nprobes` (fields,
  constructor params, validation, docs); the `cli` bootstrap call site stops passing
  them.
- **Why:** both are IVF parameters that apply only to LanceDB's IvfHnswSq; the usearch
  module docs already state "the IVF fields `num_partitions`/`nprobes` do not apply to
  pure HNSW" (`crates/vectors/src/usearch/mod.rs`). They are part of "everything
  related to lance". They were never exposed in shipped presets (`configs/*.yaml` set
  no `vectors:` section), so no preset breaks.
- **Alternative rejected:** keep them as reserved fields — dead config is exactly what
  this change removes.

### D5: parity-harness loses only the A/B-benchmark transitional code

- **Decision:** delete `tests/ab_benchmark_test.rs` (the lance-vs-usearch comparison)
  and `src/bench.rs` if it has no remaining consumers; remove the
  `engine-lance`/`engine-usearch` features and the `vectors` dev-dep feature wiring.
  `fixtures.rs`, `mcp_client.rs`, `metrics.rs`, `diff.rs` stay.
- **Why:** the A/B benchmark existed to choose the engine (its own comments say so);
  the choice is made. The rest of the harness is the permanent parity-verification
  mechanism (fixture tool-response parity, recall/percentile gates) and stays.
- **Alternative rejected:** remove the whole parity-harness as "transitional" — it is
  outside the product dependency graph by design and is how machine parity is checked;
  the user's "remove transitional modules" refers to comparison machinery, not the
  verification tooling.

### D6: CI darwin-sdk workaround removed with lance

- **Decision:** delete the `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS` env block and
  its comment in `.github/workflows/ci.yml`, and delete the `ci/darwin-sdk/` directory.
- **Why:** the workaround exists solely because `lancedb → lance-arrow` (a cdylib)
  links `-framework CoreFoundation`, which the official Zig SDK lacks (ziglang/zig#1349;
  see `ci/darwin-sdk/README.md`). With lancedb gone, nothing in the dependency graph
  links macOS frameworks. The `aarch64-apple-darwin` matrix leg keeps building (it
  already built the usearch C++ core via cxx-build during the comparison period, when
  usearch was the default feature).
- **Risk:** if some other transitive dependency needs the stub, the darwin leg fails on
  push and the workaround can be re-added with a new justification. Acceptable — the
  matrix is the detection mechanism.

### D7: ADR 0003 gets a supersede status line only

- **Decision:** add one status line at the top of `docs/adr/0003-ann-engine.md`:
  "Superseded by ADR 0004 (2026-08-31): the lance engine was removed; usearch is the
  sole ANN engine." No other edit.
- **Why:** ADRs are immutable architectural history; the decision "choose lance" was
  correct at the time and its rationale (benchmarks, CGO-free alternative) remains
  informative. A status line preserves both the history and discoverability of the
  supersession.
- **Alternative rejected:** delete ADR 0003 — destroys the decision trail (why the
  stack moved vec0 → lance → usearch).

### D8: Frozen-stack entry in `openspec/config.yaml` updated (explicit contract change)

- **Decision:** the frozen-stack bullet "lance (IvfHnswSq, disk-backed/квантованный
  HNSW) — ... (ADR 0003; usearch отклонён)" is replaced with a usearch entry:
  usearch 2.x (C++11 HNSW core via cxx, disk-backed, scalar-quantized, WAL + segments
  per ADR 0004) — the sole ANN engine. (The current text's "usearch отклонён" is stale
  — usearch was adopted, not rejected.)
- **Justification (frozen-contract change, user-approved 2026-08-31):** the stack
  entry must reflect reality after the engine removal; the approval was given
  explicitly in the phase-0 planning.

## Task ordering

1.1 vectors crate → 1.2 cli + workspace → 1.3 parity-harness → 1.4 config crate →
1.5 docs/specs/CI sweep. Each task leaves the workspace compiling and all gates green.
1.5 runs last so the sweep grep sees a codebase already free of lance code.
