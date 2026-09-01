# Tasks: close the single-flight redundant-repack race

Change: `fix-vectors-singleflight-redundant-repack`

## Change header (read first)

- **Goal:** make the usearch compaction single-flight guarantee ("exactly one
  repack") actually hold, so `concurrent_maybe_compact_runs_exactly_once` is
  deterministic and `cargo test --workspace` is no longer flaky for it.
- **File scope (only this file):** `crates/vectors/src/usearch/compaction.rs`.
  Do NOT touch other crates, the Go oracle (`../synopsis` is read-only), or any
  test file (the existing test is correct and must pass unchanged).
- **Context (read these first):**
  - This file, `compaction.rs` — especially `maybe_compact` (`:54`, the
    check-then-CAS), the background-thread spawn (`:94-98`), and
    `CompactionState::run` (`:120`, layout lock at `:121`, stale-cache clear at
    `:202`), and the test `concurrent_maybe_compact_runs_exactly_once` (`:567`).
  - `openspec/changes/fix-vectors-singleflight-redundant-repack/proposal.md`
    (root cause).
  - `openspec/changes/fix-vectors-singleflight-redundant-repack/design.md` (D1).
  - `openspec/config.yaml`, `AGENTS.md` (gates).
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo check --workspace`; `cargo test --workspace`.
- **Test-count invariant:** the workspace total stays **1,474** (no tests added
  or removed).

- [x] **1.1** — Add the single-flight double-check at the top of `run()`.

  **Goal:** re-validate the compaction trigger under the layout lock and bail if
  the work was already done by a prior repack, so a redundant second repack
  cannot run.

  **File scope:** `crates/vectors/src/usearch/compaction.rs` only — specifically
  `CompactionState::run` (`:120`).

  **Change:** at the very top of `run()`, immediately after the existing
  `let _layout = mutex_guard(&self.layout_lock);` (`:121`) and BEFORE the existing
  live-key snapshot (`let stale = self.stale.snapshot();` at `:126`), insert this
  self-contained guard block:

  ```rust
  // Double-check (ADR 0004 §7 single-flight): the trigger read the stale
  // fraction BEFORE acquiring the flag; a concurrent repack may have cleared it
  // since. Re-validate under the layout lock and bail if the work is already
  // done — otherwise a redundant second repack would move the same live keys to
  // yet newer ids.
  {
      let stale = self.stale.snapshot();
      let stale_total: usize = stale
          .iter()
          .filter(|(id, _)| **id > 0)
          .map(|(_, set)| set.len())
          .sum();
      let total_disk: usize = disk_segments_read_guard(&self.disk_segments)
          .iter()
          .map(|segment| segment.index.size())
          .sum();
      let threshold = self.usearch_config.compaction_stale_threshold;
      if total_disk == 0 || stale_total * 100 <= threshold as usize * total_disk {
          return Ok(());
      }
  }
  ```

  **Notes / invariants:**
  - This block is the SAME predicate `maybe_compact` already uses (`:57-70`);
    reuse the identical expression so the two checks cannot drift. `disk_segments_read_guard`,
    `self.stale`, `self.disk_segments`, and `self.usearch_config` are all in
    scope in `run()`. No new imports needed.
  - Do NOT change the existing body of `run()` (the live-key snapshot at `:126`
    onward, the swap, the WAL cleanup, the in-memory swap, the stale-cache clear
    at `:202`). Do NOT change `maybe_compact`, the spawn closure (`:94-98`), the
    ADR 0004 §7 ordering, or the crash matrix.
  - Do NOT modify the test `concurrent_maybe_compact_runs_exactly_once` (`:567`)
    or any other test — it is correct and must pass unchanged.
  - The bail returns `Ok(())`; the spawn closure still clears the `compacting`
    flag, so there is no stuck flag.

  **Approach:** insert the guard block exactly as shown; run the gates.

  **Acceptance (Критерии приёмки):**
  1. `cargo fmt --all --check` clean.
  2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
  3. `cargo check --workspace` clean.
  4. `cargo test -p vectors` green (all vectors tests, including
     `concurrent_maybe_compact_runs_exactly_once`).
  5. `cargo test --workspace` green; **run it 5 consecutive times** and all five
     must pass (the original failure mode was ~1-in-3 under full load — 5 clean
     runs is a stronger signal than the 3 used for the WAL fix).
  6. Additionally, run `cargo test -p vectors
     concurrent_maybe_compact_runs_exactly_once` 10 times in a loop; all 10 must
     pass (this test is the concurrency stress for the fix).
  7. Workspace test count unchanged: 1,474 (report `cargo test --workspace --
     --list | grep -c ': test$'`).
  8. `git diff --name-status` shows only `crates/vectors/src/usearch/compaction.rs`
     modified (nothing under `../synopsis`, no new dependencies, no test files, no
     other files).

  **Oracle reference:** none — usearch is a new engine (design D5); the Go oracle
  has no compaction thread.

  **Revision history:**
  - Rev 1 (2026-09-01): the task body's guard block referenced
    `self.usearch_config`, but `CompactionState` has no such field. Approved
    deviation: copy out the scalar `compaction_stale_threshold: u8` (consistent
    with the existing `max_segment_vectors` copy-out) instead of adding the whole
    `UsearchConfig`; the guard reads `self.compaction_stale_threshold`.
    Behavior-neutral, removes the `max_segment_vectors` redundancy the whole-config
    approach would have introduced.
