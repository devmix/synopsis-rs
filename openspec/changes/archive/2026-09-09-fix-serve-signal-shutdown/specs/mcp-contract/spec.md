# mcp-contract Specification

## MODIFIED Requirements

### Requirement: Transport

The server provides the MCP protocol over HTTP on two transports simultaneously (both always enabled, no flags or settings): (1) Streamable HTTP (MCP spec ≥ 2025-11-25; official SDK rmcp 3.x, design D8) — a single endpoint for POST JSON-RPC with the response as plain JSON or an SSE stream; (2) legacy HTTP+SSE (mcp-go v0.57.0 SSEServer — wire contract, override of D8 by human decision 2026-08-31): `GET /sse` → `200 text/event-stream`, the first frame is `event: endpoint` with data = absolute URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host from `X-Forwarded-Proto`/`X-Forwarded-Host`, otherwise `http` + the `Host` header; server-generated UUIDv4), then `event: message` frames with JSON-RPC 2.0; `POST /message?sessionId=<id>` → `202 Accepted` (empty body), responses arrive over the SSE stream; missing/unknown sessionId → `400`; keep-alive is disabled (default); a session lives until the SSE stream is closed, 300 s of inactivity (idle reaper — an extension of the wire contract for the service model, human decision 2026-09-01), or the server performs a graceful shutdown (the server terminates all active legacy SSE sessions — the streams end and the clients see a clean EOF — so the HTTP drain completes within the bounded shutdown; an extension of the wire contract, human decision 2026-09-08); the wildcard `Access-Control-Allow-Origin` header is NOT issued (a deviation from the wire contract, security of a shared service, human decision 2026-09-01). The JSON-RPC methods of the legacy transport — tools-only semantics of mcp-go: `initialize` (result: protocolVersion/capabilities/serverInfo — serverInfo matches the name/version of the Streamable HTTP path), `notifications/initialized` (no response), `ping` (`{}`), `tools/list` (the same 12 tools), `tools/call` (the same `Server::dispatch`; tool error → result with `isError: true`, protocol error → JSON-RPC error); unknown method → `-32601`. Additionally: `GET /health` — status, version, and knowledge base counters; the health endpoint response structure is fixed by this contract.

#### Scenario: Connecting a modern MCP client
- **WHEN** an MCP client (Streamable HTTP) performs initialize and tools/list against the Rust server
- **THEN** the handshake completes with an agreed protocol version, and tools/list returns the 12 tools

#### Scenario: Health endpoint
- **WHEN** GET /health after a successful start
- **THEN** 200 with the structure (status/version/sync state/counters) fixed by this contract

#### Scenario: Connecting a legacy SSE client
- **WHEN** an MCP client (legacy SSE) performs GET /sse
- **THEN** 200 text/event-stream; the first frame is `event: endpoint` with data = absolute URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host from `X-Forwarded-Proto`/`X-Forwarded-Host`, otherwise `http` + `Host`)

#### Scenario: Session idle timeout
- **WHEN** a legacy SSE session receives no JSON-RPC request for 300 seconds
- **THEN** the reaper closes the session (the SSE stream ends); subsequent POSTs for this sessionId get 400

#### Scenario: Sessions on graceful shutdown
- **WHEN** the server performs a graceful shutdown (SIGINT/SIGTERM) and a legacy SSE session is active
- **THEN** the server terminates the session (the SSE stream ends, the client sees a clean EOF), the HTTP drain completes within the bounded shutdown, and the process exits with code 0

#### Scenario: Tool call via legacy SSE
- **WHEN** a legacy SSE client sends a `tools/call` to POST /message?sessionId=<id>
- **THEN** the server answers 202 Accepted, and the result arrives as an `event: message` frame (JSON-RPC response) over the SSE stream of the same session; the same call over Streamable HTTP returns identical JSON (except fields marked approximate)

#### Scenario: Invalid session
- **WHEN** POST /message?sessionId=<id> with a missing or unknown sessionId
- **THEN** 400 Bad Request; existing sessions are unaffected

#### Scenario: Connection drop
- **WHEN** a legacy SSE client closes the GET /sse connection
- **THEN** the session is removed from the registry; subsequent POSTs for this sessionId get 400
