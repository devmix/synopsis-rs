# Design: raise the compaction test poll timeout

## Context

`wait_until` (`crates/vectors/src/usearch/compaction.rs:349`) is the shared poll
helper for the three compaction tests:

```rust
fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);   // :350
    while !cond() {
        assert!(Instant::now() < deadline,
                "the compaction did not finish within the timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}
```

Call sites: `:449`, `:513`, `:622`. All three wait for a **background** repack
thread to complete. The repack is tiny (50–100 vectors, a few usearch `save`s),
so under normal scheduling it finishes in well under a second. The 10 s timeout
is therefore pure starvation headroom — it only bites when the OS fails to
schedule the background thread for >10 s, which happens under `cargo test
--workspace`'s full parallel load on a laptop.

No test relies on the timeout *firing*: the `:354` assertion is a failure
condition, and every call site expects `cond()` to become true. So raising the
bound cannot break a passing test; it only widens the window in which a
starved-but-healthy repack is still allowed to finish.

## Decision D1 — 60 s, via a named constant

Introduce a named constant and use it in `wait_until`:

```rust
/// Generous bound for the background-repack poll. The repack is tiny (finishes
/// in milliseconds under normal scheduling); this ceiling only matters when the
/// OS deschedules the background thread for many seconds under full-workspace
/// parallel load. 10 s proved too tight on a 16 GB laptop; 60 s is 6x headroom.
const REPACK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + REPACK_WAIT_TIMEOUT;
    ...
}
```

The named constant makes the intent (and the "why 60") self-documenting, rather
than a magic `10`/`60` inline.

## Why 60 s (and not more / less)

- **Not less (e.g. 30 s):** the observed failure was a >10 s starvation; 30 s is
  only 3× headroom and could still flake under heavier load.
- **Not more (e.g. 300 s):** a genuine hang (a real bug) would take 5× longer to
  surface in CI. 60 s is well beyond any realistic scheduling delay for a
  millisecond-scale repack, so extra headroom buys nothing.
- **60 s** is the pragmatic sweet spot: robust against laptop load, still a
  meaningful ceiling for real hangs.

## Alternatives considered

- **Synchronous repack in tests (test seam).** Eliminates the background thread
  entirely, so no starvation. Rejected: a larger change (a test-only path or a
  production seam) for an issue that a generous timeout resolves; the background
  thread is the real production behavior and the test should exercise it.
- **Reduce test parallelism (`--test-threads=1`).** A CI-config change, not a
  code fix; slows the whole suite; masks the symptom rather than giving the
  thread enough time.
- **Keep 10 s and retry on timeout.** Adds retry complexity and still flakes
  within the retry budget; a higher ceiling is simpler.

## Verification

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --workspace` clean.
- `cargo test -p vectors` green (all compaction tests).
- `cargo test --workspace` run ≥5 consecutive times, all green (sanity; the
  starvation is environmental and not deterministically reproducible, but a pure
  timeout increase cannot regress a passing test).
- Workspace test count unchanged (1,474).
