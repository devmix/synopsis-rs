# Proposal: fix the flaky usearch compaction tests

## Why

Two DISK-WAL compaction tests in `crates/vectors/src/usearch/compaction.rs` are
**flaky**: they intermittently fail `cargo test --workspace` under full-workspace
parallel load (observed ~1-in-5 runs) while passing reliably in isolation.

- `compaction_merges_segments_with_monotonic_ids` (`:398`)
- `compaction_slices_live_vectors_by_max_segment_vectors` (`:469`)

This is a real defect: the workspace gate is the machine-verified acceptance
criterion for every task, and a flaky gate makes that criterion unreliable.

## Root cause (diagnosed)

`maybe_compact` (`:54`) is a *trigger* that spawns a background thread (`:94`)
which runs `CompactionState::run()`. The documented procedure order in `run()`
(`:117-119`) is:

> live-key collection → new segments in scratch → **atomic directory swap** →
> **WAL cleanup** → in-memory list swap

Both flaky tests wait with `wait_until` for the **directory swap only** (the new
segment id is present, the old ids are gone) and then immediately assert the
**WAL is empty**:

```rust
wait_until(|| {
    let ids = segment_ids(&dir.0);
    ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)   // swap only
});
// ...
assert!(wal_rows(&conn).is_empty(), "the DISK WAL rows are cleaned");   // racy
```

Because the WAL DELETE runs *after* the swap in the background thread, the
predicate can return `true` while the WAL cleanup is still in flight. Under
full-workspace CPU load the background thread lags, so the assertion fires too
early and fails. In isolation the thread finishes fast enough that it passes.

The third compaction test (`:586`) is **not** affected: it waits on
`!engine.compacting` (the flag cleared only after `run()` fully returns, i.e.
after the WAL cleanup).

## What changes

Extend the `wait_until` predicate in the two affected tests so it also waits for
the WAL to be empty — i.e. wait for exactly the two conditions the tests assert
(swap **and** WAL cleanup). No production code, public API, schema, or contract
changes. The post-wait assertions are kept unchanged.

## Impact

- **Frozen contracts:** none touched (no MCP tools, CLI surface, data schema, or
  config format).
- **Parity:** unaffected — this is test-only synchronization, no behavior change.
- **Gates:** `cargo test --workspace` becomes deterministic for these two tests
  (the gate that was intermittently red).

## Non-goals

- Not re-architecting the compaction thread or its single-flight flag.
- Not changing the ADR 0004 §7 "directory before WAL" ordering (it is mandatory
  and correct; only the test's wait condition was incomplete).
- Not touching the test at `:586` (`concurrent_maybe_compact_runs_exactly_once`):
  it is flaky too, but for a **different** reason — the stale-threshold check and
  the single-flight CAS in `maybe_compact` are not atomic, so a redundant second
  repack can run under concurrent load. That is a production-behavior question and
  is deferred to its own change (human decision 2026-09-01).
