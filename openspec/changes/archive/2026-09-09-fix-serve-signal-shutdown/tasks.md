# Tasks: fix-serve-signal-shutdown

Read first (binding context): `openspec/config.yaml`, `AGENTS.md`, this
change's `proposal.md`, `design.md` (decisions D1–D5), and the delta specs
under `specs/pipeline/spec.md`, `specs/cli-surface/spec.md`, and
`specs/mcp-contract/spec.md`.

Root cause: the stop signal is only observed in the owner-loop `select!`,
but the document-queue worker cycles run synchronously inline on the owner
thread (`DocumentWorker::run_once` — `!Send` runner) with no cancellation
check between claims; the startup drain runs *before* the owner loop. The
10 s forced bound covers only the post-loop axum drain + watcher stop.

Second root cause (found after 1.1–1.3, design D5): even with the
cooperative stop, a `serve` with a connected MCP client over the legacy
SSE transport takes the full 10 s bound — axum's graceful drain waits for
in-flight requests, and the long-lived `GET /sse` stream never ends on the
shutdown signal (deterministic repro: open `curl -N /sse` + SIGINT →
10.15 s + "forced shutdown after timeout"; no client → 0.10 s graceful).

Order reflects dependencies: 1.1 → 1.2 → 1.3; 1.4 → 1.5 (1.4 depends on
1.2, 1.5 depends on 1.4). 1.1, 1.2 and 1.4 are Rust implementation tasks
(route to `rust-implementer`); 1.3 and 1.5 are documentation tasks (route
to `docs-writer`). Every implementation task must leave the workspace
green: `cargo fmt --check`, `cargo clippy --all-targets
-- -D warnings`, `cargo test` (whole workspace, no network/services).

- [x] **1.1** — `ShutdownFlag` + cancellable `run_once` (crates/ingestion)

**Goal.** Add the shared shutdown flag (design D1) and make
`DocumentWorker::run_once` cooperative: check the flag before each claim
(including the first); once set, stop claiming and complete the cycle tail
uniformly. Design D2. Delta spec: `specs/pipeline/spec.md`
(`Cancellable worker cycle`).

**File scope.**
- `crates/ingestion/src/worker.rs`:
  - New public newtype `ShutdownFlag` wrapping `Arc<AtomicBool>` with
    `new() -> Self`, `cancel(&self)`, `cancelled(&self) -> bool`, and a
    manual `Clone` impl (clones the `Arc`). Document the type and every
    method (the workspace denies `missing_docs`; the library crate stays
    logger-less — no `tracing` here).
  - `DocumentWorker` gains an optional flag field
    (`Option<ShutdownFlag>`… store the cheap `Clone`d handle, not the
    `Arc`, so the struct stays `!Send`-friendly as today):
    `pub fn new(db: &'a Db, runner: &'a Runner<'a>) -> Self` (unchanged
    behavior — no flag) and a new
    `pub fn with_shutdown_flag(db: &'a Db, runner: &'a Runner<'a>, flag: &ShutdownFlag) -> Self`
    (documented).
  - In `run_once`, at the top of every claim-loop iteration (before
    `claim_one`), break out of the claim loop when the flag (if present)
    reports `cancelled()`. The cycle tail (GC sweep, per-cycle vector
    persistence when `processed > 0`, cycle summary) runs uniformly — no
    separate cancelled-tail path. Update the module doc comment and the
    `run_once` doc comment to state the cancellation contract.
- `crates/ingestion/src/lib.rs` — export `ShutdownFlag` (verify the
  existing `pub use`/module pattern first).
- Tests (in the `worker.rs` test module — it already has the `Harness`,
  `MockEmbedding` with the `probe` hook (invoked before every embedding
  batch) and `fail_marker`, and `MemoryIndex` with the
  `build_index_calls` counter):
  - **Flag set before the cycle:** enqueue three `doc:index` tasks, build
    the worker via `with_shutdown_flag`, cancel the flag, `run_once` →
    assert zero documents created, all three tasks still `pending`.
  - **Flag set mid-cycle (the field case):** fresh harness, three tasks,
    the `probe` hook cancels the flag on its **first** invocation (i.e.
    during task 1's embedding) → `run_once` → assert exactly one document
    created, task 1 `done`, tasks 2–3 still `pending` (the in-flight task
    completed; no further claims).
  - **In-flight failure during shutdown:** same shape, but the embed
    `fail_marker` makes task 1 fail and the probe cancels the flag on the
    first invocation → task 1 recorded with backoff (attempts 1, `pending`,
    `next_attempt_at` advanced), tasks 2–3 untouched `pending`.
  - **Persistence on a cancelled work cycle:** the mid-cycle shape with a
    successful embed → `MemoryIndex.build_index_calls() == 1` (the tail
    ran).
  - **Worker without a flag:** the pre-existing tests (all built via
    `new`) must pass unchanged — they are the no-flag regression guard.

**Dependencies.** None (first task). References: design D1–D2; delta spec
`specs/pipeline/spec.md`; the existing `run_once` claim loop and the
`WORKER_BATCH_SIZE` guard in `crates/ingestion/src/worker.rs`; the
`probe`/`fail_marker`/`build_index_calls` hooks in the `worker.rs` test
module.

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace): `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo test`.
- The four new tests pass (before-cycle → 0 processed / all pending;
  mid-cycle → 1 processed, rest pending; in-flight failure → backoff
  recorded, no further claims; cancelled work cycle → exactly one
  `build_index`).
- All pre-existing `worker.rs` tests pass unchanged (no-flag behavior).
- `ShutdownFlag` + `with_shutdown_flag` documented; no new dependency;
  `crates/cli` untouched by this task.

**Estimated size.** ~200–280 lines (code + tests).

- [x] **1.2** — Serve wiring: flag, second-signal force-exit, integration test (crates/cli)

**Goal.** Wire the flag into `serve` (design D3): the signal task cancels
the flag on the first SIGINT/SIGTERM and force-exits with code 130 on the
second (design D4); `serve_with_stop` takes the flag and builds the worker
via `with_shutdown_flag`, making the startup drain and every owner-loop
cycle cancellable. The owner loop itself is **unchanged** (its
`stop.recv()` arm still fires — the flag does not consume the broadcast
message). Delta spec: `specs/cli-surface/spec.md` (`serve subcommand`).

**File scope.**
- `crates/cli/src/serve/server.rs`:
  - Restructure `shutdown_signal()` into a helper that builds the
    `ctrl_c`/`sigterm` `select!` pair (keeping the existing
    SIGTERM-install-failure degradation to SIGINT-only, and the existing
    log lines), reusable for **both** the first and the second wait
    (`tokio::signal` futures are re-creatable — call the helper twice).
  - `serve()`: create `let shutdown_flag = ShutdownFlag::new();` and change
    the spawned signal task to: first signal → `shutdown_flag.cancel()`
    **then** the broadcast send (order matters — design D1); second signal
    → `tracing::warn!("second shutdown signal received, forcing immediate
    exit")` + `std::process::exit(130)`. Add
    `const FORCED_EXIT_CODE: i32 = 130;` (documented: 128 + SIGINT, the
    Unix "killed by SIGINT" convention) and use it in the `exit` call.
  - `serve_with_stop`: add the parameter
    `shutdown_flag: &ShutdownFlag` (update the doc comment: the flag is the
    cooperative-cancel channel into the inline worker cycles — the startup
    drain and the owner-loop cycles), and build the worker via
    `DocumentWorker::with_shutdown_flag(&db, &runner, shutdown_flag)`.
  - Unit test (in `server.rs`): `FORCED_EXIT_CODE == 130`.
- `crates/cli/tests/serve_server.rs`:
  - Update **every** `serve_with_stop` call site (five tests): create a
    `ShutdownFlag`, pass it, and in each stub killer task call
    `flag.cancel()` **before** `stop_tx.send(())` (mirroring the production
    signal task order).
  - New integration test
    `serve_stop_cancels_the_startup_drain_mid_task`:
    - Temp dir + `test_bootstrap` shape, but the bootstrap's embed is a new
      test-local `GateEmbed` (4-dim, fixed vectors) whose
      `generate_embeddings` does, on the **first** batch only (guard with
      `AtomicBool::swap`): (a) sends a `tokio::sync::oneshot` "entered"
      token (synchronous send — fine from the owner thread), (b) calls
      `flag.cancel()` + `stop_tx.send(())` (simulating the SIGINT +
      broadcast exactly as the production signal task does), (c) blocks on
      a `std::sync::mpsc` release receiver (simulating the slow LLM call),
      then returns the fixed vectors.
    - One markdown source with **three** small `.md` files,
      `no_initial_sync: false` (the startup reconcile enqueues three
      `doc:index` rows).
    - The test spawns an async task on the runtime that awaits the
      "entered" oneshot, then sends the release token (letting the
      in-flight task finish).
    - Run `serve_with_stop` on the test thread (the owner thread).
    - Assert: the result is `Ok`; exactly **one** document is indexed and
      its task row is `done`; the other two task rows are still
      `pending` (the drain stopped after the in-flight task — the field
      bug's exact scenario); the process path returned (no hang).
- No other crate changes; no new dependency.

**Dependencies.** 1.1 committed (`ShutdownFlag` + `with_shutdown_flag`
exist). References: design D3–D4; delta spec `specs/cli-surface/spec.md`;
`serve()` / `serve_with_stop` / `shutdown_signal` in
`crates/cli/src/serve/server.rs` (the signal task, the startup drain at
the `worker.run_once` call, the owner-loop `select!`); the existing
killer-task pattern and `test_bootstrap` / `FakeEmbed` / `MemIndex` in
`crates/cli/tests/serve_server.rs`.

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace): `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo test`.
- The new integration test passes: stop during the startup drain → 1 task
  `done`, 2 tasks `pending`, `serve_with_stop` returns `Ok` (no hang).
- All five pre-existing `serve_with_stop` tests pass with the updated
  call sites.
- `FORCED_EXIT_CODE == 130` unit test passes.
- The owner-loop `select!` and the 10 s bound are untouched (verify by
  reading the diff); no new dependency; no frozen-contract change.

**Estimated size.** ~250–350 lines (code + tests).

**Revision history.**
- Rev 1 (after first review, verdict approve + nit): the axum serve task's
  shutdown future only awaits its resubscribed receiver; a stop broadcast
  sent before the serve task spawns (a signal during the startup drain) is
  missed by the resubscribed receiver (tokio broadcast: a receiver only
  sees messages sent after subscription), so the bounded shutdown waits the
  full `SHUTDOWN_TIMEOUT` on `serve_handle.await` and logs a misleading
  "forced shutdown after timeout" for the user's exact scenario. Fix: at
  the start of the serve task's shutdown future, check
  `shutdown_flag.cancelled()` and resolve immediately when set (the flag is
  cancelled before the broadcast send, and the serve task spawns after the
  drain — the check deterministically closes the window; a signal after the
  check is still delivered via the resubscribed receiver). The new
  integration test must then take the graceful path (well under
  `SHUTDOWN_TIMEOUT`), and the test doc comment must describe the actual
  execution path. Note: the owner loop's original receiver is NOT affected
  (it was subscribed at channel creation; buffered messages are delivered
  on the first `recv()`) — only the resubscribed axum receiver misses
  pre-subscription messages.

- [x] **1.3** — Documentation: shutdown semantics (README + site/docs)

**Goal.** Document the new observable shutdown behavior (AGENTS.md rule:
behavior changes ship with their docs in the same change). Delta spec:
`specs/cli-surface/spec.md` (the new `serve subcommand` scenarios are the
source of truth for the wording).

**File scope.**
- `site/docs/concepts/job-queue.mdx` — in/after the "The worker and
  retries" section, a short "Stopping the server" section: the first
  SIGINT/Ctrl+C is a cooperative graceful stop (the in-flight queue task
  finishes and is recorded, no further tasks are claimed, the server
  drains and exits 0 — this also applies to the startup drain); a second
  SIGINT/Ctrl+C forces an immediate exit (code 130); unclaimed tasks stay
  `pending` and are picked up on the next start (link the existing
  restart-recovery and self-heal sections).
- `README.md` — one or two sentences in the Quick start (or CLI) section:
  stop with Ctrl+C (graceful, finishes the in-flight document); press
  Ctrl+C again to force an immediate stop.
- No code changes.

**Dependencies.** 1.2 committed (the behavior is final). References: delta
spec `specs/cli-surface/spec.md` scenarios; the existing
`site/docs/concepts/job-queue.mdx` and `site/docs/concepts/vector-rebuild.mdx`
(Unclean shutdown) wording for style; `README.md` Quick start section.

**Acceptance criteria (machine-checked).**
- The site page states both semantics (graceful first signal bounded by the
  in-flight task; second signal → immediate exit) and that unclaimed tasks
  survive to the next start; the README mentions the stop behavior.
- `site/docs` builds (no broken links/anchors); no code files touched.

**Estimated size.** ~40–80 lines of docs.

- [x] **1.4** — Terminate SSE sessions on graceful shutdown (crates/mcp + crates/cli)

**Goal.** Fix design D5: a `serve` with a connected legacy-SSE MCP client
still takes the full 10 s bound on Ctrl+C, because axum's
`with_graceful_shutdown` waits for in-flight requests and the long-lived
`GET /sse` stream never ends on the shutdown signal. The server must end
its own SSE streams when the graceful stop fires, so the axum drain
completes promptly. Read design D5 for the full rationale, the rejected
alternatives, and the documented known limitations (the mid-request race
window; the rmcp Streamable HTTP fallback is untouched — its in-flight
responses are bounded and drain as usual).

**File scope.**
- `crates/mcp/src/transport/sse.rs` — `SseSessionMap::close_all()`:
  remove every session (drop the `SseSession` values → drop the outbound
  senders → the `ReceiverStream` ends → the body drops → the existing
  `SessionGuard` removal path, design D6 — no new removal path). Returns
  the number of sessions removed. Document the shutdown contract.
- `crates/mcp/src/server.rs` — `Server` retains the map: create
  `SseSessionMap::new()` in `Server::new` (no signature change), add the
  field, `router()` uses `self.sessions.clone()` instead of constructing a
  fresh map (the idle-reaper `spawn_reaper` call stays where it is), and
  add `Server::close_all_sessions()` delegating to the map. The cli never
  sees `SseSessionMap`.
- `crates/cli/src/serve/server.rs` — the serve-task shutdown future (the
  `with_graceful_shutdown` future in `serve_with_stop`) calls
  `close_all_sessions()` **after the stop resolves** — i.e. after BOTH the
  pre-set `shutdown_flag.cancelled()` check AND the resubscribed
  `stop_for_serve.recv().await` have run — the exact moment axum
  transitions to draining. Capture the `mcp::Server` handle (a cheap
  clone) into the serve task for this call.
- `crates/cli/tests/serve_server.rs` — an integration test (see
  Acceptance). No changes to the existing `serve_with_stop` call sites
  (the `Server` is built inside `serve_with_stop`; the map is internal).
- No new dependencies. No changes to the MCP tool surface, the JSON-RPC
  methods, the wire frames, the CLI surface, the data schema, or config
  formats.

**Design (bind to this — see D5 for why).**
- The map is created once in `Server::new` and shared by every `Arc`
  clone (the Streamable HTTP factory and the SSE routes already share one
  `Arc<Server>`); `router()` consumes `self` and moves the map into the
  axum state — so the map must be cloned into `SseState` from the field,
  not newly constructed.
- `close_all` is synchronous (a `Mutex`-guarded `HashMap` clear,
  microseconds) — safe to call from the async shutdown future.
- The idle reaper holds its own `Arc` clone of the map; after `close_all`
  the map is empty and the reaper's passes are no-ops — no interference.
- Ordering: the call goes at the END of the shutdown future body (after
  the flag check / broadcast recv), so it runs the moment axum starts
  draining — the streams are already ending when the drain starts waiting.

**Dependencies.** 1.2 committed (the `serve_with_stop` shutdown future, the
`shutdown_flag` parameter, and the resubscribed-receiver structure exist).
References: `crates/mcp/src/transport/sse.rs` (`SseSessionMap`,
`handle_sse`, `SessionGuard`, design D6), `crates/mcp/src/server.rs`
(`Server::new`, `router()`), `crates/cli/src/serve/server.rs`
(`serve_with_stop` shutdown future, `serve_handle`),
`crates/cli/tests/serve_server.rs` (existing `serve_with_stop` test
scaffolding: `test_bootstrap`, `free_port`, the stub-killer pattern).

**Acceptance criteria (machine-checked).**
- `SseSessionMap::close_all` unit test: create ≥2 sessions (keeping the
  receiver ends), call `close_all()`, then `len() == 0` / `is_empty()`,
  and each kept `ReceiverStream` yields `None` (the stream ended).
- A `Server` built via `Server::new` exposes `close_all_sessions()`;
  `Server::new`'s signature is UNCHANGED (no new parameters).
- Integration test in `serve_server.rs` (the field scenario, design D5):
  a `serve_with_stop` run (temp db, fake embed, no sources) on a free port
  where a spawned task (a) waits for `/health` 200, (b) opens a `GET /sse`
  streaming request and reads the `endpoint` frame (confirming the
  session is registered), (c) cancels the shutdown flag and sends the stop
  broadcast (production order, mirroring the existing stub killers). The
  test asserts: `serve_with_stop` returns `Ok`, the stop took the graceful
  path (`stopped < SHUTDOWN_TIMEOUT` — NOT the 10 s bound), and the SSE
  response body stream ended (the client saw EOF / the stream yielded
  `None`).
- All pre-existing `serve_with_stop` tests still pass unchanged
  (no SSE client → `close_all` on an empty map is a no-op).
- Workspace green: `cargo fmt --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test` (whole workspace).

**Estimated size.** ~250–350 lines (code + tests).

- [x] **1.5** — Documentation: SSE sessions end on graceful shutdown (site/docs)

**Goal.** Document the D5 behavior (AGENTS.md rule: behavior changes ship
with their docs in the same change). Source of truth for the wording: the
delta spec `specs/cli-surface/spec.md` (`serve subcommand`, the new
"Graceful stop with an active SSE session" scenario + the SSE sentence in
the shutdown paragraph) and `specs/mcp-contract/spec.md` (`Transport`,
"Sessions on graceful shutdown").

**File scope.**
- `site/docs/concepts/job-queue.mdx` — in the "Stopping the server"
  section (added by task 1.3), one or two sentences: as part of the
  graceful stop the server ends its own long-lived MCP SSE client streams
  (connected clients see the stream close and can reconnect), so the stop
  completes promptly even with connected clients.
- No code changes. `README.md` is NOT touched (its two-sentence stop note
  from task 1.3 remains accurate).

**Dependencies.** 1.4 committed (the behavior is final). References: the
delta specs above; the existing `site/docs/concepts/job-queue.mdx`
"Stopping the server" section.

**Acceptance criteria (machine-checked).**
- The site page states that the server ends its own SSE client streams on
  a graceful stop (clients see the stream close / can reconnect).
- `site/docs` builds (no broken links/anchors); no code files touched.

**Estimated size.** ~5–15 lines of docs.
