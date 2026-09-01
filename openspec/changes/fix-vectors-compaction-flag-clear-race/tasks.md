# Tasks: close the compaction test flag-clear race

Change: `fix-vectors-compaction-flag-clear-race`

## Change header (read first)

- **Goal:** make the two racy compaction tests wait for the *final* observable
  state (the `compacting` flag cleared) before asserting on it, so they stop
  flaking under full-workspace load.
- **File scope (only this file):** `crates/vectors/src/usearch/compaction.rs` —
  the test module only. Do NOT touch other crates, the Go oracle
  (`../synopsis` is read-only), any production code, the repack ordering, the
  `wait_until` helper's timeout/cadence, or any other test.
- **Context (read these first):**
  - This file, `compaction.rs` — the `maybe_compact` trigger + thread (the flag
    clears at the thread's last step), `CompactionState::run` (the step 5/6/7
    ordering), the `wait_until` helper (`:356`), and the two affected tests
    (`compaction_merges_segments_with_monotonic_ids`,
    `compaction_slices_live_vectors_by_max_segment_vectors`).
  - `openspec/changes/fix-vectors-compaction-flag-clear-race/proposal.md`.
  - `openspec/changes/fix-vectors-compaction-flag-clear-race/design.md`
    (the ordering analysis, D1, the affected-test table).
  - `openspec/config.yaml`, `AGENTS.md` (gates).
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo check --workspace`; `cargo test --workspace`.
- **Test-count invariant:** the workspace total stays **1,474** (no tests added
  or removed).

- [ ] **1.1** — Extend the two racy `wait_until` predicates to wait for the flag.

  **Goal:** close the window where the predicate passes (WAL empty) but the
  `compacting` flag is still set, which makes the flag assertion flake under
  load.

  **File scope:** `crates/vectors/src/usearch/compaction.rs` only — the two
  `wait_until` predicates in the two named tests.

  **Change (exactly two predicates):**

  1. In `compaction_merges_segments_with_monotonic_ids`, the `wait_until`
     predicate (currently ends with `wal_rows(&conn).is_empty()` after the
     `ids.contains(&3)` swap check) becomes:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         if !(ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)) {
             return false;
         }
         // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
         // it too, or the WAL assertion below is racy under load.
         let conn = Connection::open(&db_path).unwrap();
         let wal_empty = wal_rows(&conn).is_empty();
         drop(conn);
         // The single-flight flag clears after the WAL cleanup + the in-memory
         // list swap (the repack thread's last step): wait for it too, or the
         // flag assertion below is racy under load.
         wal_empty && !engine.compacting.load(Ordering::SeqCst)
     });
     ```

  2. In `compaction_slices_live_vectors_by_max_segment_vectors`, the `wait_until`
     predicate (currently ends with `wal_rows(&conn).is_empty()` after the
     `ids.contains(&5)` swap check) becomes:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         if !(ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)) {
             return false;
         }
         // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
         // it too, or the WAL assertion below is racy under load.
         let conn = Connection::open(&db_path).unwrap();
         let wal_empty = wal_rows(&conn).is_empty();
         drop(conn);
         // The single-flight flag clears after the WAL cleanup + the in-memory
         // list swap (the repack thread's last step): wait for it too, or the
         // flag assertion below is racy under load.
         wal_empty && !engine.compacting.load(Ordering::SeqCst)
     });
     ```

  **Notes / invariants:**
  - This is the ONLY change. Do NOT modify any assertion, the `wait_until`
    helper (its 60 s timeout / 10 ms cadence stay), `maybe_compact`,
    `CompactionState::run`, the ADR 0004 §7 step ordering, the crash matrix, or
    any other test/file.
  - `Ordering` and `SeqCst` are already used in this test module (the existing
    flag asserts load with `Ordering::SeqCst`), so no new import is needed.
  - Do NOT touch `concurrent_maybe_compact_runs_exactly_once` (it already waits
    on the flag), `below_threshold_is_a_noop`, or `ram_only_engine_is_a_noop`
    (neither sets the flag).

  **Approach:** edit the two predicates to add the flag load, run the gates.

  **Acceptance (Критерии приёмки):**
  1. `cargo fmt --all --check` clean.
  2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
  3. `cargo check --workspace` clean.
  4. `cargo test -p vectors` green (all vectors tests, including all five
     compaction tests).
  5. `cargo test --workspace` green; **run it 8 consecutive times** and all eight
     must pass (the flake reproduced ~1/8 under load).
  6. Workspace test count unchanged: 1,474 (report `cargo test --workspace --
     --list | grep -c ': test$'`).
  7. `git diff --name-status` shows only `crates/vectors/src/usearch/compaction.rs`
     modified (nothing under `../synopsis`, no new dependencies, no production
     code, no other file).

  **Oracle reference:** none — usearch is a new engine (design D5); the Go oracle
  has no compaction thread.
