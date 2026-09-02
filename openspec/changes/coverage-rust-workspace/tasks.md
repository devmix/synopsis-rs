# Tasks — coverage-rust-workspace

**Change header (context for every fresh agent):**
- Workspace: synopsis-rs (Rust rewrite of the Go service "Synopsis"). Read
  `openspec/config.yaml` (frozen stack, execution model) and `AGENTS.md` (commands,
  gotchas, OpenSpec workflow) first.
- This is a **tooling** change (`skip_specs: true`) — it adds dev-only coverage
  measurement. It changes **no product behavior, no frozen contract, no test code, and no
  `Cargo.lock`**. The Go oracle (`../synopsis`) is **not** referenced (no behavior is
  ported or compared); do not read or modify it.
- Tool: **`cargo-llvm-cov` v0.9.0** (cargo-installed binary; install with
  `cargo install --locked cargo-llvm-cov` after `rustup component add llvm-tools-preview`).
  It is **not** a `Cargo.toml` dependency — never add it to any `Cargo.toml`/`Cargo.lock`.
- Strategy: **measure-first** — baseline report + documented targets only; **no coverage
  gates** (`--fail-under-*`) in this change (gating is Phase 3).
- Gates that must stay green after every task: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`,
  `cargo test --workspace`.
- Per-task diff ≤ ~500 lines; each task is self-contained (a fresh agent with no prior
  context can complete it from this body alone).

---

## 1. CI coverage job

- [x] 1.1 Add a `coverage` job to `.github/workflows/ci.yml`

**Goal.** Add a `coverage` job that runs **in parallel** to the existing `checks` and
`cross-builds` jobs (not a dependency of either) and produces an `lcov.info` artifact.

**File scope.** `.github/workflows/ci.yml` only. Do not modify any other workflow, any
`Cargo.toml`, or any source file.

**Dependencies.** None.

**Approach.** Add a `coverage` job that:
1. Checks out the repo and sets up the **pinned** toolchain via `dtolnay/rust-toolchain`
   using the SAME revision the existing jobs use (it must match `rust-toolchain.toml` =
   1.96.0 — a mismatch makes every cargo command fail), adding the
   `components: llvm-tools-preview`.
2. Installs the tool via `taiki-e/install-action@cargo-llvm-cov` (pin the version the
   existing `cargo install --locked cargo-zigbuild` step already uses as the pattern for
   how dev tools are installed here).
3. Runs `cargo llvm-cov --workspace --lcov --output-path lcov.info`.
4. Uploads `lcov.info` with `actions/upload-artifact`.

**Acceptance criteria.**
1. A `coverage` job exists and is **not** in the `needs:` chain of `checks` (it runs
   parallel; a coverage failure must not block the core gate in Phase 2).
2. The job installs `llvm-tools-preview` + `cargo-llvm-cov` and runs
   `cargo llvm-cov --workspace --lcov --output-path lcov.info`, then uploads `lcov.info`.
3. **No** `--fail-under-*` flag anywhere (measure-first; no gates).
4. The existing `checks` and `cross-builds` jobs are byte-for-byte unchanged.
5. The workflow is valid YAML (verify with `actionlint` if available, else
   `ruby -ryaml -e "YAML.load_file('.github/workflows/ci.yml')"`, else a careful
   indentation review) — no syntax errors.
6. All four gates (fmt/clippy/check/test) still pass (this task changes no Rust code, so
   they must be trivially green).

---

## 2. Baseline coverage report

- [ ] 1.2 Generate the per-crate baseline coverage report → `baseline.md`

**Goal.** Record the per-crate line-coverage baseline in
`openspec/changes/coverage-rust-workspace/baseline.md`.

**File scope.** `openspec/changes/coverage-rust-workspace/baseline.md` (new). Do not
modify any source file, `Cargo.toml`, or `Cargo.lock`.

**Dependencies.** None (local measurement is independent of task 1.1). The tool must be
available: `rustup component add llvm-tools-preview` then
`cargo install --locked cargo-llvm-cov` (both are no-ops if already installed).

**Approach.** Run `cargo llvm-cov --workspace` and capture the per-file table. Aggregate
by crate (the file path contains `crates/<crate>/…`; the columns are `Lines` then
`Missed Lines`). Compute per-crate line coverage = (1 − missed/lines) × 100. Exclude the
`parity-harness` crate from the product-target table (it is test infrastructure) but list
it separately. Exclude any std-library / toolchain paths that appear (e.g. lines whose
path contains `rustlib` or `toolchain`). Record the overall total too.

**Acceptance criteria.**
1. `baseline.md` contains a per-crate table with **all 11 product crates** (db, graph,
   llm, utils, ingestion, search, mcp, vectors, config, embedding, cli) showing line
   count, missed lines, and coverage %; `parity-harness` is listed separately as
   excluded-from-targets.
2. The numbers match a fresh `cargo llvm-cov --workspace` run (the file states the exact
   command used and is reproducible).
3. The overall total line coverage is recorded (expected ≈ 93%).
4. The two lowest crates (expected: `cli` ~77%, `embedding` ~88%) are called out as the
   residual-gap focus for follow-up.
5. No source files modified; all four gates still pass.

---

## 3. Coverage developer guide

- [ ] 1.3 Write `COVERAGE.md` (local command + per-crate targets)

**Goal.** Document the coverage workflow for developers in a `COVERAGE.md` at the repo
root.

**File scope.** `COVERAGE.md` (new, repo root). Do not modify any source file,
`Cargo.toml`, or `Cargo.lock`.

**Dependencies.** None (the targets come from `design.md` D4; the current numbers from
task 1.2's `baseline.md`).

**Approach.** Write `COVERAGE.md` containing:
1. **Setup:** `rustup component add llvm-tools-preview` and
   `cargo install --locked cargo-llvm-cov` (note: dev-only tool, not in `Cargo.lock`).
2. **Local commands:** `cargo llvm-cov --workspace --html` (HTML report in
   `target/llvm-cov/html/`) and `cargo llvm-cov --workspace --text` (per-file table).
3. **Per-crate targets** table (copy from `design.md` D4): 70–80% core
   (`db`/`search`/`vectors`), 60–70% (`ingestion`/`mcp`/`config`/`graph`/`utils`/`llm`),
   50–60% integration-heavy (`cli`/`embedding`); `parity-harness` excluded.
4. **Status note:** measure-first — the current baseline (link to
   `openspec/changes/coverage-rust-workspace/baseline.md`) already meets every target;
   **gating (`--fail-under-*`) is deferred to Phase 3**, and targets may be raised once
   the gap list is in hand.

**Acceptance criteria.**
1. `COVERAGE.md` exists at the repo root and documents setup, the two local commands, the
   per-crate targets table, and the measure-first / no-gates note.
2. It references `baseline.md` for the current numbers.
3. It does **not** add the tool to any `Cargo.toml`.
4. No source files modified; all four gates still pass.

---

## 4. Final verification

- [ ] 1.4 Final verification: gates + CI validity + docs consistency

**Goal.** Machine-verify the whole change before archive.

**File scope.** Read-only (measurement only — no code edits). The only files this change
should have touched are `.github/workflows/ci.yml`,
`openspec/changes/coverage-rust-workspace/baseline.md`, and `COVERAGE.md`.

**Dependencies.** Tasks 1.1–1.3 complete.

**Acceptance criteria.**
1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo check --workspace`, `cargo test --workspace` all green (test count unchanged —
   this change adds no tests).
2. `.github/workflows/ci.yml` is valid YAML; the `coverage` job is present, parallel to
   `checks`, and has no `--fail-under-*` gate.
3. `baseline.md` and `COVERAGE.md` exist and are mutually consistent (same per-crate
   numbers; same targets).
4. No Rust source file, `Cargo.toml`, or `Cargo.lock` was modified (verify with
   `git status` / `git diff --name-only`); `../synopsis` untouched.
5. `git diff` of the change is scoped to exactly: `ci.yml`, `baseline.md`, `COVERAGE.md`
   (plus the openspec change artifacts).
