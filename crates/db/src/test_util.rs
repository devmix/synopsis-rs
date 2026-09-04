//! Test support for the `db` test suite and downstream crates' integration
//! tests: in-memory databases and temporary file-backed databases.
//!
//! These helpers are public (integration tests cannot see `#[cfg(test)]`
//! modules) and always compiled — the cost is negligible for this project.

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use r2d2_sqlite::SqliteConnectionManager;

use crate::connection::{Db, KNOWLEDGE_MIGRATIONS, POOL_MAX_SIZE, apply_pragmas};

/// Open an in-memory KNOWLEDGE database with the knowledge init migration
/// and D8 PRAGMAs applied (`journal_mode` degrades to `memory`, inherent to
/// in-memory databases; every other PRAGMA reads back exactly).
///
/// This is a knowledge database (task 1.9, storage-layout-restructure): the
/// cache tables (`app_kv`, `llm_ner_cache`, `llm_linker_cache`) are NOT
/// created — DAOs that need them (e.g. [`crate::AppKv`]) create them lazily
/// at runtime, mirroring `LlmNerCache`.
///
/// Every pooled connection shares ONE database (design D12): the manager
/// opens each connection against the same shared-cache URI
/// (`file:{id}?mode=memory&cache=shared`; `SQLITE_OPEN_URI` is part of
/// rusqlite's default open flags) and pins a persistent connection that
/// keeps the shared cache alive.
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub fn in_memory_db() -> Db {
    // In-memory open + embedded migration failure is a test-infra failure:
    // panic with the message rather than force `Result` plumbing everywhere.
    let manager = SqliteConnectionManager::memory().with_init(|conn| apply_pragmas(conn));
    let db = Db::new(manager, POOL_MAX_SIZE).expect("in-memory pool build");
    db.run_migrations(&KNOWLEDGE_MIGRATIONS)
        .expect("migrate in-memory database");
    db
}

/// A temporary file-backed database that deletes itself when dropped
/// (task 1.16).
///
/// Unlike [`in_memory_db`], the database lives in a real file under
/// `std::env::temp_dir()` with WAL — the production configuration. That
/// matters for concurrent WRITE tests: on the shared-cache `:memory:`
/// database SQLite's table-level locks are NOT retryable by the busy
/// handler (`SQLITE_LOCKED_SHAREDCACHE`, extended code 262), so parallel
/// writers flake; on a file-backed WAL database `busy_timeout=5000` (D8)
/// serializes them exactly as in production.
///
/// The wrapper derefs to [`Db`]. It is deliberately NOT `Clone`: a
/// `clone()` call resolves through the deref to [`Db::clone`], so a clone
/// handed to a worker thread holds a plain pool handle and does NOT own
/// the cleanup. The file and its `-wal`/`-shm` sidecars are removed when
/// the wrapper drops — after the `db` field (the pool) has been dropped,
/// so the last connection closes and checkpoints cleanly first.
pub struct TempDb {
    db: Db,
    path: PathBuf,
}

impl Deref for TempDb {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.db
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // The `db` field (the pool) is already dropped by the time this
        // body runs (field drop order), so no connection holds the file.
        let base = &self.path;
        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(format!("{}-wal", base.display()));
        let _ = std::fs::remove_file(format!("{}-shm", base.display()));
    }
}

/// Open a fresh temporary file-backed database — migrated, D8 PRAGMAs and
/// WAL applied — that deletes itself when dropped.
///
/// Use this for tests that write from SEVERAL threads or pool connections
/// (task 1.16); keep [`in_memory_db`] for non-concurrent tests.
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub fn temp_file_db() -> TempDb {
    // A test-infra failure (cannot create/open the temp file): panic with
    // the message rather than force `Result` plumbing through every test.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "synopsis-db-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let db = Db::open_knowledge(&path).expect("open temp file database");
    TempDb { db, path }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use super::*;
    use crate::error::DbError;

    /// `temp_file_db` yields a migrated, WAL, file-backed database that a
    /// clone can write through, and deletes its files on drop.
    #[test]
    fn temp_file_db_is_wal_migrated_and_cleans_up() {
        let db = temp_file_db();
        let path = db.path.clone();
        let db_clone = db.clone();

        // WAL + migrated.
        let journal_mode: String = db
            .with_conn(|conn| conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(journal_mode, "wal", "temp db must be file-backed WAL");
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(
            user_version, 1,
            "temp db must be migrated (the single squashed init migration)"
        );

        // A clone (a plain Db, no cleanup) writes through the same file.
        db_clone
            .exec_tx(|tx| {
                tx.execute(
                    "INSERT INTO documents (source_type, original_path) \
                     VALUES ('markdown', '/x.md')",
                    [],
                )
                .map_err(DbError::from)
            })
            .expect("insert via clone");
        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1, "clone write must be visible in the same file");

        drop(db_clone);
        drop(db);
        assert!(
            !path.exists(),
            "database file must be removed on drop, found {}",
            path.display()
        );
        assert!(
            !Path::new(&format!("{}-wal", path.display())).exists(),
            "wal sidecar must be removed on drop"
        );
        assert!(
            !Path::new(&format!("{}-shm", path.display())).exists(),
            "shm sidecar must be removed on drop"
        );
    }
}
