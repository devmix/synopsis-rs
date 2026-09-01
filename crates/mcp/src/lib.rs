//! MCP server crate: rmcp 3.x Streamable HTTP transport over axum (design
//! D1/D8), the frozen 12-tool registry (`mcp-contract`) and the `GET
//! /health` endpoint (design D5).
//!
//! Oracle mapping: `../synopsis/internal/mcp` + `internal/mcp/handlers`
//! (design.md D1). The Go code is a reference for behavior and contracts
//! only — this crate is the Rust re-architecture (functional copy, not a
//! code copy): the transport is rmcp's Streamable HTTP (design D8) plus the
//! oracle's legacy HTTP+SSE wire contract ([`transport`], restored by
//! add-legacy-sse-transport), tool schemas are transcribed from
//! `../synopsis/internal/mcp/tools.go` and pinned by the registry test in
//! [`server`].
//!
//! # Public API
//!
//! Everything the `cli` crate (the next change) needs is re-exported at the
//! crate root; the compile-time test at the bottom of this file pins the
//! root paths so a rename, removal or signature drift fails the build:
//!
//! - [`Server`] — injected collaborators (Db, Searcher, GraphIndex) plus
//!   `router()` for the axum assembly;
//! - [`McpError`] — handler error mapped to MCP tool errors (design D7);
//! - [`HealthStatus`] / [`KbCounters`] — the `GET /health` payload (design D5).
//!
//! The modules [`error`], [`health`], [`pagination`], [`server`], [`tools`]
//! and [`transport`] stay public for intra-crate use and test access; deeper
//! seams (e.g. `health::HealthState`, the per-tool `handle_*` functions) are
//! reachable through them but are not part of the root API the cli
//! consumes.

pub mod error;
pub mod health;
pub mod pagination;
pub mod server;
pub mod tools;
pub mod transport;

pub use error::McpError;
pub use health::{HealthStatus, KbCounters};
pub use server::Server;

#[cfg(test)]
mod root_api {
    //! Compile-time pin of the crate-root public API (task 5.10): the cli
    //! change consumes these root paths, so any rename, removal or
    //! signature drift breaks this test at compile time.

    use std::sync::Arc;

    /// The injected search contract handle (the `Server::new` parameter
    /// type; aliased so the constructor pin below stays readable).
    type SearcherHandle = Arc<dyn search::Searcher + Send + Sync>;

    /// The root re-exports resolve with the exact signatures the cli
    /// assembly will use. (Inside the crate the root is `crate::`; the
    /// external `mcp::` spelling is pinned by the integration test in
    /// `tests/server_integration.rs`.)
    #[test]
    fn root_reexports_resolve() {
        // Server: the constructor and the axum assembly (design D1).
        let _constructor: fn(
            String,
            String,
            db::Db,
            SearcherHandle,
            Arc<graph::GraphIndex>,
        ) -> crate::Server = crate::Server::new;
        let _router: fn(crate::Server) -> axum::Router = crate::Server::router;

        // McpError: the tool-error mapping seam (design D7).
        let _error: crate::McpError = crate::McpError::NotFound {
            what: "root api pin".to_owned(),
        };

        // The /health payload types (design D5).
        let _counters: crate::KbCounters = crate::KbCounters {
            documents: 0,
            chunks: 0,
            entities: 0,
            facts: 0,
        };
        let _status: &crate::HealthStatus = &crate::HealthStatus {
            status: "ok".to_owned(),
            version: "0.0.0".to_owned(),
            sync_state: "idle".to_owned(),
            counters: _counters,
        };
    }
}
