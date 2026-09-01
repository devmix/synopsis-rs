# Design: add-legacy-sse-transport

## Context

- **Deployment model (user decision 2026-09-01, supersedes the "personal
  laptop" assumption):** the server is a self-hosted service for general use —
  potentially behind a reverse proxy / TLS termination, with multiple
  concurrent clients. The **16 GB memory constraint is unchanged** (disk-backed
  quantized ANN per ADR 0004, no embedding model on the query path). Design
  decisions below that relied on "single user, no TLS" are revised accordingly
  (marked *revised 2026-09-01*).
- Oracle: `../synopsis/internal/mcp/server.go` — `mcpserver.NewSSEServer(s.server)`
  (mcp-go **v0.57.0**, pinned in `../synopsis/go.mod`) mounted at `mux.Handle("/", sseSrv)`
  with `mux.HandleFunc("/health", ...)`; `ReadHeaderTimeout: 10s`; graceful shutdown
  via `sseSrv.CloseSessions()` then `httpSrv.Shutdown(5s)`. mcp-go's SSEServer
  defaults: `SSEEndpoint=/sse`, `MessageEndpoint=/message`,
  `UseFullURLForMessageEndpoint=true`, keep-alive disabled.
- Rust current state: `crates/mcp/src/server.rs` — `Server::router(self)` builds
  `Router::new().route("/health", ...).fallback_service(StreamableHttpService)`;
  `Server::dispatch(&self, name, args) -> Result<Value, McpError>` (line ~140) is
  the shared tool-execution seam; `Server` is `Clone` (the Streamable HTTP service
  wraps `Arc<Server>` and clones per request).
- rmcp 3.x: server transports = stdio + Streamable HTTP only. Its SSE
  primitives (`sse_stream` crate, `sse_stream_response` in `server_side_http.rs`)
  are `pub(crate)` — not usable as a server transport.

## Decisions

### D1 — Hand-roll the SSE transport around `Server::dispatch` (NOT rmcp's Transport trait)

The SSE path never touches rmcp. `handle_message` parses JSON-RPC 2.0 itself and
calls `Server::dispatch` (our code) for `tools/call`; the rmcp `ServerHandler`
impl remains the exclusive Streamable HTTP path.

**Alternatives considered and rejected:**
- *Custom rmcp `Transport` impl*: the trait targets persistent duplex pipes
  (stdio/TCP); per-session transports + an ID→Transport map is strictly more
  machinery, and `OneshotTransport` is stateless by design. Couples the legacy
  transport to rmcp internals that rmcp does not support this use case for.
- *Port mcp-go's SSEServer to Rust*: ~400 lines of Go translation with no
  benefit — violates the "no mechanical porting" principle (AGENTS.md); the
  wire format is small enough to implement natively in axum/tokio.

### D2 — Wire format = mcp-go v0.57.0 SSEServer defaults (oracle parity, byte-level) *— revised 2026-09-01 for the general-service deployment model*

- `GET /sse` → `200`, `Content-Type: text/event-stream`. First frame:
  `event: endpoint` with `data:` = **absolute URL**
  `<scheme>://<host>/message?sessionId=<uuid>` (absolute URL confirmed by user
  2026-09-01). **Scheme/host are proxy-aware**: taken from `X-Forwarded-Proto`
  / `X-Forwarded-Host` when present (first value, if comma-list), else
  `http` + the `Host` header (revised 2026-09-01: the service may sit behind a
  TLS-terminating reverse proxy — a hardcoded `http://` would hand clients a
  dead message URL). Session ID: server-generated UUIDv4.
- Subsequent server→client frames: `event: message` with `data:` = one JSON-RPC
  2.0 object (responses to requests; server notifications if any — a tools-only
  mcp-go server sends none).
- `POST /message?sessionId=<id>` → `202 Accepted`, empty body. Missing or
  unknown `sessionId` → `400 Bad Request` (verify exact status/body against
  mcp-go v0.57.0 `sse.go` before implementing — read the Go module cache
  copy, read-only).
- Keep-alive: **disabled** (oracle default; user-confirmed 2026-09-01). Dead or
  abandoned sessions are reaped by the idle timeout (D9) + the disconnect guard,
  not by periodic ping events.
- **CORS: no `Access-Control-Allow-Origin` header** (revised 2026-09-01 —
  deliberate, justified break from oracle parity): mcp-go v0.57.0 emits
  `Access-Control-Allow-Origin: *` by default, which is a security
  anti-pattern for a general-use service. Per the migration principle (do not
  copy the oracle's mistakes), the Rust server sends no wildcard CORS; browsers
  enforce same-origin, and a deploying proxy may add explicit CORS if needed.
- Session lifetime: created on `GET /sse`, removed when the SSE stream closes
  (client disconnect via axum `Request::abort`, or server shutdown) **or when
  the idle reaper fires (D9)** (revised 2026-09-01: the oracle has no idle
  timeout because it is a single-user local server; a general service must reap
  proxy-held idle connections).
- `ReadHeaderTimeout: 10s` (oracle): verify the axum serve config in
  `crates/cli/src/serve`; add `.header_read_timeout(10s)` if absent (task 1.3).

**Verification source:** mcp-go v0.57.0 `server/sse.go` from the Go module
cache (`$(go env GOMODCACHE)/github.com/mark3labs/mcp-go@v0.57.0/`, read-only) or
the v0.57.0 tag on GitHub. The oracle's own tests (`../synopsis/internal/mcp/`)
contain no SSE wire tests, so the library source is the wire reference.

### D3 — In-memory session map, zero new crates *— revised 2026-09-01 for the general-service deployment model*

`Arc<std::sync::Mutex<std::collections::HashMap<String, SseSession>>>` where
`SseSession { tx: tokio::sync::mpsc::Sender<String>, last_activity: Instant }`
(payload = one JSON-RPC object string; the `/sse` stream frames it as
`event: message`).

- **Bounded channel, capacity 64** (revised 2026-09-01; was unbounded under the
  laptop assumption): a slow/dead client must not accumulate unbounded frames
  in RAM — with multiple concurrent clients an unbounded channel is an
  unbounded memory growth vector (the 16 GB constraint makes this a real
  risk). Backpressure: `SseSessionMap::send(id, payload)` awaits
  `tx.send(payload)` under a 1 s timeout; on timeout the session is removed
  (a client that cannot drain 64 small JSON-RPC frames in 1 s is dead) and the
  send fails — task 1.2 maps that failure to an HTTP error per mcp-go v0.57.0.
- **Encapsulation (revised 2026-09-01, reviewer note):** `tx` is private; the
  map exposes `create()`, `get()` (for tests), `send(id, payload)`,
  `touch(id)` (activity update, used by task 1.2's POST handler), `remove(id)`,
  `len()`, `is_empty()`.
- `last_activity` is set on create and refreshed by `touch()`; D9's reaper
  reads it.
- `std::sync::Mutex` (not DashMap): the lock is held only for HashMap ops
  (microseconds), never across I/O or `.await`; fine at service-scale session
  counts. DashMap would be a new dependency — rejected per the frozen stack.
- Dependencies made direct (ALL already in Cargo.lock — no new crates in the
  tree): `sse-stream` 0.2.x (SSE frame serialization, incl. multi-line data
  escaping), `uuid` (v4, session IDs), `tokio-stream` (mpsc Receiver → Stream
  for the axum body). `reqwest` (already a workspace dep, parity-harness) gains
  the `stream` feature only.

### D4 — Minimal JSON-RPC 2.0 method table (tools-only server semantics)

| method | response |
|---|---|
| `initialize` | result: `protocolVersion` (echo the client's if known, else the server's default — pin exact rule from mcp-go v0.57.0), `capabilities` (tools-only shape — pin from mcp-go v0.57.0), `serverInfo` = same name/version as the Streamable HTTP path (`Server.name`/`version`) |
| `notifications/initialized` | none (notification) |
| `ping` | `{}` |
| `tools/list` | `{ tools: [ …12 tools… ] }` from `Server::tools()` |
| `tools/call` | `Server::dispatch(name, args)`; tool-level `McpError` → MCP tool result `isError: true` with the error text in `content` (MCP convention; verify mcp-go behavior); protocol-level (unknown tool, bad params) → JSON-RPC error |
| anything else | `-32601` Method not found (resources/prompts/completions included — the oracle's tools-only mcp-go server does not register them) |

Parse error → `-32700`; structurally invalid params → `-32602`. Batch requests:
mcp-go v0.57.0 does not support them → reject (verify exact error) — clients of
the oracle never send batches.

### D5 — Route composition

`Router::new().route("/health", …).route("/sse", get(handle_sse)).route("/message",
post(handle_message)).fallback_service(StreamableHttpService)`. Shared axum state:
`Arc<Server>` + `Arc<SseSessionMap>` (cheap clones per request). The oracle
mounted its SSE server at `/` (unknown paths → 404); the Rust superset serves
Streamable HTTP on other paths — an accepted consequence of the dual-transport
decision, not a wire change to `/sse` or `/message`.

### D6 — Graceful shutdown

No `CloseSessions` equivalent needed as a separate call: server shutdown closes
the TCP connections, which fires `Request::abort` in each `/sse` handler and
removes the sessions. The idle reaper (D9) is a detached tokio task that simply
stops being relevant once the map is empty and the process exits. The existing
cli stop path (broadcast stop → axum graceful shutdown) is unchanged.
Documented in the module doc.

### D7 — Testing and parity

- Unit (in `transport/sse.rs`): SSE frame bytes (endpoint event — incl.
  proxy-aware scheme/host and the no-wildcard-CORS header set — message event,
  multi-line data escaping), session map ops (create/send/touch/remove, bounded
  backpressure), idle-reaper behavior (reaped when idle, kept alive by
  `touch`), JSON-RPC parse + method table (all error codes).
- Integration (in `server.rs` tests, in-process axum): endpoint event shape
  (absolute URL from Host header), `POST /message` → 202 + response over SSE,
  dead-session 400, disconnect cleanup, `/health` + Streamable HTTP + SSE
  coexistence on one router.
- Parity (parity-harness, task 1.4): minimal `SseClient` (reqwest `stream`
  feature; manual `event:`/`data:` line parsing — format is trivial, no new
  crate) + `tests/sse_parity.rs`: against the same fixture DB, `tools/list` and
  representative `tools/call`s via BOTH transports → identical JSON (ANN-score
  fields excluded per the data/search specs' approximate-fields convention).
  The `initialize` result shape is pinned by a fixture recorded once from the Go
  binary (existing fixture-recording convention); if the Go binary is not
  buildable in the environment, derive the shape from mcp-go v0.57.0 source and
  mark the fixture source-derived in its header comment.
- Manual: MCP Inspector pointed at `http://<host>:<port>/sse`.

### D8 — Main spec Purpose edit (permitted direct edit)

`openspec/specs/mcp-contract/spec.md` Purpose sentence
"Транспорт намеренно модернизирован: Streamable HTTP вместо legacy SSE оракула
(design D8…)" becomes stale. Per the OpenSpec workflow, an existing
capability's Purpose is edited directly in the main spec (task 1.3 does this
one-sentence edit, documenting the D8 override, user decision 2026-08-31). The
delta's MODIFIED "Транспорт" requirement carries the requirement-body change at
sync time.

### D9 — Session idle timeout + reaper *— new 2026-09-01 (general-service model)*

The oracle has no idle timeout (a single local user always closes their
connection). For a general service, a reverse proxy can hold an idle SSE
connection open indefinitely → the session (and its bounded channel) leaks.

- `SseSession.last_activity: Instant`, set on create, refreshed by
  `SseSessionMap::touch(id)` (called by task 1.2's `POST /message` handler on
  every request).
- `SseSessionMap::spawn_reaper()` — a detached tokio task: every tick (30 s)
  remove sessions whose `last_activity` is older than the **idle threshold
  (default 300 s)**. Removal drops the session's `tx`, which ends the SSE
  stream, which drops the body, which fires the disconnect guard — one code
  path, no special-casing.
- The reaper is spawned once from `Server::router()` (wired in task 1.5).
  Threshold and
  tick are constructor parameters (`SseSessionMap::with_idle_timeout(dur)`),
  default 300 s; a config knob is a future change (the oracle has none, so no
  frozen-config surface is invented here).
- Tests use `tokio::time::pause()` + a short threshold for determinism (no
  real waiting).

## Risks

- **Hand-rolled JSON-RPC surface** (~150 lines): mitigated by pinning every
  shape against mcp-go v0.57.0 source + a machine parity test against the
  Go-recorded fixture.
- **Disconnect cleanup correctness**: axum `Request::abort` is the documented
  mechanism; covered by an integration test.
- **SSE framing edge cases** (multi-line data, CRLF): delegated to the
  `sse-stream` crate (already in the tree, used by rmcp's own SSE path).
- **Proxy header trust (revised 2026-09-01):** `X-Forwarded-Proto`/`Host` are
  client-settable; a malicious client could point the endpoint URL at another
  host. Accepted risk for a self-hosted service: the URL is only used by the
  same client that sent the headers (it POSTs JSON-RPC there and reads the
  response from its own SSE stream), and a deploying proxy overwrites these
  headers. Documented in the module doc; no auth is in scope (the oracle has
  none — a separate explicit decision if ever needed).
- **Bounded-channel send timeout (revised 2026-09-01):** a client that stalls
  for >1 s while the buffer is full gets its session reaped mid-request.
  Accepted: 64 small JSON-RPC frames in 1 s is far beyond any real client's
  drain rate on a LAN; the alternative (unbounded) is a memory-growth vector
  under the 16 GB constraint.
