//! Test support for integration tests (change test-hygiene-phase-2,
//! task 2.6).
//!
//! Thin seams to the `usearch` engine internals the integration tests
//! exercise that a separate test crate cannot name. Docs-hidden and
//! test-only: not part of the public API surface. The items they
//! delegate to stay `pub(crate)`; these free functions are the only
//! public entry points (a `pub use` cannot re-export a `pub(crate)`
//! item — E0364).

use std::path::Path;

use crate::VectorsError;
use crate::usearch::UsearchEngine;

/// Delegate for the `pub(crate)` [`crate::usearch::read_keys`]: reads
/// the sidecar key manifest at `path` (the `keys_manifest` codec) — the
/// integration tests inspect the flushed segment manifests on disk.
pub fn read_keys(path: &Path) -> Result<Vec<u32>, VectorsError> {
    crate::usearch::read_keys(path)
}

/// Delegate for the `pub(crate)` [`crate::usearch::ram_index_size`]: the
/// raw RAM layer index size (before the stale-set subtraction).
pub fn ram_index_size(engine: &UsearchEngine) -> usize {
    crate::usearch::ram_index_size(engine)
}
