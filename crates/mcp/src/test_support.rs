//! Test support for integration tests (design D3, change
//! test-hygiene-phase-1, task 1.10).
//!
//! Thin delegates to the `transport::sse` test seams that the inline unit
//! tests used to reach through `super::*`. Docs-hidden and test-only: not
//! part of the public API surface. The items they delegate to stay
//! `pub(crate)`; these free functions and the const are the only public
//! entry points (a `pub use` cannot re-export a `pub(crate)` item — E0364).

use axum::http::HeaderMap;
use sse_stream::Sse;

/// The proxy-aware absolute message-endpoint URL `GET /sse` puts in the
/// `endpoint` event (design D2 revision): scheme from `X-Forwarded-Proto`
/// (first value if a comma-list, lowercased) else `http`, host from
/// `X-Forwarded-Host` else the `Host` header.
pub fn endpoint_url(headers: &HeaderMap, id: &str) -> String {
    crate::transport::sse::endpoint_url(headers, id)
}

/// Encode one SSE event frame per the SSE spec (WHATWG §8.2): every line of
/// the event's data gets its own `data:` field, and the frame ends with a
/// blank line.
pub fn encode_sse_frame(sse: &Sse) -> String {
    crate::transport::sse::encode_sse_frame(sse)
}

/// The bounded per-session channel capacity (design D3 revision).
pub const CHANNEL_CAPACITY: usize = crate::transport::sse::CHANNEL_CAPACITY;
