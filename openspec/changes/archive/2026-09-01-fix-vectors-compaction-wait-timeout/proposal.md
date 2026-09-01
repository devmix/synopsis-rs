# Proposal: raise the compaction test poll timeout to survive load starvation

## Why

The usearch compaction tests poll a **background** repack thread via a shared
`wait_until` helper whose timeout is **10 s** (`crates/vectors/src/usearch/
compaction.rs:350`). Under `cargo test --workspace`'s full parallel load, the
background thread can be **CPU-starved for more than 10 s**, so the poll times
out and the test fails — even though the repack itself is tiny (50–100 vectors,
finishes in milliseconds under normal scheduling).

Observed: `compaction_merges_segments_with_monotonic_ids` failed once in a 5-run
batch with a 10 s `wait_until` timeout (the background thread was descheduled
under load); it passed 5/5 in isolation and on a re-run batch. This is the last
of a small family of load-dependent flakes in the compaction tests (the WAL-sync
race and the single-flight redundant-repack race are already fixed in separate
archived changes).

## What changes

Raise the `wait_until` timeout from **10 s to 60 s**, via a named constant with a
comment explaining the rationale. The repack is tiny, so the timeout only matters
in the (rare) starvation case; 60 s is 6× the previous headroom and is generous
for a 16 GB laptop running the full workspace suite. No logic, predicate, or
production change — the test still finishes in milliseconds as soon as the
condition holds; only the *failure* bound moves.

## Impact

- **Frozen contracts:** none (no MCP tools, CLI surface, data schema, or config
  format). Test-only.
- **Parity:** unaffected.
- **Gates:** `cargo test --workspace` no longer flakes for the compaction tests
  due to the poll timeout (the repack still has to actually complete; this only
  removes the artificial 10 s ceiling).

## Non-goals

- Not making the repack synchronous in tests (a larger change; the timeout headroom
  is the minimal, robust fix).
- Not reducing test parallelism (a CI-config change, not a code fix).
- Not changing the repack's behavior, the ADR 0004 §7 ordering, or any predicate.
