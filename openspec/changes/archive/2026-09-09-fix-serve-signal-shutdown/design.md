# Design: fix-serve-signal-shutdown

Reference contracts: `openspec/specs/pipeline/spec.md` (`Cancellable worker
cycle` — added by this change; `Per-cycle vector persistence`),
`openspec/specs/cli-surface/spec.md` (`serve subcommand`), and the existing
recovery design (event-queue-incremental-linking task 1.5 restart recovery,
vector-loss-self-heal D1/D2).

## Root cause

The stop signal is only observable in the owner-loop `tokio::select!`
(`serve/server.rs`). The worker cycles run **synchronously inline on the
owner thread** (the `Runner` is `!Send`):

1. the **startup drain** (`worker.run_once`, `serve/server.rs` ~line 477)
   runs *before* the owner loop — with N pending `doc:index` jobs each
   running the full pipeline (slow LLM entity extraction in the field:
   ~100 s/document), the signal sits unobserved in the broadcast channel for
   the whole drain;
2. every owner-loop `Tick`/`RetryBatch` runs `run_worker_cycle` →
   `run_once` inline (up to `WORKER_BATCH_SIZE = 100` claims per cycle) with
   no cancellation check between claims.

The 10 s forced bound (`SHUTDOWN_TIMEOUT`) wraps only the axum drain +
watcher stop *after* the owner loop exits. A second Ctrl+C does nothing:
tokio's `ctrl_c` just pushes another broadcast message that the blocked
owner thread never reads.

## D1 — Shared shutdown flag (cooperative cancellation)

**Decision.** A small public newtype in `crates/ingestion`
(`worker.rs`): `ShutdownFlag` wrapping `Arc<AtomicBool>`, with
`new()`, `cancel()`, `cancelled()`, and a cheap `clone()` (clones the
`Arc`). The production signal task calls `cancel()` on the first
SIGINT/SIGTERM **before** sending on the stop broadcast; the worker checks
`cancelled()` before each claim.

**Why.** The worker is `!Send` and runs inline on the owner thread — it
cannot await anything, so the only channel into it is synchronous shared
state. An atomic flag is the minimal such channel: one atomic load per
claim (negligible), no new dependency, no message-consumption semantics.
Setting the flag *before* the broadcast send guarantees ordering: whenever
the owner loop observes the stop message, the flag is already set, and
whenever the worker sees the flag, the broadcast send has already happened
(or is happening) — so the owner loop's `stop.recv()` arm still fires and
the axum drain (a separate resubscribed receiver) still starts.

**Why not the alternatives.**
- *`tokio_util::sync::CancellationToken`* — rejected: a new workspace
  dependency (frozen stack) for exactly what `Arc<AtomicBool>` provides;
  `CancellationToken`'s async `cancelled()` is useless in a `!Send`
  synchronous context anyway.
- *`try_recv` on the stop broadcast inside the worker* — rejected: broadcast
  receivers track per-receiver positions; consuming the message in the
  worker would swallow the owner loop's stop (and the flag would still be
  needed for the pre-loop startup drain, so it does not even remove the
  flag).
- *Aborting the worker from another task* — rejected: there is no worker
  task to abort — the cycle runs inline on the owner thread by design
  (module docs, `!Send` runner).
- *Moving the worker to a spawned thread/task* — rejected: the `Runner` is
  `!Send + !Sync` by design (D1 dependency graph); a thread boundary is a
  far larger architectural change than this bug warrants.

**Where.** `crates/ingestion/src/worker.rs` (newtype + `lib.rs` export);
set from `crates/cli/src/serve/server.rs` (signal task).

## D2 — Cancellable `run_once`

**Decision.** `DocumentWorker` gains an optional flag:
`DocumentWorker::new(db, runner)` (unchanged — no flag) and
`DocumentWorker::with_shutdown_flag(db, runner, &ShutdownFlag)`. In
`run_once`, the claim loop checks the flag **at the top of every iteration**
(including before the first claim): set → `break` out of the claim loop.
The cycle **tail runs uniformly** (GC sweep, per-cycle vector persistence
when `processed > 0`, cycle summary) — no separate cancelled-tail path.

**Why.** Checking at the top of each iteration bounds the shutdown delay to
exactly one in-flight task (the task already claimed). The uniform tail
keeps one code path: the per-cycle save (vector-loss-self-heal D1) still
persists whatever the cycle indexed, and the GC is a few milliseconds of
SQL — harmless at shutdown and it avoids a branching tail that could drift.
Unclaimed rows stay `pending`; a row claimed and interrupted by the
force-exit (D4) stays `processing` and is reset by the existing
`recover_stuck_processing` on the next startup.

**Why not the alternatives.**
- *A separate "cancelled" tail (skip GC / persist)* — rejected: two tails
  that must stay in sync for no real gain; the GC cost at shutdown is
  negligible and the persist is the safety save.
- *Per-stage cancellation inside the pipeline (between parse/chunk/NER/
  embed)* — rejected: it pushes the flag through `Runner` and the ingester
  (larger surface); the in-flight LLM call is the slow stage and cannot be
  cancelled without `crates/llm` changes anyway — that is the documented
  follow-up, not this change.
- *Checking the flag only in `serve_with_stop` (not in `run_once`)* —
  rejected: the check must be *between claims inside the loop*; from outside
  the loop you cannot stop a cycle already claiming 100 tasks.

**Where.** `crates/ingestion/src/worker.rs` (`run_once`, `new`,
`with_shutdown_flag`).

## D3 — Serve wiring: flag + second-signal force-exit

**Decision.** In `serve()` the signal task becomes:

```text
first SIGINT/SIGTERM  →  flag.cancel();  broadcast send   (graceful stop)
second SIGINT/SIGTERM →  warn log;  std::process::exit(130)   (force exit)
```

`shutdown_signal()` is restructured into a helper that builds the
`ctrl_c`/`sigterm` `select!` pair (reused for both the first and the second
wait — `tokio::signal` futures are re-creatable). `serve_with_stop` gains
one parameter, `shutdown_flag: &ShutdownFlag`, and constructs the worker via
`DocumentWorker::with_shutdown_flag` — the startup drain and every
owner-loop cycle inherit the check. The **owner loop itself is unchanged**:
its `stop.recv()` arm still fires (the flag does not consume the broadcast
message), so `OwnerEvent::Stop` → break → bounded shutdown proceeds exactly
as today.

**Why.** Minimal surface: one new parameter, one new method call, the
signal task extended. The owner loop, the axum drain, the 10 s bound, and
the watcher stop are all untouched — the fix inserts a cooperative stop
*inside* the only place that was unresponsive (the worker cycle).

**Why not the alternatives.**
- *Deriving the flag from the broadcast inside `serve_with_stop`* —
  rejected: the synchronous worker cannot await a broadcast; and the test
  seam (`serve_with_stop` driven by a stub killer) would then have two
  coupled ways to signal stop instead of one explicit flag.
- *Storing the flag on `Bootstrap`* — rejected: bootstrap is pre-signal
  state assembled before the runtime exists; the flag is a serve-runtime
  concern.
- *A `--force` CLI flag for the hard stop* — rejected: new CLI surface is a
  frozen-contract change; the standard double-signal UX needs no surface.

**Where.** `crates/cli/src/serve/server.rs` (`serve`,
`shutdown_signal` → first/second helpers, `serve_with_stop` signature);
`crates/cli/tests/serve_server.rs` (all `serve_with_stop` call sites pass
the flag; the stub killer tasks `flag.cancel()` before the broadcast send,
mirroring production).

## D4 — Force-exit code 130

**Decision.** The second-signal path logs a warning and calls
`std::process::exit(130)` — 128 + SIGINT, the Unix convention for
"killed by SIGINT" (used even when the second signal is SIGTERM: the
observable contract is "forced exit").

**Why.** It gives the user a guaranteed escape hatch no matter how slow the
in-flight LLM call is, and it is **safe by design**: the recovery design
already tolerates SIGKILL-grade unclean exits (stuck `processing` rows are
reset to `pending` at startup; lost vectors are self-healed; the per-cycle
save bounds the loss window). `process::exit` skips destructors on purpose
— that is the point (a true hard stop).

**Testability.** `process::exit` is not unit-testable in-process. The
machine checks cover the *cancellation* contract (worker unit tests +
serve-level integration test through the `serve_with_stop` seam); the
force-exit path is a ~6-line signal-task extension verified by review.
An end-to-end "send two real signals to a spawned binary" test was
rejected: std has no signal-sending API without a new dependency (libc —
frozen stack), shelling out to `kill` is unavailable on the Windows CI leg,
and making the server reliably busy under the test would be flaky.

## D5 — SSE sessions terminated on graceful shutdown (task 1.4)

**Problem (found after tasks 1.1–1.3, deterministically reproduced).**
The cooperative stop works, but a `serve` with a *connected MCP client*
still takes the full 10 s bound and logs "forced shutdown after
timeout": axum's `with_graceful_shutdown` stops accepting new connections
but **waits for in-flight requests to complete**, and the legacy
`GET /sse` response is a long-lived streaming body
(`once(endpoint).chain(ReceiverStream::new(rx))`, `crates/mcp/src/transport/sse.rs`)
that ends only on client disconnect or the 300 s idle reaper — nothing
ends it on the shutdown signal. Repro: `serve --port 8081` + an open
`curl -N /sse` + SIGINT → 10.15 s + the forced-shutdown warning; the same
run with no connected client → 0.10 s + "stopped gracefully". The field
case is this repo's own `.opencode/opencode.json`, which wires the
OpenCode session to `http://localhost:8080/sse`.

**Decision.** The server ends its own long-lived SSE streams when the
graceful stop fires, so the axum drain completes:
- `SseSessionMap::close_all()` — removes every session (drops the
  `SseSession` values → drops the outbound senders → the `ReceiverStream`
  ends → the body drops → the `SessionGuard` removal path, design D6 —
  one removal path, no new one);
- `Server` retains the map: the map is created in `Server::new` (no
  signature change — `SseSessionMap::new()` takes no arguments) and
  `router()` uses `self.sessions.clone()`; `Server::close_all_sessions()`
  delegates to the map (the cli never sees `SseSessionMap`);
- the cli serve-task shutdown future (the `with_graceful_shutdown` future
  in `serve_with_stop`) calls `close_all_sessions()` **after the stop
  resolves** (both paths: the pre-set flag check and the resubscribed
  broadcast recv) — the exact moment axum transitions to draining, so the
  streams are already ending when the drain starts waiting.

**Why.** The drain waits for in-flight *requests*; a server that never
ends its own streaming responses makes that wait unbounded. Ending the
streams on shutdown is standard server behavior (the client sees a clean
EOF and can reconnect — the MCP clients reconnect on stream loss), and it
reuses the existing single-removal path (dropped sender → stream end).

**Why not the alternatives.**
- *A shared shutdown watch/broadcast that the SSE body stream selects on*
  — rejected: it changes the stream body (a larger surface in the
  wire-contract module) for what dropping the senders already achieves;
  the sender-drop path is the established removal path (design D6).
- *Accept the 10 s bound as documented behavior* — rejected: the user's
  exact scenario (connected MCP client) stays 10 s + a scary
  "forced shutdown" warning for a stop that could complete in < 1 s.
- *Aborting the serve task instead of draining* — rejected: it would cut
  genuine in-flight request/response calls at shutdown; the drain is
  correct for those — only the never-ending streams are the problem.

**Known limitations (documented, not fixed).**
- A `GET /sse` request that creates its session *after* `close_all`
  (the signal lands mid-request-handling) stays open until the 10 s
  bound reaps it — a tiny race window at the exact signal instant; the
  bound remains the escape hatch.
- The rmcp Streamable HTTP fallback (`/mcp`) is untouched: its
  in-flight responses are bounded by their tool call (they complete as
  usual within the drain); no unbounded stream was observed there.

**Contract note (human decision 2026-09-08).** The mcp-contract delta
extends the legacy-SSE session lifecycle ("a session lives until the SSE
stream is closed or 300 s of inactivity") with "or the server performs a
graceful shutdown" — the same extension-of-the-wire pattern as the idle
reaper (human decision 2026-09-01). The tool surface, the JSON-RPC
methods, and the wire frames are unchanged.

**Where.** `crates/mcp/src/transport/sse.rs` (`close_all`),
`crates/mcp/src/server.rs` (map field, `close_all_sessions`),
`crates/cli/src/serve/server.rs` (shutdown future),
`crates/cli/tests/serve_server.rs` (integration test).

## Performance (16 GB laptop)

- One atomic load per claim (per task, not per chunk) — nanoseconds;
  negligible against a claim that is a SQLite round-trip.
- No new I/O, no new locks, no runtime-context entry. The shutdown path
  cost is unchanged (axum drain + watcher stop under the 10 s bound).
- D5 adds one `Mutex`-guarded `HashMap` clear on the shutdown path
  (microseconds); it *removes* the worst case — a graceful stop with a
  connected MCP client now completes in well under the 10 s bound instead
  of waiting it out.
- Worst-case graceful-stop latency: one in-flight task (bounded by the
  slowest single pipeline run — the LLM call); the escape hatch is the
  second signal (D4).
