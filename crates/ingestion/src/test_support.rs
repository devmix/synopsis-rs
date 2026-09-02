//! Test support for integration tests (design D3, change
//! test-hygiene-phase-1, task 1.11).
//!
//! Thin delegate to the crate-internal shared walk that the parsers and the
//! ingester's file counter use. Docs-hidden and test-only: not part of the
//! public API surface. The item it delegates to stays `pub(crate)`; this free
//! function is the only public entry point (a `pub use` cannot re-export a
//! `pub(crate)` item so it is visible to integration tests — E0364).

use std::path::Path;

use crate::error::IngestionError;

/// Walks `source_path` and calls `visit` for every matching file, collecting
/// best-effort errors into `errors`. Same four-argument contract as the
/// crate-internal `parsers::walk_matched_files`; a missing root, an
/// unreadable directory and a `visit` failure are all appended to `errors`
/// and the walk continues.
pub fn walk_matched_files(
    source_path: &Path,
    matches: impl Fn(&Path) -> bool,
    visit: impl FnMut(&Path) -> Result<(), IngestionError>,
    errors: &mut Vec<IngestionError>,
) {
    crate::parsers::walk_matched_files(source_path, matches, visit, errors);
}
