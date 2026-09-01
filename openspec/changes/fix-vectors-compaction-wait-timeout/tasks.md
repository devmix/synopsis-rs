# Tasks: raise the compaction test poll timeout

Change: `fix-vectors-compaction-wait-timeout`

## Change header (read first)

- **Goal:** stop the compaction tests from flaking on the `wait_until` poll
  timeout when the background repack thread is CPU-starved under full-workspace
  load, by raising the bound from 10 s to 60 s.
- **File scope (only this file):** `crates/vectors/src/usearch/compaction.rs`.
  Do NOT touch other crates, the Go oracle (`../synopsis` is read-only), or any
  test's predicate/assertions (only the shared `wait_until` helper's timeout
  changes).
- **Context (read these first):**
  - This file, `compaction.rs` — the `wait_until` helper (`:349`, timeout at
    `:350`) and its three call sites (`:449`, `:513`, `:622`).
  - `openspec/changes/fix-vectors-compaction-wait-timeout/proposal.md`.
  - `openspec/changes/fix-vectors-compaction-wait-timeout/design.md` (D1, the
    60 s rationale).
  - `openspec/config.yaml`, `AGENTS.md` (gates).
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo check --workspace`; `cargo test --workspace`.
- **Test-count invariant:** the workspace total stays **1,474** (no tests added
  or removed).

- [x] **1.1** — Raise the `wait_until` timeout to 60 s via a named constant.

  **Goal:** give the background-repack poll enough headroom to survive
  load-induced thread starvation, without changing any logic or predicate.

  **File scope:** `crates/vectors/src/usearch/compaction.rs` only — the
  `wait_until` helper (`:349`) and (optionally) the module's constant area.

  **Change:**

  1. Add a module-level (or test-module-level) named constant near `wait_until`:
     ```rust
     /// Generous bound for the background-repack poll. The repack is tiny
     /// (finishes in milliseconds under normal scheduling); this ceiling only
     /// matters when the OS deschedules the background thread for many seconds
     /// under full-workspace parallel load. 10 s proved too tight on a 16 GB
     /// laptop; 60 s is 6x headroom.
     const REPACK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
     ```
     (`Duration` is already imported in the test module — `use std::time::{
     Duration, Instant };` at `:311` — so no new import is needed.)

  2. In `wait_until` (`:349`), replace the inline timeout:
     ```rust
     let deadline = Instant::now() + Duration::from_secs(10);
     ```
     with:
     ```rust
     let deadline = Instant::now() + REPACK_WAIT_TIMEOUT;
     ```

  **Notes / invariants:**
  - This is the ONLY change. Do NOT modify any test's predicate, assertion, or
    the `wait_until` loop body / poll cadence (10 ms sleep stays). Do NOT touch
    `maybe_compact`, `CompactionState::run`, the ADR 0004 §7 ordering, the crash
    matrix, or any other file.
  - The `:354` assertion message ("the compaction did not finish within the
    timeout") may stay as-is; it is a failure condition, not a value.
  - No test relies on the timeout firing, so raising the bound cannot regress a
    passing test.

  **Approach:** add the constant, swap the inline `Duration::from_secs(10)` for
  `REPACK_WAIT_TIMEOUT`, run the gates.

  **Acceptance (Критерии приёмки):**
  1. `cargo fmt --all --check` clean.
  2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
  3. `cargo check --workspace` clean.
  4. `cargo test -p vectors` green (all vectors tests, including the three
     compaction tests that use `wait_until`).
  5. `cargo test --workspace` green; **run it 5 consecutive times** and all five
     must pass.
  6. Workspace test count unchanged: 1,474 (report `cargo test --workspace --
     --list | grep -c ': test$'`).
  7. `git diff --name-status` shows only `crates/vectors/src/usearch/compaction.rs`
     modified (nothing under `../synopsis`, no new dependencies, no test
     predicates/assertions changed, no other files).

  **Oracle reference:** none — usearch is a new engine (design D5); the Go oracle
  has no compaction thread.
