//! Shared synchronous HTTP client for OpenAI-compatible LLM APIs.
//!
//! New base-tier crate per design.md D1 (change `llm`): the only internal
//! dependency is `config` (source of [`LlmConfig`](config::preset::LlmConfig));
//! `graph` and `embedding` depend on this crate, never the other way around.
//!
//! The public seam is [`LlmClient`] (built from a validated
//! [`LlmConfig`](config::preset::LlmConfig)) and [`LlmError`]. The client is a
//! blocking ureq wrapper (design D2): async consumers must dispatch calls onto
//! `spawn_blocking` workers (workspace convention). [`LlmClient::call`]
//! performs a single chat completion and returns the model's text content.
//!
//! Design:
//! silent defaults are replaced by fail-fast validation — a zero timeout,
//! retry count, or token budget is a configuration bug, not a value to paper
//! over (see [`LlmClient::new`]).

mod client;
pub mod error;

/// Test-only seams for integration tests (design D3, change
/// test-hygiene-phase-1 task 1.9); docs-hidden, not part of the public API.
#[doc(hidden)]
pub mod test_support;

pub use client::LlmClient;
pub use error::LlmError;
