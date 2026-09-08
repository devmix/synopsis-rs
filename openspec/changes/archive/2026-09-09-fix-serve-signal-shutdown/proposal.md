# Proposal: make `serve` stop on Ctrl+C (cancellable worker cycle + force-exit on second signal)

## Why

`synopsis serve` effectively cannot be stopped with Ctrl+C: SIGINT is
logged ("received SIGINT, stopping...") but the process keeps ingesting
documents for minutes to hours, and repeated Ctrl+C presses have no effect.
The stop signal is only observed in the owner-loop `select!`, but the
document-queue worker cycles run **synchronously inline on the owner thread**
(`DocumentWorker::run_once`): the startup drain processes all pending
`doc:index` jobs *before* the loop even starts, and every job runs the full
pipeline (including the slow LLM entity extraction — ~100 s per document in
the field report). With 424 documents pending, the signal sits unobserved in
the broadcast channel for hours. The 10 s forced-shutdown bound covers only
the axum drain + watcher stop *after* the owner loop exits — it never covers
the worker cycles.

## What Changes

- **Cancellable worker cycle (ingestion):** a shared shutdown flag (a small
  newtype over `Arc<AtomicBool>` — no new dependency) set by the signal task
  on the first SIGINT/SIGTERM. `DocumentWorker::run_once` checks the flag
  before each claim (including the first): once set, the cycle stops claiming
  new tasks and completes its tail as usual (GC, per-cycle vector
  persistence when work happened, summary). An in-flight task runs to
  completion (bounded by one task) and is recorded as usual. Unclaimed tasks
  stay `pending` — the existing restart recovery
  (`recover_stuck_processing`) and startup reconcile already cover them.
- **Cancellable startup drain (cli):** the startup drain and every
  owner-loop worker cycle use the same flag (the same `DocumentWorker`), so
  the server is stoppable even during the initial-sync drain — the exact
  hang from the field report.
- **Force-exit on a second signal (cli):** after the first SIGINT/SIGTERM, a
  second SIGINT/SIGTERM exits immediately with exit code **130** (128 +
  SIGINT, the Unix convention for "killed by SIGINT"). Safe by design: the
  recovery design explicitly tolerates SIGKILL-grade unclean exits.
- **SSE sessions terminated on graceful shutdown (mcp + cli, design D5):**
  found after the first three tasks, deterministically reproduced — with a
  connected legacy-SSE MCP client the graceful stop still waited out the
  full 10 s bound (axum's drain waits for in-flight requests; the long-lived
  `GET /sse` stream never ends on the shutdown signal). The server now ends
  its own SSE streams when the graceful stop fires (`SseSessionMap::close_all`
  via `Server::close_all_sessions`, called from the serve-task shutdown
  future), so the drain completes promptly; connected clients see a clean
  EOF and can reconnect.
- **Documentation:** README.md + site/docs describe the shutdown semantics
  (first Ctrl+C = graceful stop after the in-flight task, with connected
  SSE clients' streams ended; second = immediate exit).

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `pipeline`: new requirement `Cancellable worker cycle` — the worker checks
  the shared shutdown flag before each claim; an in-flight task completes and
  is recorded as usual; unclaimed tasks stay `pending`.
- `cli-surface`: the `serve subcommand` requirement is re-specified with
  explicit signal semantics — first SIGINT/SIGTERM: graceful stop (in-flight
  task completes, the worker stops claiming, axum drains, exit 0); second
  SIGINT/SIGTERM: immediate forced exit, code 130.

## Impact

- `crates/ingestion` — `worker.rs` (new `ShutdownFlag` newtype, optional
  flag on `DocumentWorker`, claim-loop check in `run_once`, unit tests).
- `crates/cli` — `serve/server.rs` (signal task: flag set + second-signal
  force-exit; `serve_with_stop` takes the flag; startup drain and owner-loop
  cycles become cancellable), `tests/serve_server.rs` (call sites + one new
  integration test).
- `README.md`, `site/docs/` — shutdown-semantics documentation.
- No new dependencies; no MCP tool, CLI subcommand/flag, data-schema, or
  config-format changes.

## Frozen contracts touched

(none — no MCP tool, CLI subcommand/flag, data schema, or config format
change. The `cli-surface` `serve subcommand` requirement is a behavioral
contract of the serve flow: its signal semantics are made explicit, its
surface (flags, subcommands) is unchanged.)

## Non-goals

- No cancellation of an in-flight task: an LLM call already in progress runs
  to completion. Making in-flight LLM calls cancellable (a cancel token
  threaded through `crates/llm`) is a separate follow-up change.
- No new CLI flag, config key, or queue task type.
- No change to the queue schema, the backoff schedule, or the restart
  recovery semantics (they are reused as-is as the safety net).
- No change to the axum graceful-shutdown bound or the 10 s forced bound.
- No per-task timeouts.
