# Design: fix the flaky usearch compaction tests

## Context

`CompactionState::run()` (compaction.rs `:120`) performs, under the layout lock:

1. collect live keys (sidecar − stale, no SQL),
2. build new segments `N+1..N+M` in the scratch dir,
3. **atomic directory swap** (new `segments/` replaces old; old → `segments.old/`),
4. **WAL cleanup** (DELETE the old ids' DISK WAL rows),
5. in-memory list swap.

`maybe_compact` (`:54`) sets the `compacting` flag and spawns a background thread
that runs `run()` then clears the flag (`:97-98`). So the *observable* completion
signal is `!engine.compacting`; the *swap* is observable earlier (step 3).

## Decision D1 — wait for the asserted conditions, not just the swap

Extend each affected test's `wait_until` predicate to require **both** the
directory swap **and** an empty WAL. This makes the predicate wait for exactly
what the test then asserts, removing the race.

```rust
wait_until(|| {
    let ids = segment_ids(&dir.0);
    if !(ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)) {
        return false;
    }
    // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for it
    // too, or the assertion below is racy under load.
    let conn = Connection::open(&db_path).unwrap();
    wal_rows(&conn).is_empty()
});
```

The post-wait assertions (`segment_ids`, `read_keys`, `count`, `wal_rows(...).is_empty()`)
are left byte-for-byte unchanged.

## Decision D2 — open the WAL connection per poll

The predicate opens a fresh `Connection` each poll (10 ms cadence, ≤ 10 s
timeout). In SQLite WAL mode readers never block the background writer, so this
cannot interfere with the in-flight WAL DELETE. Per-poll open is the simplest
correct option and avoids borrowing a long-lived connection across the closure.

## Alternatives considered

- **Wait on `!engine.compacting` instead of the swap.** Also correct (the flag
  clears only after `run()` returns, i.e. after the WAL cleanup). Rejected as the
  primary fix because it changes the test's explicit intent (the test asserts
  specific segment ids, so waiting for the swap it asserts is clearer) and hides
  the "which step did we wait for" detail. It is a valid fallback if D1 shows any
  residual flake.
- **Make `maybe_compact` synchronous / add a completion handle.** Rejected: a
  behavioral change to production code for a test-only race; out of scope and
  against the "no 1:1, but don't change behavior unnecessarily" principle.
- **Raise the poll timeout / add a fixed sleep.** Rejected: masks the race
  instead of removing it; still flaky under heavier load.

## Verification

- `cargo test -p vectors` (whole crate, parallel) — the two tests pass.
- `cargo test --workspace` run repeatedly — the two tests no longer fail under
  full load (the original failure mode).
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --workspace` clean.
- Workspace test count unchanged (1,474): no tests added/removed, only the
  wait predicate widened.
