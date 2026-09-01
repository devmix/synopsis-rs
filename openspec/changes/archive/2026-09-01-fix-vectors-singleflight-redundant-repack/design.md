# Design: close the single-flight redundant-repack race

## Context

`maybe_compact` (`:54`) is check-then-act:

```
57   let stale = self.stale.snapshot();
58-66 compute stale_total, total_disk
67-70 if total_disk == 0 || stale_total * 100 <= threshold * total_disk { return Ok(()); }
73-79 CAS(false, true) on `compacting`; on failure (already running) return Ok(())
94-98 spawn thread { run(); store(false) }
```

The threshold check (57-70) and the CAS (73-79) are separated in time. Between
them a concurrent repack can complete: it clears the stale cache (`run()`
`:202`) and the flag (`:98`). A thread that already passed the threshold check
on the *pre-compaction* state then wins the CAS on the *post-compaction* (cleared)
flag and spawns a redundant repack.

`run()` (`:120`) acquires the layout lock (`:121`) before doing any structural
work, and the stale cache + disk segment list are only mutated under that lock.
So a re-check performed under the lock sees a consistent post-compaction state.

## Decision D1 — double-check the trigger under the layout lock

At the top of `run()`, immediately after `let _layout = mutex_guard(&self.layout_lock);`
(`:121`), recompute the exact same predicate `maybe_compact` uses and bail if it
no longer holds:

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

The existing body (live-key snapshot at `:126` onward) is unchanged. The bail
returns `Ok(())`; the spawn closure (`:98`) still clears the flag, so no stuck
flag. The redundant case (stale empty after a prior repack) bails; a genuine
repack (new stale keys above threshold) proceeds.

## Why this is correct

- **Exactly one repack:** only the first thread to win the CAS runs `run()`. Any
  thread that wins the CAS *after* that repack completed re-checks under the lock,
  sees the cleared stale state, and bails. Threads that pass the threshold check
  while the repack is still running fail the CAS (flag set) and never run.
- **No lost legitimate work:** if new deletes push the stale fraction back above
  threshold, the re-check passes and the repack proceeds.
- **No stuck flag:** the flag is cleared by the spawn closure regardless of the
  bail.
- **Consistency:** the re-check reads `stale` and `disk_segments` under the
  layout lock, the same lock `run()` uses for its structural work.

## Alternatives considered

- **B: relax the test to "≥1 repack, no data loss" + fix the doc comment.**
  Test-only, lower risk, but leaves the redundant-repack wasted work and
  contradicts the documented "exactly one" intent. Rejected per human decision
  2026-09-01 (chose A).
- **Clear the flag before clearing the stale cache.** Does not help: the race is
  in the *trigger check* reading stale state, not in the flag/cache ordering.
- **Make `maybe_compact` synchronous / add a completion handle.** A behavioral
  change for a redundant-work issue; overkill.
- **Hold the flag across the threshold re-check (move the check after the CAS
  in `maybe_compact`).** Also valid, but the check-then-CAS in `maybe_compact`
  is intentionally a cheap fast-path that avoids spawning a thread when clearly
  below threshold; moving the authoritative check into `run()` (under the lock)
  keeps the fast-path cheap and centralizes the authoritative decision where the
  work happens.

## Verification

- `cargo test -p vectors` green (all vectors tests, including
  `concurrent_maybe_compact_runs_exactly_once`).
- `cargo test --workspace` run repeatedly (≥3) — the single-flight test no longer
  flakes under full load (the original ~1-in-3 failure mode).
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --workspace` clean.
- Workspace test count unchanged (1,474): no tests added/removed.
