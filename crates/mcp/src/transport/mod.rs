//! MCP transports served on one axum router.
//!
//! Dual transport (add-legacy-sse-transport): the server speaks MCP over
//! Streamable HTTP (rmcp 3.x, design D8) *and* the Go oracle's legacy
//! HTTP+SSE wire contract (mcp-go v0.57.0 `SSEServer`). The SSE leg was
//! deliberately dropped by design D8 (2026-08-18) and is restored here by an
//! explicit user decision (2026-08-31) so legacy MCP clients connect to the
//! Rust binary unchanged. The SSE layer is hand-rolled around the shared
//! `Server::dispatch` seam (design D1) — it never touches rmcp, whose SSE
//! primitives are `pub(crate)` and unusable as a server transport.
//!
//! # Deployment model (general-service, user decision 2026-09-01)
//!
//! The server is a self-hosted service for general use — potentially behind a
//! TLS-terminating reverse proxy, with multiple concurrent clients (the 16 GB
//! memory constraint is unchanged). Two deliberate, justified breaks from the
//! oracle's wire (design D2 revision) follow from that:
//!
//! - **No wildcard CORS.** mcp-go emits `Access-Control-Allow-Origin: *` by
//!   default — a security anti-pattern for a general service. The Rust server
//!   sends none; a deploying proxy may add explicit CORS.
//! - **Proxy-aware endpoint URL.** The oracle hardcodes `http://` (empty
//!   `baseURL`), a dead message URL behind TLS termination. The `endpoint`
//!   frame takes scheme from `X-Forwarded-Proto` (first value if a comma-list,
//!   lowercased) else `http`, and host from `X-Forwarded-Host` else `Host`.
//!
//! The per-session channel is **bounded** (design D3 revision) with a
//! `send()` backpressure helper: a slow/dead client must not accumulate
//! unbounded frames in RAM under the 16 GB constraint. `last_activity` +
//! `touch()` (design D9) feed task 1.5's idle reaper, which reaps proxy-held
//! idle connections the oracle (single local user) never needed to reap.
//!
//! The oracle's `mux.Handle("/", sseSrv)` mounted the SSE server at the root
//! path; the Rust router instead exposes explicit `GET /sse` + `POST
//! /message` routes and keeps the Streamable HTTP service as the fallback for
//! every other path (design D5).
//!
//! # Shutdown (design D6)
//!
//! There is no `CloseSessions`-style call to port: server shutdown closes the
//! TCP connections, which fires axum's body-drop (the client disconnect) in
//! each `/sse` handler and removes the session from the registry. The existing
//! cli stop path (broadcast stop → axum graceful shutdown) is unchanged.

pub mod sse;

pub use sse::{SseSession, SseSessionMap, handle_sse};
