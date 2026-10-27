//! Test support for integration tests (change test-hygiene-phase-2,
//! task 2.4).
//!
//! Thin seams to the `serve::server` items the integration tests exercise
//! that a separate test crate cannot name. Docs-hidden and test-only: not
//! part of the public API surface. The items they delegate to stay
//! `pub(crate)`; these free functions and the const are the only public
//! entry points (a `pub use` cannot re-export a `pub(crate)` item — E0364).

use std::time::Duration;

use crate::error::CliError;
use crate::serve::bootstrap::Bootstrap;

/// The serve graceful-shutdown bound (10 s): the shutdown-timing assertions
/// bound themselves against it plus margin.
pub const SHUTDOWN_TIMEOUT: Duration = crate::serve::server::SHUTDOWN_TIMEOUT;

/// Delegate for the `pub(crate)`
/// [`crate::serve::server::recreate_vectors_engine`]: drops the stored ANN
/// table and recreates the engine with the configured dimension.
pub fn recreate_vectors_engine(boot: &mut Bootstrap) -> Result<(), CliError> {
    crate::serve::server::recreate_vectors_engine(boot)
}
