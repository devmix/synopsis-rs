//! MCP transports served on one axum router.
//!
//! Dual transport (add-legacy-sse-transport): the server speaks MCP over
//! Streamable HTTP (rmcp 3.x, design D8) *and* the legacy HTTP+SSE wire
//! contract (mcp-go v0.57.0 `SSEServer`). The SSE leg was
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
//! legacy wire (design D2 revision) follow from that:
//!
//! - **No wildcard CORS.** mcp-go emits `Access-Control-Allow-Origin: *` by
//!   default — a security anti-pattern for a general service. The Rust server
//!   sends none; a deploying proxy may add explicit CORS.
//! - **Proxy-aware endpoint URL.** The legacy wire hardcodes `http://` (empty
//!   `baseURL`), a dead message URL behind TLS termination. The `endpoint`
//!   frame takes scheme from `X-Forwarded-Proto` (first value if a comma-list,
//!   lowercased) else `http`, and host from `X-Forwarded-Host` else `Host`.
//!
//! The per-session channel is **bounded** (design D3 revision) with a
//! `send()` backpressure helper: a slow/dead client must not accumulate
//! unbounded frames in RAM under the 16 GB constraint. `last_activity` +
//! `touch()` (design D9) feed the idle reaper, which reaps proxy-held idle
//! connections a single local user never needed to reap.
//!
//! The legacy wire mounted the SSE server at the root path; the router
//! instead exposes explicit `GET /sse` + `POST /message` routes and keeps the
//! Streamable HTTP service as the fallback for every other path (design D5).
//!
//! # Idle timeout (design D9, task 1.5)
//!
//! The legacy wire has no idle timeout — a single local user always closes
//! their connection. A general service behind a reverse proxy can hold an
//! idle SSE connection open indefinitely, leaking the session and its bounded
//! channel. `SseSessionMap::spawn_reaper()` (spawned once from
//! `Server::router()`) runs a detached process-lifetime task that every 30 s
//! (tick) removes sessions idle beyond 300 s (threshold) — the design D9
//! defaults, stored in the map as constructor parameters
//! (`with_idle_timeout` / `with_tick`); the legacy wire has no such surface,
//! so no config knob is invented here. Removal
//! drops the session's outbound sender, which ends the SSE stream and fires
//! the disconnect guard — one removal code path (design D6).
//!
//! # Shutdown (design D6)
//!
//! Removal has one code path: dropping the session's outbound sender ends
//! the SSE stream, the body drops, and the disconnect guard removes the
//! session from the registry. The client disconnect takes it for free
//! (axum's body-drop); the idle reaper (design D9) and the graceful-shutdown
//! close (fix-serve-signal-shutdown D5, `Server::close_all_sessions`) take
//! it explicitly — the cli stop path ends the sessions itself the moment
//! the stop resolves, so the axum drain completes promptly instead of
//! waiting out the forced bound on the never-ending streams.

pub mod jsonrpc;
pub mod sse;

pub use jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use sse::{MessageQuery, SseSession, SseSessionMap, SseState, handle_message, handle_sse};

#[cfg(test)]
mod test_util {
    //! Shared test fixtures for the transport tests: a server over an empty
    //! in-memory KB (the same scaffold pattern as `server.rs` tests).

    use std::sync::Arc;

    use graph::GraphIndex;
    use search::{SearchError, SearchResult};

    use crate::server::Server;

    /// A Searcher stub: the transport tests only need an injectable handle.
    pub struct StubSearcher;

    impl search::Searcher for StubSearcher {
        fn hybrid_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Lexical("stub".to_owned()))
        }

        fn lexical_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Lexical("stub".to_owned()))
        }

        fn semantic_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Semantic("stub".to_owned()))
        }
    }

    /// A server over an empty in-memory KB with no graph.
    pub fn test_server() -> Server {
        Server::new(
            "synopsis-sse-test".to_owned(),
            "0.2.0".to_owned(),
            db::test_util::in_memory_db(),
            Arc::new(StubSearcher),
            Arc::new(GraphIndex::Unavailable),
        )
    }
}
