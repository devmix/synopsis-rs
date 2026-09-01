# Proposal: close the compaction test flag-clear race

## What

Two usearch compaction tests assert that the single-flight `compacting` flag is
cleared immediately after a `wait_until` poll whose predicate only waits for the
segment swap and the post-swap WAL cleanup. Because the background repack thread
clears the flag **after** the WAL cleanup and the in-memory list swap, there is a
window in which the predicate passes (WAL empty) but the flag is still set — so
the flag assertion fails under full-workspace load.

This change extends the two `wait_until` predicates to also wait for the
`compacting` flag to be cleared, so each test waits for the *final* observable
state before asserting on it.

## Why

The ADR 0004 §7 repack ordering is: directory swap (step 5) → WAL cleanup (step
6) → in-memory list swap (step 7) → flag clear (the thread's last step, after
`CompactionState::run` returns). The flag-clear ordering in production is correct
(the flag must stay set until all the work is done). The bug is in the *tests*:
their poll predicate stops at the WAL cleanup (step 6) and then asserts on the
flag (set/cleared at step 7+), a state that has not been guaranteed yet.

This is the same class of load-dependent flake as the earlier compaction fixes
(the WAL-sync predicate and the poll-timeout bound), one step later in the
ordering. It was observed failing in `compaction_merges_segments_with_monotonic_ids`
(the flag assertion) under full-workspace `cargo test --workspace`.

## Scope

- **File:** `crates/vectors/src/usearch/compaction.rs` — the test module only.
- **Change:** add `&& !engine.compacting.load(Ordering::SeqCst)` to the
  `wait_until` predicate in two tests:
  - `compaction_merges_segments_with_monotonic_ids`
  - `compaction_slices_live_vectors_by_max_segment_vectors`
- **Test-only.** No production code, no logic, no predicate ordering of the
  repack, no other file.

## Frozen contracts touched

None. This is a test-only change to the `vectors` crate's compaction unit tests.
No MCP tool, CLI surface, data schema, or config format is affected. No parity
impact (usearch is a new engine, design D5; the Go oracle has no compaction
thread).

## Non-goals

- Do NOT change the production repack ordering (the flag-clear after the WAL
  cleanup + list swap is correct and stays).
- Do NOT change the `wait_until` helper's timeout or poll cadence (already 60 s
  / 10 ms from the earlier `fix-vectors-compaction-wait-timeout` change).
- Do NOT touch the other compaction tests: `below_threshold_is_a_noop` and
  `ram_only_engine_is_a_noop` never set the flag (no repack), and
  `single_flight_admits_exactly_one_repack` already waits on the flag.
- Do NOT touch the cli test
  `serve_startup_reconcile_jobs_are_processed_by_the_worker` (tracked separately;
  not reproduced in 8 full-workspace runs).
