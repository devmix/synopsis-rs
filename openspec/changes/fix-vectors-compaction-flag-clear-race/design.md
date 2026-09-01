# Design: close the compaction test flag-clear race

## Context

The usearch background compaction (ADR 0004 §7) runs on a fire-and-forget
`std::thread`. The tests trigger a repack with `maybe_compact()` and then poll
with a shared `wait_until` helper until the observable state settles, and assert
on that state. Three earlier load-dependent flakes in this test family have been
fixed (the WAL-sync predicate, the single-flight redundant repack, and the
poll-timeout bound). This design closes the last one.

## The repack ordering (production, unchanged)

In `maybe_compact` (compaction.rs:55) the trigger CASes the `compacting` flag
false→true and spawns a thread that runs `CompactionState::run` and then clears
the flag:

```
thread:  state.run()                      // steps 1..7
         compacting.store(false)          // the flag clears LAST
```

`CompactionState::run` (compaction.rs:122), under the layout lock, does:

1. collect live keys,
2. continue the monotonic id sequence,
3. collect vectors,
4. build new segments in the scratch dir,
5. **atomic directory swap** (step 5, compaction.rs:202),
6. **WAL cleanup** — delete the old ids (step 6, compaction.rs:208),
7. **in-memory list swap** + stale-cache bump (step 7).

So the flag clears strictly *after* step 7, which is strictly after step 6 (the
WAL cleanup). The ordering is correct: the flag must stay set until the WAL is
cleaned and the in-memory list is swapped, or a concurrent query/trigger could
observe a half-compacted state.

## The test race

The two affected tests poll with a predicate that waits for the segment swap and
the WAL to be empty:

```rust
wait_until(|| {
    let ids = segment_ids(&dir.0);
    if !(ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)) {
        return false;
    }
    let conn = Connection::open(&db_path).unwrap();
    wal_rows(&conn).is_empty()           // step 6 done
});
// ...
assert!(!engine.compacting.load(...), "the single-flight flag is cleared");
// ^ set/cleared at step 7+ (AFTER step 6) — not guaranteed by the predicate
```

Under full-workspace load the repack thread can be scheduled such that step 6
(the WAL DELETE) commits before the thread finishes step 7 and clears the flag.
The predicate then returns true (WAL empty) while the flag is still set, and the
subsequent assertion fails. This is exactly the observed failure:

```
compaction_merges_segments_with_monotonic_ids panicked at compaction.rs:496:9
  "the single-flight flag is cleared"
```

## D1 — Wait for the final observable state (the flag) in the predicate

Extend both predicates to also require the flag to be cleared:

```rust
wait_until(|| {
    let ids = segment_ids(&dir.0);
    if !(ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)) {
        return false;
    }
    let conn = Connection::open(&db_path).unwrap();
    let wal_empty = wal_rows(&conn).is_empty();
    drop(conn);
    wal_empty && !engine.compacting.load(Ordering::SeqCst)
});
```

Because the flag clears *after* the WAL cleanup + list swap, a cleared flag
*implies* the WAL is already empty and the swap is done. Keeping the swap + WAL
checks in the predicate is harmless (they make the intent explicit and catch a
repack that never ran) and costs nothing extra (the flag load is a cheap atomic;
the connection is already opened per poll).

**Why not the alternatives:**
- *Sleep a fixed duration after the poll* — reintroduces the timing dependency
  the `wait_until` helper exists to remove; a fixed sleep is either too short
  (flakes) or too long (slow).
- *Assert the flag inside the predicate and panic on timeout* — the `wait_until`
  helper already panics on timeout; adding the flag to the predicate is the
  minimal, intent-preserving change.
- *Change the production ordering to clear the flag before the WAL cleanup* —
  wrong: the flag must stay set until the WAL is cleaned and the list is swapped,
  or a concurrent query/trigger sees a half-compacted state. The production
  ordering is correct; only the test's wait was incomplete.

## Affected tests (and why the others are fine)

| Test | Predicate | Flag assert | Status |
|---|---|---|---|
| `compaction_merges_segments_with_monotonic_ids` | swap + WAL | `:497` | **racy — fix** |
| `compaction_slices_live_vectors_by_max_segment_vectors` | swap + WAL | `:649` | **racy — fix** |
| `single_flight_admits_exactly_one_repack` | flag only (`:629`) | — | fine (waits on the flag; a cleared flag implies the WAL is empty) |
| `below_threshold_is_a_noop` | none (sleep 300 ms) | `:419` | fine (no repack; flag never set) |
| `ram_only_engine_is_a_noop` | none | `:649` | fine (no disk segments; flag never set) |

## Oracle reference

None — usearch is a new engine (design D5); the Go oracle has no background
compaction thread. The fix is a test-only wait-completeness correction.
