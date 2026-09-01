# Tasks: fix the flaky usearch compaction tests

Change: `fix-vectors-compaction-flaky-test`

## Change header (read first)

- **Goal:** remove the load-induced race in the two DISK-WAL compaction tests so
  `cargo test --workspace` is deterministic.
- **File scope (only this file):** `crates/vectors/src/usearch/compaction.rs`.
  Do NOT touch production code, public API, other crates, or the Go oracle
  (`../synopsis` is read-only).
- **Context (read these first):**
  - This file, `compaction.rs` — especially `maybe_compact` (`:54`), the
    background-thread spawn (`:94-99`), `CompactionState::run` (`:120`), the
    `wait_until` helper (`:325`), and the two tests below.
  - `openspec/changes/fix-vectors-compaction-flaky-test/proposal.md` (root cause).
  - `openspec/changes/fix-vectors-compaction-flaky-test/design.md` (D1, D2).
  - `openspec/config.yaml`, `AGENTS.md` (gates).
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo check --workspace`; `cargo test --workspace`.
- **Test-count invariant:** the workspace total stays **1,474** (no tests added or
  removed — only the wait predicate of two existing tests is widened).

- [ ] **1.1** — Widen the `wait_until` predicate in both flaky compaction tests to
  also require the WAL to be empty.

  **Goal:** make each test wait for exactly the two conditions it asserts — the
  directory swap **and** the WAL cleanup — so the post-wait WAL assertion is no
  longer racy under full-workspace load.

  **File scope:** `crates/vectors/src/usearch/compaction.rs` only.

  **The two tests (do NOT touch the third test at `:586`, which already waits on
  `!engine.compacting` and is correct):**

  1. `compaction_merges_segments_with_monotonic_ids` (`:398`). Its current
     predicate (`:425-428`) is:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)
     });
     ```
     Replace it so it returns `true` only when the swap is done **and** the WAL is
     empty:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         if !(ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)) {
             return false;
         }
         // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
         // it too, or the WAL assertion below is racy under load.
         let conn = Connection::open(&db_path).unwrap();
         wal_rows(&conn).is_empty()
     });
     ```
     Leave the post-wait assertions (`:432-458`, including the
     `wal_rows(&conn).is_empty()` at `:453-457`) unchanged.

  2. `compaction_slices_live_vectors_by_max_segment_vectors` (`:469`). Its current
     predicate (`:483-486`) is:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)
     });
     ```
     Replace it analogously (swap on `&5`), with the same WAL-empty clause:
     ```rust
     wait_until(|| {
         let ids = segment_ids(&dir.0);
         if !(ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)) {
             return false;
         }
         // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
         // it too, or the WAL assertion below is racy under load.
         let conn = Connection::open(&db_path).unwrap();
         wal_rows(&conn).is_empty()
     });
     ```
     Leave the post-wait assertions (`:489-510`, including the
     `wal_rows(&conn).is_empty()` at `:509`) unchanged.

  **Notes / invariants:**
  - `Connection` (`rusqlite::Connection`) and `wal_rows` are already imported in
    the test module (`:289`, `:294`); `db_path` is in scope in both tests. No new
    imports needed.
  - Keep the existing `#[allow(clippy::unwrap_used)]` test-module opt-out; the new
    `.unwrap()` in the predicate is fine under it.
  - Do NOT change `maybe_compact`, `CompactionState::run`, the `wait_until` helper
    signature, the ADR 0004 §7 ordering, or any production code.
  - Do NOT touch the third test (`:586`, waits on `!engine.compacting`).

  **Approach:** edit the two predicates exactly as shown; run the gates.

  **Acceptance (Критерии приёмки):**
  1. `cargo fmt --all --check` clean.
  2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
  3. `cargo check --workspace` clean.
  4. `cargo test -p vectors` green (all vectors tests, including the two edited).
  5. `cargo test --workspace` green; **run it 3 consecutive times** and all three
     must pass (the original failure mode was ~1-in-5 under full load).
  6. Workspace test count unchanged: 1,474 (report `cargo test --workspace --
     --list | grep -c ': test$'`).
  7. `git diff --name-status` shows only `crates/vectors/src/usearch/compaction.rs`
     modified (nothing under `../synopsis`, no new dependencies, no other files).

  **Oracle reference:** none — this is a Rust-only test-synchronization fix; the Go
  oracle has no compaction thread (usearch is a new engine, design D5).
