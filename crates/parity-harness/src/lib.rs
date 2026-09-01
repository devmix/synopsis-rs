//! Parity harness for the Synopsis Rust rewrite (design D6/D8).
//!
//! Dev-tooling crate that gives every module change an automatic parity gate:
//! it compares the Rust implementation against fixtures recorded once from the
//! Go oracle (`../synopsis`) and measures MCP tool-call latency with p50/p95
//! gates. Wire compatibility with the oracle's legacy SSE transport is NOT
//! preserved (design D8); parity lives at the level of tool responses, reached
//! through the official rmcp SDK over Streamable HTTP.
//!
//! The crate provides the mechanism; the actual parity cases arrive together
//! with each module change:
//! - [`mcp_client`]: MCP client wrapper (`initialize`/`tools/list`/`tools/call`)
//!   with per-operation p50/p95 timing;
//! - [`fixtures`]: fixture loader API (knowledge.db + SYNX vectors.bin rows,
//!   streamed via `vectors::synx`; `load_fixture_set_from_dir` reads the
//!   committed `vectors.bin` dump into memory for the recall@k gate);
//! - [`metrics`]: recall@k against provided ground truth;
//! - [`diff`]: JSON/text diff utilities for parity reports;
//! - [`sse_client`]: legacy HTTP+SSE client (mcp-go v0.57.0 wire contract) for
//!   cross-transport parity — drives `GET /sse` + `POST /message` directly.

pub mod diff;
pub mod fixtures;
pub mod mcp_client;
pub mod metrics;
pub mod sse_client;

use std::path::PathBuf;

/// Unified error type for harness operations.
///
/// All failures are returned as values (never panicked): transport/protocol
/// problems, typed tool-call failures, and fixture load errors.
#[derive(Debug)]
pub enum HarnessError {
    /// MCP transport or protocol failure: connection refused, timeout, JSON-RPC
    /// error that is not a tool-level failure, etc.
    McpService {
        /// The SDK's rendered message (`ServiceError`/`ClientInitializeError` Display).
        detail: String,
    },
    /// A `tools/call` request addressed a tool the server does not implement
    /// (JSON-RPC `METHOD_NOT_FOUND`).
    UnknownTool {
        /// Name of the tool that was called.
        tool: String,
        /// Server-provided error message.
        message: String,
    },
    /// The tool ran but reported an error result (`is_error = true`).
    ToolFailed {
        /// Name of the failed tool.
        tool: String,
        /// Concatenated text content of the error result.
        detail: String,
    },
    /// A fixture file was missing or unreadable.
    Fixture {
        /// Path to the problematic fixture file.
        path: PathBuf,
        /// Why loading failed (e.g. "file not found").
        reason: String,
    },
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::McpService { detail } => write!(f, "MCP service error: {detail}"),
            Self::UnknownTool { tool, message } => {
                write!(f, "unknown tool `{tool}`: {message}")
            }
            Self::ToolFailed { tool, detail } => {
                write!(f, "tool `{tool}` returned an error result: {detail}")
            }
            Self::Fixture { path, reason } => {
                write!(f, "fixture load failed for {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for HarnessError {}
