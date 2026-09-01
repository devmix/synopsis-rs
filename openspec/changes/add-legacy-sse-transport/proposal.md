# Proposal: add-legacy-sse-transport

## Problem

Design D8 (2026-08-18) deliberately dropped the Go oracle's legacy MCP SSE
transport in favor of Streamable HTTP (rmcp 3.x). The user has reversed that
decision (2026-08-31, Option A): the Rust server must ALSO serve the oracle's
legacy SSE transport — `GET /sse` (SSE stream, server first sends an
`endpoint` event with the session message URL) + `POST /message?sessionId=<id>`
(client→server JSON-RPC, responses flow back over the SSE stream) — alongside
the existing Streamable HTTP transport, so legacy MCP clients (and the oracle's
own client ecosystem) can connect to the Rust binary unchanged. rmcp 3.x has no
server-side SSE transport, so the SSE layer is hand-rolled in `crates/mcp`
around the existing `Server::dispatch` seam (problem-analyst recommendation,
Option A).

## Solution

**Deployment model (user decision 2026-09-01, supersedes the "personal laptop"
assumption):** the server is a self-hosted service for general use — potentially
behind a reverse proxy / TLS termination, multiple concurrent clients. The
16 GB memory constraint is unchanged. Consequently the session layer is
service-hardened: proxy-aware endpoint URL, no wildcard CORS, bounded
per-session channels, and an idle-session reaper.

1. **New transport module** `crates/mcp/src/transport/` (sse.rs): in-memory
   session map (server-generated UUID sessions, bounded channels, idle reaper),
   `GET /sse` handler (proxy-aware absolute-URL endpoint event + JSON-RPC event
   stream, disconnect cleanup + idle timeout), `POST /message` handler
   (202 Accepted; JSON-RPC 2.0 parsed and dispatched through the existing
   `Server::dispatch`; responses pushed into the session's SSE channel).
2. **Minimal JSON-RPC 2.0 layer** for the methods a tools-only mcp-go server
   answers: `initialize`, `notifications/initialized`, `ping`, `tools/list`,
   `tools/call`; unknown method → -32601; parse error → -32700; invalid params
   → -32602. Tool-level failures follow the MCP convention (result with
   `isError: true`), protocol-level failures are JSON-RPC errors. Exact
   `initialize` result shape pinned from mcp-go v0.57.0 (the oracle's pinned
   version).
3. **Router composition** in `Server::router()`: explicit routes `GET /sse` +
   `POST /message` mounted before the Streamable HTTP fallback service;
   `GET /health` unchanged. Both transports always on — no new CLI flags or
   config (oracle parity: the oracle's SSE server was the only transport and
   had no transport switch).
4. **Parity tooling**: a minimal SSE client in `parity-harness` (reqwest
   streaming, manual SSE line parsing — no new crate) and a cross-transport
   parity test (same tool call via SSE and via Streamable HTTP against the
   same fixture DB → identical JSON responses).
5. **Spec**: `mcp-contract` delta — the "Транспорт" requirement becomes
   dual-transport (Streamable HTTP + legacy SSE with the oracle's wire
   contract); the main spec's Purpose sentence about D8 is directly updated
   (permitted Purpose edit).

## Frozen contracts touched

- **MCP contract (user-approved override of D8, 2026-08-31):** the "Транспорт"
  requirement changes from "Streamable HTTP only, legacy SSE intentionally not
  supported" to "Streamable HTTP + legacy SSE (oracle wire contract)". The 12
  tool names/schemas/semantics are untouched; `/health` is untouched.
  Deliberate, justified deviations from the oracle's wire behavior for the
  general-service deployment model (user decision 2026-09-01): no wildcard
  `Access-Control-Allow-Origin`, proxy-aware endpoint-URL scheme
  (`X-Forwarded-Proto`/`Host`), and a 300 s session idle timeout.
- **CLI surface:** untouched (no new flags; both transports always on).
- **Config format:** untouched (no transport setting in the oracle).
- **Data schema:** untouched.

## Non-goals

- Changing or removing the Streamable HTTP transport (it stays; dual transport).
- Keep-alive/ping events on the SSE stream (oracle default: disabled — parity).
- Session persistence, session restoration, or client-provided session IDs
  (oracle: server-generated, in-memory, per-connection).
- Transport selection, load balancing, or per-transport metrics.
- Any change to tool handler behavior (shared `Server::dispatch`).
- Supporting MCP method groups the oracle's tools-only server does not answer
  (resources/prompts/completions → -32601, same as mcp-go v0.57.0).
- Adding authentication or multi-tenancy (the oracle has none; a separate
  explicit decision if ever needed — out of scope for this transport change).
- Oracle `ReadHeaderTimeout: 10s` parity — axum 0.8's serve API has no header
  timeout; the faithful hyper-util serve-loop rewrite is **declined for this
  change** (user decision c, 2026-09-01) and recorded as a deferred future
  improvement in design.md ("Deferred improvements"), to be done when the
  service is actually exposed to an untrusted network.
