//! Test support for integration tests (design D3, change
//! test-hygiene-phase-1, task 1.9).
//!
//! Thin delegates to the `LlmClient` test seams that the inline unit tests
//! used to reach through `super::*`. Docs-hidden and test-only: not part of
//! the public API surface. The methods they delegate to stay `pub(crate)`;
//! these free functions are the only public entry points (a `pub use`
//! cannot re-export inherent methods).

use std::time::Duration;

use crate::client::LlmClient;

/// Replaces the backoff sleeper of `client` (the `with_sleeper` seam).
///
/// The default sleeper is a real [`std::thread::sleep`]; tests install a
/// recorder to observe the backoff delays without sleeping. The sleeper
/// runs on the calling thread, between retry attempts.
pub fn with_sleeper(
    client: LlmClient,
    sleeper: impl Fn(Duration) + Send + Sync + 'static,
) -> LlmClient {
    client.with_sleeper(sleeper)
}

/// The backoff delay `client` will sleep before retry `retry`
/// (1-based: 1 = the first retry).
///
/// `BACKOFF_BASE_MS · BACKOFF_FACTOR^(retry-1)` scaled by a ±20% jitter
/// drawn from the client's splitmix64 stream.
pub fn backoff_delay(client: &LlmClient, retry: u32) -> Duration {
    client.backoff_delay(retry)
}
