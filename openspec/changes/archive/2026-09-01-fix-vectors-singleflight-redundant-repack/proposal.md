# Proposal: close the single-flight redundant-repack race in usearch compaction

## Why

`crates/vectors/src/usearch/compaction.rs` has a flaky test,
`concurrent_maybe_compact_runs_exactly_once` (`:567`), that intermittently fails
`cargo test --workspace` under full-workspace parallel load (observed ~1-in-3
runs) while passing in isolation. It asserts that concurrent `maybe_compact`
calls run **exactly one** repack (`segment_ids == [5]`); it sometimes gets
`[6]` — a **second**, redundant repack ran.

The root cause is a real (if minor) production race, not a test bug: the
stale-threshold check and the single-flight CAS in `maybe_compact` are **not
atomic**.

## Root cause (diagnosed)

`maybe_compact` (`:54`) does, in order:

1. read the stale fraction from the cache and pass the threshold check
   (`:57-70`);
2. `compare_exchange(false, true)` on the `compacting` flag (`:73-79`);
3. spawn a background thread that runs `CompactionState::run()` then clears the
   flag (`:94-98`).

`run()` clears the stale cache at its end (`:202`) and the flag is cleared only
*after* `run()` returns (`:98`). So a thread can:

1. read the stale fraction (40%, passes the threshold) **before** a concurrent
   repack clears it, then
2. acquire the flag **after** that repack completed and cleared the flag →
   spawn a **redundant second repack** that moves the same live keys to yet
   newer ids.

The single-flight flag prevents *overlapping* repacks, but it does not prevent a
redundant *sequential* repack when a trigger straddles a completion. The
documented intent (`:562-565`) is "exactly one repack," which the current code
does not guarantee.

The redundant repack is **harmless** (no data loss — count and WAL still hold)
but wastes work: on a 16 GB laptop it is an avoidable CPU/IO spike.

## What changes

Add a **double-check** in `CompactionState::run()`: right after acquiring the
layout lock, recompute the stale fraction and return `Ok(())` early if it is now
below threshold (the work was already done by a prior repack). This is the
standard check-then-act / double-check pattern: the trigger check in
`maybe_compact` stays as a cheap fast-path, and the authoritative check moves to
the point of doing the work, under the layout lock.

The redundant case (stale already cleared) bails; a genuine second repack (new
stale keys above threshold) still proceeds. The "exactly one repack" property
holds, and the flaky test becomes deterministic.

## Impact

- **Frozen contracts:** none (no MCP tools, CLI surface, data schema, or config
  format). The usearch engine is a new crate (design D5), not a frozen contract.
- **Parity:** unaffected — no change to the query path or to compaction output
  for a genuine repack; only a redundant repack is suppressed.
- **Gates:** `cargo test --workspace` becomes deterministic for this test.

## Non-goals

- Not changing the "directory before WAL" ordering or the crash matrix (ADR
  0004 §7) — they are correct and untouched.
- Not making `maybe_compact` synchronous or adding a completion handle.
- Not re-architecting the single-flight flag; the double-check is the minimal
  correct fix.
