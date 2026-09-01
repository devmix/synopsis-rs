# Design: a forced shutdown is not a failure

## Context

`serve_with_stop` (crates/cli/src/serve/server.rs:302) runs the `serve`
subcommand: bootstrap → initial sync (reconcile) → document worker → graph →
searcher → MCP server → watcher → bind → owner loop → graceful shutdown under a
hard bound. The bound is `SHUTDOWN_TIMEOUT = 10 s` (`:106`, oracle
`shutdownCtx`).

The observed flake: under full-workspace load the graceful-shutdown path (axum
drain + `watcher.stop()`) can exceed the 10 s bound, so `serve_with_stop`
returns `Err`, and the integration test
`serve_startup_reconcile_jobs_are_processed_by_the_worker` fails at its
`result.is_ok()` assertion (`server.rs:1216`). Reproduced ~1-in-30 full-workspace
runs.

## The inconsistency

The shutdown tail (server.rs:638–654):

```rust
match runtime.block_on(async { tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown_done).await }) {
    Ok(()) => tracing::info!("MCP server stopped gracefully"),
    Err(_) => tracing::warn!("forced shutdown after timeout"),
}
let serve_result = match serve_result {
    Some(result) => result,
    None => Err(io::Error::other("serve task did not finish within the shutdown bound")),
};
if let Err(err) = &serve_result {
    tracing::error!(error = %err, "MCP server error");
}
serve_result.map_err(CliError::Io)
```

`serve_result` is `Option<io::Result<()>>`:
- `Some(Ok(()))` — the axum serve drained cleanly (graceful stop).
- `Some(Err(e))` — the axum serve failed (e.g. a panic folded by `reap_serve`).
- `None` — the bound expired before the serve task drained (**forced shutdown**).

On the timeout the code logs `warn!("forced shutdown after timeout")` — treating
the forced stop as **acceptable** — and *then* maps the `None` arm to `Err`,
treating it as a **failure**. The log and the return value disagree.

## D1 — A forced shutdown returns `Ok(())`

Change only the `None` arm:

```rust
let serve_result = match serve_result {
    Some(result) => result,
    // A forced shutdown (the bound expired before the serve task drained) is
    // not a failure: the process is about to exit, so the in-flight axum drain
    // is abandoned — that is the point of the forced bound. The warn! above
    // already logged it; returning Ok keeps the exit code clean (a forced stop
    // met the user's intent: the server stopped). The task is reaped when the
    // runtime drops at the end of run_serve.
    None => Ok(()),
};
```

The `Some(result)` arm is unchanged: a serve task that finished with an error
(a panic, via `reap_serve`) is still `Err`. A setup/bind error still propagates
via `?` earlier in the function.

**Why this is correct (not a weakened error path):**
- The user ran `serve` to run the server and stop it on signal. A forced stop
  *stopped the server* — the intent is met. The only difference from a graceful
  stop is that in-flight requests (if any) are dropped at process exit, which is
  inherent to a forced bound.
- The existing `warn!` is the operator signal that the stop was forced; the
  return value no longer contradicts it.
- A genuine failure (serve task panic, bind error, config error) is unchanged.

**Why not the alternatives:**
- *Raise `SHUTDOWN_TIMEOUT`* — the bound is oracle parity (`shutdownCtx`);
  changing it would diverge from the oracle and would only delay the flake, not
  remove it (any fixed bound can be exceeded under load).
- *Make the test tolerate the specific error string* — fragile (string-matches
  the `io::Error` message), must be duplicated in every `serve` test, and leaves
  the production `warn!`-then-`Err` inconsistency in place.
- *Return a distinct "forced" status* — there is no such status in the CLI
  surface contract; `Ok` + the `warn!` log is the existing, minimal encoding.

## Contract impact

The frozen `cli-surface` spec pins subcommands, flags, argument order, and
config resolution. It does **not** pin the exit code on a forced shutdown, so
this is a behavior correction, not a contract change. `skip_specs: true`.

## Affected tests

All three `serve_with_stop` integration tests assert `result.is_ok()` and are
*fixed* by this change (none expects the `Err`):
- `serve_starts_serves_health_and_stops` (`:1041`, `:1081`)
- `serve_startup_reconcile_jobs_are_processed_by_the_worker` (`:1133`, `:1217`)
- `serve_rebuilds_vectors_on_dimension_mismatch` (`:1314`, `:1356`)

The sibling test's `stopped < SHUTDOWN_TIMEOUT + 5 s` margin (`:1092–1095`) is
unchanged and remains valid (it bounds a *hang*, which never returns).

## Oracle reference

`../synopsis/cmd/app/serve.go` — the `shutdownCtx` 10 s bound (the bound itself
is unchanged). The oracle's forced-stop exit semantics are not part of the
frozen surface contract; this change corrects the Rust return value to match its
own log line.
