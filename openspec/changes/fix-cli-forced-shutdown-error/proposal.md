# Proposal: a forced shutdown is not a failure

## What

`serve_with_stop` (the `serve` subcommand's run loop) treats a **timed-out
forced shutdown** as a hard error. When the graceful-shutdown path exceeds its
hard bound (`SHUTDOWN_TIMEOUT = 10 s`, oracle `shutdownCtx`), the function logs
`warn!("forced shutdown after timeout")` and *then* returns
`Err("serve task did not finish within the shutdown bound")`. The warning says
"acceptable", the return value says "failure" — internally inconsistent.

This change makes a timed-out forced shutdown return `Ok(())` instead of `Err`,
consistent with the existing `warn!`. A real failure (the axum serve task
panicking, or a setup error) is still an error.

## Why

Under full-workspace load the axum drain + `watcher.stop()` can exceed the 10 s
bound, so `serve` returns an error even though the process stopped (the user's
intent was achieved). This is a load-dependent flake in the `serve` integration
tests (`serve_startup_reconcile_jobs_are_processed_by_the_worker`, reproduced
~1-in-30 full-workspace runs at the `result.is_ok()` assertion) and would
misreport a clean forced stop as a CLI failure.

The semantics: a forced shutdown means the server stopped (the process is about
to exit) but the in-flight axum drain did not finish within the bound. The
in-flight drain is abandoned when the process exits — that is the *point* of the
forced bound. The user's intent (stop the server) is met, so it is not a
failure. The existing `warn!` already encodes this; only the return value was
wrong.

## Scope

- **File:** `crates/cli/src/serve/server.rs` — `serve_with_stop` only (the
  `serve_result` match, currently `:643–650`).
- **Change:** the `None` arm (the serve task did not finish within the bound)
  returns `Ok(())` instead of `Err(io::Error::other("serve task did not finish
  within the shutdown bound"))`. The `Some(result)` arm is unchanged (a serve
  task that finished with an error — e.g. a panic folded by `reap_serve` — is
  still an error).

## Frozen contracts touched

**None.** The frozen `cli-surface` spec pins the subcommands, flags, argument
order, and config resolution — it does **not** pin the process exit code on a
forced shutdown. This is a behavior correction (the return value now matches the
already-present `warn!`), not a contract change. No MCP tool, data schema, or
config format is affected. No parity impact (the oracle's `shutdownCtx` is the
10 s bound; the oracle's forced-stop exit semantics are not part of the frozen
surface contract).

## Behavior change (explicit)

`synopsis serve` now exits `0` (with the existing `warn!` log line) when the
graceful shutdown exceeds the 10 s bound, instead of exiting with an error. A
serve task that panics, or a setup/bind error, still exits with an error.

## Non-goals

- Do NOT change `SHUTDOWN_TIMEOUT` (the 10 s bound is oracle parity and stays).
- Do NOT change the `Some(result)` arm (a real serve-task failure is still an
  error).
- Do NOT change the `warn!`/`info!` log lines.
- Do NOT weaken the `serve` integration tests — they keep asserting
  `result.is_ok()`; the production fix is what makes that assertion robust under
  load. (The sibling test's `stopped < SHUTDOWN_TIMEOUT + 5 s` margin is
  unchanged.)
- Do NOT touch the Go oracle (`../synopsis` is read-only).
