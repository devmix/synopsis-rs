# Tasks: post-migration-lance-removal

Order: 1.1 → 1.2 → 1.3 → 1.4 → 1.5. Each task leaves the workspace compiling with
all gates green. Oracle reference: N/A for every task (lance is a Rust-side engine;
the Go oracle's vec0 index was never ported — no parity surface).

- [x] 1.1 Remove LanceEngine and all engine features from the `vectors` crate
- [x] 1.2 Remove engine-feature plumbing from `cli` and the workspace root
- [x] 1.3 Remove the A/B-benchmark transitional code from `parity-harness`
- [x] 1.4 Config validation: `vectors.engine` rejects `"lance"`
- [x] 1.5 Docs, specs context, and CI sweep

---

## Task 1.1 — Remove LanceEngine and all engine features from the `vectors` crate

**Goal.** Make `UsearchEngine` the single, unconditional ANN engine: delete
`LanceEngine`, all `engine-*` Cargo features, the `lancedb`/`futures` dependencies,
the lance-gated test targets, and the IVF-only config fields.

**Read first.** `openspec/changes/post-migration-lance-removal/design.md` (D1, D3,
D4), `crates/vectors/src/lib.rs` (feature gates, `Engine` enum, factory,
`VectorIndexConfig`), `crates/vectors/src/engine.rs` (LanceEngine),
`crates/vectors/src/error.rs`, `crates/vectors/src/usearch/mod.rs` (module docs
mention the IVF fields).

**Scope of files (exact).**
- `crates/vectors/Cargo.toml` — delete the `[features]` section, the `lancedb` and
  `futures` dependencies, the `[[test]] integration_gates` and `[[test]]
  full_benchmark` targets (keep the `persistence_integration` target but drop its
  `required-features`). Keep `usearch`, `cxx`, `rayon`, `rusqlite`, `libsqlite3-sys`
  (now non-optional), `cxx-build` build-dep, dev-deps.
- `crates/vectors/src/engine.rs` — delete the file (it is the LanceEngine).
- `crates/vectors/src/lib.rs` — remove the `engine` module wiring, `ENGINE_LANCE`
  const, `Engine::Lance` variant and all `#[cfg(feature = ...)]` gates; make the
  usearch module unconditional; remove `num_partitions`/`nprobes` from
  `VectorIndexConfig` (fields, constructor, validation, doc comments); factory:
  `""` or `"usearch"` → `UsearchEngine`, `"lance"` → new
  `VectorError::EngineRemoved` (message: `the "lance" engine was removed; the only
  engine is "usearch"`), other values → existing unknown-engine error; update the
  factory tests (delete the lance factory test, add a `"lance"` → `EngineRemoved`
  test and keep the usearch/unknown tests).
- `crates/vectors/src/error.rs` — drop lance-specific error variants (if any), add
  `EngineRemoved`.
- `crates/vectors/src/usearch/mod.rs` — update the module doc line about "the IVF
  fields `num_partitions`/`nprobes` do not apply" (the fields no longer exist).
- DELETE `crates/vectors/tests/integration_gates.rs` and
  `crates/vectors/tests/full_benchmark.rs` (both `required-features =
  ["engine-lance"]`).

**Out of scope.** `crates/cli/**`, `crates/parity-harness/**`, `Cargo.toml`
(workspace), docs/specs — those are tasks 1.2–1.5. (The workspace will not compile
until 1.2 fixes the cli call sites — acceptable; gates below run with `-p vectors`
plus a workspace `cargo check` expected to fail ONLY in cli, which 1.2 fixes. If a
workspace-wide `cargo test` is required green before commit, coordinate: this task
may include the two-line `crates/cli/src/serve/bootstrap.rs` `VectorIndexConfig`
constructor call-site fix (lines passing `tuning.num_partitions, tuning.nprobes`) to
keep the whole workspace green — prefer that.)

**Dependencies.** None.

**Acceptance criteria (machine-checked).**
1. `rg -i "lance" crates/vectors` → matches ONLY in the `EngineRemoved` error
   message + its doc comment, the factory `"lance"` detection, and the required
   `"lance"` → `EngineRemoved` test (no other matches; no engine module, no
   feature-gated test files).
2. `rg -n "num_partitions|nprobes" crates/vectors` → zero matches.
3. `cargo test -p vectors` → green (includes `persistence_integration` without any
   feature flag).
4. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` → clean.
5. `cargo check --workspace` → green (with the bootstrap call-site fix if included).
6. `cargo tree -p vectors -i lancedb` → lancedb absent from the graph.

**Oracle reference.** N/A (see change header).

**Revision history.**
- Rev 1 (after rust-reviewer `request_changes`): (a) AC #1 corrected — the 4
  intentional `"lance"` string matches (error message + doc, factory detection,
  required test) are mandated by the task body and are the allowed residual, not a
  criterion failure; (b) scope deviation accepted and documented: the implementer
  also removed the dangling `vectors/engine-*` feature references from
  `crates/cli/Cargo.toml` and `crates/parity-harness/Cargo.toml` (nominally 1.2/1.3
  scope) because Cargo resolves the whole workspace — without it even
  `cargo test -p vectors` failed to resolve; tasks 1.2/1.3 now verify-and-skip the
  already-done Cargo.toml portions; (c) reviewer nit: the leftover
  `default-features = false` on the `vectors` dep in `crates/parity-harness/Cargo.toml`
  is dead config (no features exist) — fixed in this revision.

---

## Task 1.2 — Remove engine-feature plumbing from `cli` and the workspace root

**Goal.** Delete the `engine-lance`/`engine-usearch` feature plumbing from `cli` and
the workspace root manifest; fix lance leftovers in cli code/tests.

**Read first.** `design.md` (D1), `crates/cli/Cargo.toml`, `Cargo.toml` (workspace
`[workspace.dependencies]` and member list), `crates/cli/src/serve/bootstrap.rs`
(engine selection + the `unwrap_or("lance")` log fallback), `crates/cli/src/db.rs`
(test fixture creating a `vectors/lance` dir), `crates/cli/src/serve/server.rs`
(module docs + the "other engine's subdirectory (lance layout)" test),
`crates/cli/src/error.rs` (doc comment mentioning LanceDB).

**Scope of files (exact).**
- `Cargo.toml` (workspace) — remove the `lancedb` workspace dependency; revert
  `vectors = { path = "crates/vectors", default-features = false }` to
  `vectors = { path = "crates/vectors" }` (features no longer exist).
- `crates/cli/Cargo.toml` — delete the `engine-lance`/`engine-usearch` features and
  the `default = ["engine-usearch"]` line; the `vectors` dev-dependency
  (`features = ["engine-lance"]`) becomes a plain workspace dep (or is removed if
  the tests that used it are deleted — check first). NOTE (rev 1): the feature
  removal was already done during task 1.1 (Cargo workspace resolution required
  it) — verify and skip if absent.
- `crates/cli/src/serve/bootstrap.rs` — update the engine-selection comments (the
  field is now `"usearch"` only / absent); the log line
  `engine_name.as_deref().unwrap_or("lance")` → `unwrap_or("usearch")`.
- `crates/cli/src/db.rs` — in the test that seeds a dataset state dir: drop the
  `vectors/lance` fixture creation and its assertions; keep the usearch side.
- `crates/cli/src/serve/server.rs` — update module doc comments (Lance → vector
  engine / usearch); in the test seeding "the other engine's subdirectory (lance
  layout)", drop the lance leg (keep the test's purpose: clearing removes the whole
  state dir incl. the usearch index).
- `crates/cli/src/error.rs` — doc comment: "LanceDB engine" → "vector engine".

**Out of scope.** `crates/vectors/**` (task 1.1), `crates/parity-harness/**`
(task 1.3), docs/specs (task 1.5).

**Dependencies.** Task 1.1 (the vectors crate must already be feature-free; the
workspace dep revert assumes no `engine-*` features remain).

**Acceptance criteria (machine-checked).**
1. `rg -i "lance" crates/cli Cargo.toml` → zero matches.
2. `rg -n "engine-lance|engine-usearch" crates/` → zero matches outside
   `crates/parity-harness` (task 1.3 removes those).
3. `cargo test -p cli` → green.
4. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` → clean.
5. `cargo check --workspace` → green.
6. `cargo build --release` → produces `target/release/synopsis` (default features
   include the usearch engine: `cargo run -- --version` works).

**Oracle reference.** N/A (see change header).

---

## Task 1.3 — Remove the A/B-benchmark transitional code from `parity-harness`

**Goal.** Delete the lance-vs-usearch comparison machinery from `parity-harness`;
keep the permanent parity tooling (fixtures, mcp_client, metrics, diff).

**Read first.** `design.md` (D5), `crates/parity-harness/Cargo.toml`,
`crates/parity-harness/src/bench.rs`, `crates/parity-harness/tests/ab_benchmark_test.rs`,
`crates/parity-harness/src/fixtures.rs`, `crates/parity-harness/tests/parity_test.rs`.

**Scope of files (exact).**
- `crates/parity-harness/Cargo.toml` — delete the `engine-lance`/`engine-usearch`
  features; the `vectors` dev-dependency (currently
  `default-features = false, features = ["engine-lance"]`) becomes a plain workspace
  dep (or is removed if no remaining test needs it — check first). NOTE (rev 1):
  the feature removal was already done during task 1.1 (Cargo workspace resolution
  required it) and the dead `default-features = false` was removed in the 1.1
  revision — verify the line is a plain workspace dep and skip if so.
- `crates/parity-harness/tests/ab_benchmark_test.rs` — DELETE (the A/B engine
  comparison; its stated purpose was choosing the engine, which is done).
- `crates/parity-harness/src/bench.rs` — check for remaining consumers of
  `bench_engine`/`dir_size_bytes` (rg in the workspace). If none survive the
  `ab_benchmark_test.rs` deletion, DELETE the file and its `mod` declaration; if a
  surviving consumer exists, keep it and strip lance references.
- `crates/parity-harness/src/fixtures.rs` — remove lance references (engine
  selection / lance fixture handling); keep SYNX `vectors.bin` loading.
- `crates/parity-harness/tests/parity_test.rs` — remove `engine-lance` feature
  gates / lance branches; the fixture tool-response parity test must keep running
  with the default (usearch) engine.

**Out of scope.** `crates/vectors/**`, `crates/cli/**`, docs/specs.

**Dependencies.** Task 1.1 (vectors is feature-free); Task 1.2 not strictly required
but run after it to keep the workspace green between commits.

**Acceptance criteria (machine-checked).**
1. `rg -i "lance" crates/parity-harness` → zero matches.
2. `rg -n "engine-lance|engine-usearch" crates/` → zero matches workspace-wide.
3. `cargo test -p parity-harness` → green (parity fixture test + metrics/percentile
   tests still run).
4. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` → clean.
5. `cargo check --workspace` → green.

**Oracle reference.** N/A (see change header).

---

## Task 1.4 — Config validation: `vectors.engine` rejects `"lance"`

**Goal.** The config crate validates the `vectors.engine` field: absent or
`"usearch"` pass; `"lance"` is rejected with an explicit removal error; unknown
values are rejected (existing behavior).

**Read first.** `design.md` (D2), `openspec/changes/post-migration-lance-removal/specs/config-format/spec.md`,
`crates/config/src/preset.rs` (the `vectors` section struct + validation), existing
preset tests for the `vectors` section.

**Scope of files (exact).**
- `crates/config/src/preset.rs` — validation for the `engine` field:
  - `None` → Ok (default engine, usearch);
  - `Some("usearch")` → Ok;
  - `Some("lance")` → Err with a message containing: `the "lance" engine was
    removed; the only engine is "usearch"`;
  - `Some(other)` → Err (unknown-value error). Rev 1: the message MUST no longer
    list the removed engine — it becomes
    `vectors.engine must be "usearch", got {engine:?}` (the old
    `must be "lance" or "usearch"` wording is factually wrong after the removal;
    the spec delta only requires "an explicit parse/validation error" and does not
    pin the wording).
  Update struct/doc comments that mention the two-engine choice.
- `crates/config` tests — update/extend the `vectors`-section tests:
  - absent field → Ok;
  - `"usearch"` → Ok;
  - `"lance"` → Err, assert the removal message;
  - `"foo"` → Err.

**Out of scope.** `crates/vectors/**` factory (task 1.1 already mirrors this),
docs/specs (task 1.5).

**Dependencies.** None (config crate is independent); run after 1.1–1.3 to keep the
workspace green between commits.

**Acceptance criteria (machine-checked).**
1. The four test cases above exist and pass: `cargo test -p config` → green.
2. `rg -in "lance" crates/config/src` → matches only in the error message / test
   expectations (no engine wiring).
3. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` → clean.
4. `cargo check --workspace` → green.

**Oracle reference.** N/A (see change header).

**Revision history.**
- Rev 1: the task body's "keep wording" for the unknown-value error was an
  orchestrator error — the original message `must be "lance" or "usearch"`
  presents the removed engine as a valid option. Corrected above: the message
  becomes `vectors.engine must be "usearch", got {engine:?}`. The `"foo"` test
  only asserts the field name, so no test change is required.

---

## Task 1.5 — Docs, specs context, and CI sweep

**Goal.** Remove every remaining lance reference from active docs, the frozen-stack
config, ADR 0003 (status line only), and CI; delete the `ci/darwin-sdk` workaround.

**Read first.** `design.md` (D6, D7, D8), `docs/adr/0003-ann-engine.md` (top of the
file), `openspec/config.yaml` (frozen-stack bullet about lance), `.github/workflows/ci.yml`
(lines ~81–89), `ci/darwin-sdk/README.md`.

**Scope of files (exact).**
- `openspec/config.yaml` — replace the frozen-stack lance bullet with: usearch 2.x
  (C++11 HNSW core via cxx, disk-backed, scalar-quantized default bf16, WAL +
  segments per ADR 0004) — the sole ANN engine, replaces vec0 brute-force; vectors
  are rebuilt from chunk text. (The stale "usearch отклонён" phrasing disappears.)
- `docs/adr/0003-ann-engine.md` — add ONE status line near the top:
  `**Status:** Superseded by ADR 0004 (2026-08-31): the lance engine was removed;
  usearch is the sole ANN engine.` No other edits to the ADR body.
- `AGENTS.md` — the frozen-stack line "usearch or lance — ... engine chosen by
  benchmark in `native-seam-spikes`" → usearch-only wording (sole engine, ADR 0004).
- `README.md` — same line ("usearch or lance ANN index, disk-backed and quantized
  (engine decided by benchmark ...)") → usearch-only wording.
- `.github/workflows/ci.yml` — delete the `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS`
  env block and its comment (lines ~81–89); the `aarch64-apple-darwin` matrix leg
  stays.
- DELETE the `ci/darwin-sdk/` directory (README + stubs) — it exists only for the
  lance-arrow CoreFoundation link (design D6).
- Module doc comments (wording only, no code changes):
  - `crates/search/src/semantic.rs` (module doc: "re-architected for the
    lancedb-backed index" → usearch-backed);
  - `crates/search/tests/hybrid_integration.rs` (module doc: "mock-pattern stand-in
    for the LanceDB engine" → vector engine);
  - `crates/mcp/src/server.rs` (comment: "the Lance index" → "the vector index");
  - `crates/ingestion/src/ingester/mod.rs` (VectorSink doc: `vectors::LanceEngine`
    → the vector engine);
  - `crates/ingestion/Cargo.toml` (comment "without a LanceDB engine" → vector
    engine).

**Out of scope.** `openspec/specs/**` (synced from this change's delta specs at
archive time — do NOT hand-edit), `openspec/changes/archive/**`, `.archive/**`,
`docs/adr/spike-s3-results.md` (historical spike record),
`docs/port-verification-report.md` (dated report).

**Dependencies.** Tasks 1.1–1.4 (the code must already be lance-free so the sweep
grep is meaningful).

**Acceptance criteria (machine-checked).**
1. `rg -i "lance" --hidden -g '!target' -g '!.git' -g '!.archive/**' -g
   '!openspec/changes/archive/**' -g '!openspec/changes/post-migration-lance-removal/**'
   -g '!docs/adr/0003-ann-engine.md' -g '!docs/adr/spike-s3-results.md' -g
   '!docs/port-verification-report.md'` → zero matches. (Allowed residual: the
   config-crate error message + its tests, which name the removed engine by design —
   if the grep catches them, that is expected and documented here.)
2. `ci/darwin-sdk/` directory does not exist; `git status` clean.
3. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
   → all green.
4. `openspec validate post-migration-lance-removal` → passes (if the CLI is
   available; otherwise state so in the report).

**Oracle reference.** N/A (see change header).

**Revision history.**
- Rev 1 (orchestrator-caused, before review): two defects in the
  `openspec/config.yaml` bullet — (a) 3-space indent instead of 2 (sibling
  bullets), (b) "the sole ANN engine" was translated to Russian although the task
  body and the user's standing mandate specify English for produced text. Both
  fixed; the preserved pre-existing Russian sentence ("Векторы НЕ читаются из
  старого vec0...") stays as-is. AC #1 literal "zero matches" deviation
  documented by the implementer and accepted: substring false-positives
  ("bal**ance**d"/"bal**ance**r" in out-of-scope mediawiki files), the 4 mandated
  residuals in `crates/vectors` (task 1.1) + 4 in `crates/config` (documented in
  this task's AC #1), and `openspec/specs/**` (synced from the delta specs at
  archive time — explicitly out of scope).
