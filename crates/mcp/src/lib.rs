//! MCP server crate: rmcp 3.x Streamable HTTP transport over axum (design
//! D1/D8), the frozen 12-tool registry (`mcp-contract`) and the `GET
//! /health` endpoint (design D5).
//!
//! Oracle mapping: `../synopsis/internal/mcp` + `internal/mcp/handlers`
//! (design.md D1). The Go code is a reference for behavior and contracts
//! only — this crate is the Rust re-architecture (functional copy, not a
//! code copy): the transport is rmcp's Streamable HTTP (design D8; the
//! oracle's legacy SSE is deliberately not ported), tool schemas are
//! transcribed from `../synopsis/internal/mcp/tools.go` and pinned by the
//! registry test in [`server`].
//!
//! Public API (task 5.1 scaffold; re-exports finalized in task 5.10):
//! - [`Server`] — injected collaborators (Db, Searcher, GraphIndex) +
//!   axum router assembly;
//! - [`McpError`] — handler error mapped to MCP tool errors (design D7);
//! - [`health`] — `/health` status (design D5);
//! - [`pagination`] — opaque cursor pagination shared by the catalog tools
//!   (design D3);
//! - [`tools`] — per-tool handlers (design D2/D4).

pub mod error;
pub mod health;
pub mod pagination;
pub mod server;
pub mod tools;

pub use error::McpError;
pub use server::Server;
