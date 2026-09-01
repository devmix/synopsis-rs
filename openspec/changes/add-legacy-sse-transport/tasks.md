# Tasks: add-legacy-sse-transport

Change: add-legacy-sse-transport · Read first: `design.md` (all decisions D1–D9
+ the deployment-model note),
`proposal.md`, `openspec/changes/add-legacy-sse-transport/specs/mcp-contract/spec.md`.

**Change header (applies to every task).** Oracle: `../synopsis` is READ-ONLY —
never create/modify/delete anything there. Wire reference for the legacy SSE
transport: mcp-go **v0.57.0** (pinned in `../synopsis/go.mod`) — read
`server/sse.go` from the Go module cache
(`$(go env GOMODCACHE)/github.com/mark3labs/mcp-go@v0.57.0/server/sse.go`)
or the v0.57.0 tag on GitHub; the oracle's mount/shutdown code is
`../synopsis/internal/mcp/server.go` (lines ~114–155). All produced content in
English (code, comments, tests, docs). Gates: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
Frozen stack: no NEW crates in the dependency tree — `sse-stream`, `uuid`,
`tokio-stream` are already transitive deps (Cargo.lock); they become direct
workspace deps (design D3). `../synopsis` and `openspec/changes/archive/**` are
untouchable.

Order: 1.1 → 1.2 → 1.3 → 1.4 → 1.5. Each task leaves the workspace compiling
with all gates green.

- [x] 1.1 SSE session core + `GET /sse` handler (transport module skeleton, general-service-hardened)
- [ ] 1.2 `POST /message` + minimal JSON-RPC 2.0 method table
- [ ] 1.3 Router composition + main-spec Purpose update
- [ ] 1.4 parity-harness SSE client + cross-transport parity tests
- [ ] 1.5 Session idle timeout + reaper (general-service hardening)

---

## Task 1.1 — SSE session core + `GET /sse` handler (transport module skeleton, general-service-hardened)

**Goal.** Create the `transport` module in `crates/mcp` with the SSE session
model and the `GET /sse` handler: session creation, the `endpoint` event
(proxy-aware absolute URL), the JSON-RPC event stream, bounded per-session
channels with a `send()` helper, activity tracking (`last_activity`/`touch`),
and disconnect cleanup. No routing yet (task 1.3 wires the routes) and no idle
reaper yet (task 1.5); the handler is unit/integration-tested in isolation.

**Read first.** `design.md` D1–D3, D7, D9 (for the fields D9's reaper will
consume) and the deployment-model note at the top of Context;
`crates/mcp/src/server.rs` (the `Server` struct, `router()`, the existing
`router_serves_health_on_explicit_route` test pattern for in-process axum
tests); mcp-go v0.57.0 `sse_server.go` (the `handleSSE` function — exact
headers, endpoint event frame, session ID generation, disconnect handling);
`../synopsis/internal/mcp/server.go` lines ~114–155 (oracle mount,
ReadHeaderTimeout).

**Scope of files (exact).**
- `crates/mcp/src/transport/mod.rs` (NEW) — module doc (dual-transport context,
  D8 override, general-service deployment model, the two justified oracle
  deviations — no wildcard CORS, proxy-aware endpoint URL — shutdown behavior
  per design D6), `pub mod sse;`, re-exports.
- `crates/mcp/src/transport/sse.rs` (NEW):
  - `SseSession { tx: mpsc::Sender<String>, last_activity: std::time::Instant }`
    (payload = one JSON-RPC object string; `tx` is PRIVATE — design D3
    revision);
  - `SseSessionMap` = `Arc<std::sync::Mutex<std::collections::HashMap<String,
    SseSession>>>` with:
    - `create() -> (id, rx)` — UUIDv4 id, **bounded** `mpsc::channel(64)`
      (design D3 revision: backpressure under the 16 GB constraint),
      `last_activity = Instant::now()`;
    - `send(&self, id, payload: String) -> Result<(), SendError>` — clones the
      sender and awaits `tx.send(payload)` under a **1 s `tokio::time::timeout`**;
      on timeout removes the session (a client that cannot drain 64 small
      frames in 1 s is dead — design D3) and returns `Err`; on channel-closed
      removes the session and returns `Err`;
    - `touch(&self, id) -> bool` — refreshes `last_activity` (task 1.2's POST
      handler calls it on every request; task 1.5's reaper reads it);
    - `get(&id) -> Option<mpsc::Sender<String>>` (tests/internal),
      `remove(&id) -> bool`, `len()`, `is_empty()`.
  - `handle_sse` axum GET handler: generate UUIDv4, insert session, respond
    `200` `Content-Type: text/event-stream` + `Cache-Control: no-cache` (per
    mcp-go v0.57.0 — verify its exact headers) and **NO
    `Access-Control-Allow-Origin` header** (design D2 revision — deliberate
    break from the oracle's wildcard CORS); first frame `event: endpoint` with
    `data:` = absolute URL `<scheme>://<host>/message?sessionId=<id>` where
    scheme = `X-Forwarded-Proto` (first value if comma-list, lowercased) else
    `http`, host = `X-Forwarded-Host` else the `Host` header (design D2
    revision — proxy-aware); then stream channel payloads as `event: message`
    frames (via `sse-stream` `Sse` + `tokio-stream` `ReceiverStream` over the
    mpsc Receiver → axum body); on client disconnect (axum `Request::abort`)
    remove the session from the map.
  - SSE frame helper(s) using `sse-stream` (correct `event:`/`data:` framing
    incl. multi-line data escaping).
- `crates/mcp/src/lib.rs` — `pub mod transport;` (+ re-export if useful).
- `Cargo.toml` (workspace) — palette entries for `sse-stream` (0.2), `uuid`
  (v4 feature), `tokio-stream`; `crates/mcp/Cargo.toml` — the three deps.
  ALL are already in Cargo.lock (verify with `cargo tree` before adding — if any
  is NOT in the tree, STOP and report: frozen stack).

**Out of scope.** `POST /message` + JSON-RPC (task 1.2), router wiring (task
1.3), parity-harness (task 1.4), the idle reaper (task 1.5), any change to
`Server::dispatch` or the Streamable HTTP path.

**Dependencies.** None.

**Acceptance criteria (machine-checked).**
1. Unit tests (inline `#[cfg(test)]` in sse.rs): endpoint frame bytes exactly
   `event: endpoint\ndata: <scheme>://<host>/message?sessionId=<id>\n\n` for
   ALL THREE cases — (a) no proxy headers → `http://` + `Host` header, (b)
   `X-Forwarded-Proto: https` + `X-Forwarded-Host: svc.example.com` →
   `https://svc.example.com/…`, (c) comma-list `X-Forwarded-Proto: https, http`
   → first value wins; response headers include `text/event-stream` +
   `no-cache` and contain NO `access-control-allow-origin`; a channel payload
   becomes exactly `event: message\ndata: <json>\n\n`; a payload containing
   `\n` is escaped per the SSE spec (assert the escaped form); session map
   create/get/remove/touch round-trip; unknown-id `get`/`touch` → None/false;
   **backpressure**: fill a session's channel to capacity (64), then
   `try_send`/`send` of a 65th frame fails (full), and `send()` of a frame to a
   session whose receiver was dropped removes the session and returns `Err`.
2. In-process axum test: a Router with ONLY `GET /sse` (built in the test, not
   `Server::router` yet) returns 200 `text/event-stream` and the first frame is
   the endpoint event (proxy-aware URL); after the test client drops, the
   session is removed (map empty) — use a `tokio::time::timeout` bounded wait,
   no sleeps.
3. `cargo test -p mcp` green; `cargo fmt --all --check` clean;
   `cargo clippy -p mcp --all-targets -- -D warnings` clean;
   `cargo check --workspace` green.
4. `cargo tree -i sse-stream` / `-i uuid` / `-i tokio-stream` show no NEW crate
   versions appearing in Cargo.lock (git diff Cargo.lock contains no added
   packages).

**Oracle reference.** `../synopsis/internal/mcp/server.go` (mount + shutdown),
mcp-go v0.57.0 `server/sse.go` `handleSSE` (wire frames, headers).
Note: the oracle's `handleSSE` hardcodes `http://` (empty baseURL) and emits
`Access-Control-Allow-Origin: *` — both are DELIBERATE deviations here per
design D2 (general-service model, user decision 2026-09-01); pin the REST of
the wire (event names, frame shape, session id, disconnect handling) from the
v0.57.0 source.

**Revision history.**
- **Rev 1 (2026-09-01)** — after rust-reviewer `request_changes` + user
  decision: deployment model changed from "personal laptop" to "self-hosted
  general-use service" (memory constraint unchanged). Changes vs rev 0:
  (1) endpoint URL is proxy-aware (`X-Forwarded-Proto`/`X-Forwarded-Host`,
  fallback `http`+`Host`) — reviewer issue, now genuinely needed; (2) NO
  wildcard `Access-Control-Allow-Origin` — justified oracle deviation;
  (3) bounded channel (64) + `SseSessionMap::send()` helper replacing `pub tx`
  — backpressure + encapsulation (reviewer issue); (4) `last_activity`/`touch()`
  added now so task 1.5's reaper has its seam. Rev 0's staged code is replaced
  in place.

---

## Task 1.2 — `POST /message` + minimal JSON-RPC 2.0 method table

**Goal.** Implement the client→server leg: `POST /message?sessionId=<id>`
parses JSON-RPC 2.0, dispatches through the existing `Server::dispatch`, and
pushes responses/notifications onto the session's SSE channel. 202 Accepted on
success; 400 on missing/unknown session; correct JSON-RPC error codes.

**Read first.** `design.md` D2, D4; mcp-go v0.57.0 source (READ-ONLY):
`server/sse.go` `handleMessage` (status codes, session errors) AND
`server/server.go` (the `initialize` handler — EXACT result shape:
protocolVersion negotiation rule, `capabilities` for a tools-only server,
`serverInfo`; the `ping`, `tools/list`, `tools/call` handlers; how a tool
error is framed — JSON-RPC error vs result `isError: true`; the unknown-method
error; batch request handling). Pin every shape from the v0.57.0 source and
record the pinned shapes as comments at the point of use.

**Scope of files (exact).**
- `crates/mcp/src/transport/sse.rs` — extend with:
  - JSON-RPC 2.0 types: request (`jsonrpc`, `id: Option<Value>`, `method`,
    `params: Option<Value>`), response, error object (codes: -32700 parse,
    -32602 invalid params, -32601 method not found);
  - `handle_message` axum POST handler: extract `sessionId` query param
    (missing → 400; unknown → 400, mcp-go v0.57.0 status/body — pin it);
    parse body as JSON-RPC request (parse error → -32700 pushed to the session
    channel, HTTP 202); dispatch by method:
    - `initialize` → result pinned from mcp-go v0.57.0 (protocolVersion rule,
      tools-only capabilities, `serverInfo` = `Server.name`/`version` — the
      same values the Streamable HTTP path reports);
    - `notifications/initialized` → no response (notification);
    - `ping` → `{}`;
    - `tools/list` → `{ tools: [ … ] }` from `Server::tools()` (the same 12
      Tool definitions the Streamable HTTP path serves);
    - `tools/call` → `Server::dispatch(name, args)`: success → result
      `content` per the existing tool payload convention (same JSON as the
      Streamable HTTP path produces for the same call); `McpError` → MCP tool
      result `isError: true` with the error text (MCP convention — verify
      mcp-go v0.57.0 does the same); unknown tool name → JSON-RPC error
      (mcp-go v0.57.0 behavior — pin it);
    - any other method → -32601 (resources/prompts/completions included —
      the oracle's tools-only server does not register them);
    - batch requests → rejected per mcp-go v0.57.0 (pin exact behavior);
  - on every request, call `SseSessionMap::touch(id)` (refreshes
    `last_activity` for task 1.5's reaper);
  - responses are pushed as `event: message` frames via
    `SseSessionMap::send(id, payload)` (task 1.1's bounded channel — NOT a raw
    `tx`, which is private); `send()` `Err` (session reaped/dead mid-send) →
    HTTP error status pinned from mcp-go v0.57.0's failed-send behavior;
  - the HTTP response on success is `202 Accepted` with an empty body
    (mcp-go v0.57.0 — verify).
- If sse.rs exceeds ~600 lines total, split the JSON-RPC types + method table
  into `crates/mcp/src/transport/jsonrpc.rs` (NEW) and `pub mod jsonrpc;` in
  `transport/mod.rs`.

**Out of scope.** Router wiring (task 1.3), parity-harness (task 1.4), any
change to `Server::dispatch`/tool handlers (the SSE path consumes their output
as-is), Streamable HTTP behavior.

**Dependencies.** Task 1.1 (session map + channel + frame helper).

**Acceptance criteria (machine-checked).**
1. Unit tests: JSON-RPC parse (valid request, missing id on a request → error
   per JSON-RPC spec, parse error → -32700); method table: initialize result
   shape (assert the pinned fields incl. serverInfo name/version equality with
   `Server`'s values), ping, tools/list returns exactly 12 tools with the same
   names as `Server::tools()`, tools/call success + tool-error (`isError: true`)
   + unknown-tool framing, unknown method → -32601; `notifications/initialized`
   produces NO channel message.
2. In-process axum test (Router with `/sse` + `/message` built in the test):
   full round-trip — GET /sse → read endpoint event → POST initialize +
   tools/list + a tools/call against a `Server` built on a temp fixture DB →
   responses arrive over the SSE stream in order; POST with unknown sessionId
   → 400; POST without sessionId → 400.
3. `cargo test -p mcp` green; fmt + clippy clean; `cargo check --workspace`
   green.
4. Every mcp-go-pinned shape (initialize result, capabilities, error
   statuses/bodies) has a `// mcp-go v0.57.0 <file>: <behavior>` comment at the
   point of use.

**Oracle reference.** mcp-go v0.57.0 `server/sse.go` + `server/server.go`
(module cache, read-only); `../synopsis/internal/mcp/server.go` (the oracle
wraps `s.server` — a tools-only mcp-go MCPServer).

---

## Task 1.3 — Router composition + main-spec Purpose update

**Goal.** Mount `GET /sse` + `POST /message` on the real `Server::router()`
alongside `/health` and the Streamable HTTP fallback, sharing one `Arc<Server>`
and one `SseSessionMap`; verify coexistence; update the mcp-contract main spec
Purpose sentence (design D8) and module docs.

**Read first.** `design.md` D5, D6, D8; `crates/mcp/src/server.rs` `router()`
(lines ~74–94) and its module doc; `crates/cli/src/serve/server.rs` (the axum
`serve` call site — check for `header_read_timeout`); `openspec/specs/mcp-contract/spec.md`
(Purpose, line ~5).

**Scope of files (exact).**
- `crates/mcp/src/server.rs` — `router()`: build `Arc<Server>` +
  `Arc<SseSessionMap>`; add `.route("/sse", get(transport::sse::handle_sse))`
  and `.route("/message", post(transport::sse::handle_message))` BEFORE
  `.fallback_service(service)`; update the `router()` doc comment (dual
  transport: explicit `/health`, `/sse`, `/message` routes + Streamable HTTP
  fallback; D8 override note with the user decision date).
- `crates/mcp/src/lib.rs` / `crates/mcp/src/transport/mod.rs` — module doc
  updates (shutdown behavior per design D6: no CloseSessions equivalent —
  connection close fires Request::abort → session removal).
- `openspec/specs/mcp-contract/spec.md` — DIRECT Purpose edit (permitted by the
  OpenSpec workflow for existing capabilities; design D8): replace the sentence
  "Транспорт намеренно модернизирован: Streamable HTTP вместо legacy SSE
  оракула (design D8, решение человека 2026-08-18)." with a sentence stating
  dual transport: Streamable HTTP (rmcp, design D8) + legacy SSE oракула
  (override D8, решение человека 2026-08-31; wire-контракт mcp-go v0.57.0).
  Do NOT touch the "Транспорт" requirement body (the delta carries it at sync
  time).
- `crates/cli/src/serve/server.rs` — ONLY if the axum serve call lacks
  `header_read_timeout`: add `.header_read_timeout(std::time::Duration::from_secs(10))`
  with a comment (oracle parity: `ReadHeaderTimeout: 10s`,
  `../synopsis/internal/mcp/server.go` line ~129). If it already exists, skip.

**Out of scope.** New CLI flags/config (none — both transports always on),
tool handler changes, Streamable HTTP behavior changes, parity-harness.

**Dependencies.** Tasks 1.1 + 1.2.

**Acceptance criteria (machine-checked).**
1. In-process test on the REAL `Server::router()` (temp fixture DB):
   `GET /health` → 200 (existing behavior unchanged); `GET /sse` → endpoint
   event; `POST /message` round-trip works; a Streamable HTTP request (rmcp
   client, reuse the existing test pattern if present in server.rs tests) still
   completes initialize + tools/list on the same router instance.
2. `rg -n "legacy SSE|/sse" crates/mcp/src/server.rs` — the router doc comment
   names both transports and the D8 override.
3. `openspec/specs/mcp-contract/spec.md` Purpose contains "2026-08-31" and no
   longer says legacy SSE is "намеренно не поддерживается"; the "Транспорт"
   requirement body is UNCHANGED (git diff shows only the Purpose line).
4. `cargo test --workspace` green; `cargo fmt --all --check` +
   `cargo clippy --workspace --all-targets -- -D warnings` clean;
   `openspec validate add-legacy-sse-transport` passes.

**Oracle reference.** `../synopsis/internal/mcp/server.go` lines ~114–155
(mux composition, ReadHeaderTimeout, shutdown).

---

## Task 1.4 — parity-harness SSE client + cross-transport parity tests

**Goal.** Add a minimal legacy-SSE client to `parity-harness` and a
cross-transport parity test proving the SSE path serves the same tool
responses as the Streamable HTTP path against the same fixture DB.

**Read first.** `design.md` D7; `crates/parity-harness/src/mcp_client.rs`
(existing client + `nearest_rank_percentile` + test patterns to mirror),
`crates/parity-harness/src/fixtures.rs` (fixture loading),
`crates/parity-harness/tests/parity_test.rs` (how the in-process product server
is booted for tests); `crates/parity-harness/Cargo.toml` (reqwest is already a
dep with `default-features = false`).

**Scope of files (exact).**
- `crates/parity-harness/Cargo.toml` — add the `stream` feature to the existing
  `reqwest` dep (no new crate; verify `reqwest/stream` pulls nothing new into
  Cargo.lock — if it does, STOP and report).
- `crates/parity-harness/src/sse_client.rs` (NEW) — `SseClient`:
  `connect(base_url) -> (session_id, message_stream)` (GET /sse, parse the
  endpoint event, extract sessionId, yield incoming `event: message` payloads
  as `serde_json::Value` via manual line parsing — the format is `event:`/
  `data:` lines, no new crate); `send(session_id, json_rpc_request)` (POST
  /message, assert 202); `call_tool(session_id, name, args) -> Value`
  (builds the tools/call request, waits for the matching response by `id`);
  `initialize(session_id) -> Value`; `list_tools(session_id) -> Value`.
  Module doc: legacy SSE wire contract + mcp-go v0.57.0 reference.
- `crates/parity-harness/src/lib.rs` — `pub mod sse_client;`.
- `crates/parity-harness/tests/sse_parity.rs` (NEW) — boot the product server
  the same way `parity_test.rs` does (temp workspace/fixture DB), then:
  1. `tools/list` via SSE == `tools/list` via Streamable HTTP (same 12 names +
     schemas, deep-equal JSON);
  2. `initialize` via SSE: serverInfo name/version == the Streamable HTTP
     initialize serverInfo; (if a Go-recorded initialize fixture exists per the
     fixture convention, deep-equal against it too — ANN-free, deterministic);
  3. 2–3 representative `tools/call`s (pick read-only tools that work on the
     fixture DB — e.g. search + an entity/graph tool; mirror the tool set the
     existing parity tests exercise) via BOTH transports → deep-equal JSON,
     excluding fields the data/search specs mark approximate (ANN scores) —
     reuse the existing diff utilities from `parity-harness::diff` if present.
- `crates/parity-harness/tests/` — if a Go-recorded `initialize` fixture is
  created: file under the existing fixtures dir with a header comment stating
  the source (Go binary `../synopsis` recorded 2026-09-01, or source-derived
  from mcp-go v0.57.0 if the Go binary was not buildable — say which).

**Out of scope.** Changes to `crates/mcp` (done in 1.1–1.3), the Streamable
HTTP client, p50/p95 latency gates (SSE latency is not a frozen gate for this
change), any change to existing parity tests.

**Dependencies.** Tasks 1.1–1.3 (the SSE routes must be live on
`Server::router()`).

**Acceptance criteria (machine-checked).**
1. `cargo test -p parity-harness` green, including the new `sse_parity` tests
   (all three parity assertions above).
2. `cargo test --workspace` green; `cargo fmt --all --check` +
   `cargo clippy --workspace --all-targets -- -D warnings` clean.
3. `git diff Cargo.lock` adds NO packages (reqwest `stream` feature only).
4. `rg -n "TODO|FIXME" crates/parity-harness/src/sse_client.rs` → empty (the
   client is complete, not a stub).

**Oracle reference.** Wire contract: mcp-go v0.57.0 `server/sse.go`;
response shapes: `../synopsis/internal/mcp/tools.go` + the Go-recorded fixtures
(existing convention in `crates/parity-harness`).

---

## Task 1.5 — Session idle timeout + reaper (general-service hardening)

**Goal.** Prevent session leaks from proxy-held idle SSE connections: add an
idle reaper to `SseSessionMap` (design D9) and spawn it from
`Server::router()`. The oracle has no idle timeout (single local user); a
general service must reap sessions idle beyond the threshold (user decision
2026-09-01).

**Read first.** `design.md` D3 (session fields, `touch()`), D6 (shutdown), D9
(reaper semantics, defaults: threshold 300 s, tick 30 s);
`crates/mcp/src/transport/sse.rs` (task 1.1's `SseSession.last_activity`,
`SseSessionMap::touch`); `crates/mcp/src/server.rs` `router()` (task 1.3's
composition — where the reaper is spawned).

**Scope of files (exact).**
- `crates/mcp/src/transport/sse.rs` — extend `SseSessionMap`:
  - `with_idle_timeout(dur) -> Self` constructor (default 300 s via
    `SseSessionMap::new()`/`Default`); store the threshold + tick (30 s) in
    the map;
  - `spawn_reaper(self) -> tokio::task::JoinHandle<()>` — a detached tokio
    task: every tick, remove sessions whose `last_activity` is older than the
    threshold. Removal drops the session's `tx` → the SSE stream ends → the
    body drops → the existing disconnect guard fires (ONE removal code path —
    do not add a second one).
- `crates/mcp/src/server.rs` — `router()`: after building the
  `SseSessionMap`, call `sessions.spawn_reaper()` (detached; no `JoinHandle`
  held — document why in a comment: the task is process-lifetime and
  self-terminating when the map drains).
- `crates/mcp/src/transport/mod.rs` — module doc: add the idle-timeout
  behavior (threshold, tick, that it is a deliberate oracle deviation per the
  general-service model).

**Out of scope.** A config knob for the threshold (the oracle has no such
surface — inventing frozen config is a separate explicit decision); keep-alive
pings (design D2: disabled, reaper + disconnect guard cover dead sessions);
any change to `POST /message` (task 1.2 already calls `touch()`).

**Dependencies.** Tasks 1.1 (`last_activity`/`touch`), 1.3 (router
composition).

**Acceptance criteria (machine-checked).**
1. Unit tests (inline, `tokio::time::pause()` + short threshold — deterministic,
   no real waiting): a session with no `touch()` is reaped after the threshold
   (map empty, its `tx` dropped → a pending `send()` returns `Err`); a session
   `touch()`ed within the threshold survives; the reaper does not remove
   freshly-created sessions; `spawn_reaper` is idempotent-safe (spawning twice
   is harmless or prevented — pick one, test it).
2. In-process axum test: `GET /sse` with a short-threshold map → the SSE
   stream ENDS (client observes EOF) after the idle threshold with no POSTs;
   with periodic `touch()` (POST /message) the stream stays open past the
   threshold.
3. `Server::router()` test (or the existing router test in server.rs): the
   reaper is spawned (e.g. a session created via `/sse` on the real router is
   reaped after a short threshold — if the threshold is not injectable through
   `Server`, test the reaper at the map level only and assert `router()` calls
   `spawn_reaper` via a code-inspection comment + the unit tests above; do NOT
   add a config surface).
4. `cargo test --workspace` green; `cargo fmt --all --check` +
   `cargo clippy --workspace --all-targets -- -D warnings` clean.

**Oracle reference.** None — the oracle has no idle timeout (this is a
deliberate general-service deviation, design D9). Disconnect-cleanup semantics
mirror mcp-go v0.57.0 `handleSSE`'s `defer s.sessions.Delete(sessionID)`.
