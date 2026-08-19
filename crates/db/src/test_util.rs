//! Test support for the `db` test suite and downstream crates' integration
//! tests: in-memory databases and the read-only v5 parity fixture.
//!
//! These helpers are public (integration tests cannot see `#[cfg(test)]`
//! modules) and always compiled — the cost is negligible for this project.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::connection::Db;

/// Open an in-memory database with the full init migration and D8 PRAGMAs
/// applied (`journal_mode` degrades to `memory`, inherent to `:memory:`;
/// every other PRAGMA reads back exactly).
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub fn in_memory_db() -> Db {
    // In-memory open + embedded migration failure is a test-infra failure:
    // panic with the message rather than force `Result` plumbing everywhere.
    Db::open(":memory:").expect("open in-memory database")
}

/// Resolve the repo root from the crate manifest location (robust against
/// the test process working directory).
///
/// The two-level layout (`crates/db` under the workspace root) is a
/// build-time invariant; a violation panics with a diagnostic instead of
/// plumbing a `Result` through test helpers.
#[allow(clippy::expect_used)]
fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("crates/db must live two levels below the repo root")
}

/// Open the v5 parity fixture `fixtures/knowledge.db` (a Go-oracle-created
/// database; provenance in `fixtures/README.md`) strictly read-only and
/// immutable: `file:...?mode=ro&immutable=1`, never mutated, no `-wal`/`-shm`
/// sidecars created next to it.
///
/// The fixture skips migration and PRAGMA setup by construction: it is
/// already at the v5 schema, and the Go oracle tracked schema state in the
/// `_schema_migrations` table, leaving `PRAGMA user_version` at 0. Tests must
/// not write through the returned handle.
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub fn fixture_db() -> Db {
    let path = fixture_path();
    let uri = format!("file:{}?mode=ro&immutable=1", path.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap_or_else(|source| {
        panic!(
            "open fixture {}: {source} (regenerate per fixtures/README.md)",
            path.display()
        )
    });
    Db::new(conn)
}

/// The fixture path at the repo root (gitignored binary; provenance and the
/// regeneration command in `fixtures/README.md`).
fn fixture_path() -> PathBuf {
    repo_root().join("fixtures").join("knowledge.db")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The read-only fixture opens and answers a query. Skips when the
    /// gitignored binary is missing so CI (fresh checkout) stays green.
    #[test]
    fn fixture_db_opens_read_only() {
        if !fixture_path().exists() {
            eprintln!("skip: fixture {} not present", fixture_path().display());
            return;
        }
        let db = fixture_db();
        // v5 scale per fixtures/README.md.
        let chunks: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 270);
    }
}
