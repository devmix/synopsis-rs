//! Connection management (design D1), PRAGMA parity (D8) and the embedded
//! squashed v5 init migration (D3).
//!
//! Oracle mapping: `../synopsis/internal/database/database.go` (Open /
//! applyPRAGMAs / Close) and the migration mechanism, re-anchored on
//! `PRAGMA user_version` per ADR 0001.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use include_dir::{Dir, include_dir};
use rusqlite::{Connection, Transaction};
use rusqlite_migration::Migrations;

use crate::error::DbError;

/// The migration tree embedded at compile time from the repo-root
/// `migrations/` directory (D3): one squashed v5 init migration for now;
/// future migrations are added as numbered `<id>-<slug>/up.sql` directories,
/// forward-only, and shipped files are never edited.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../migrations");

/// A SQLite database handle: ONE shared [`Connection`] behind `Arc<Mutex>`
/// (design D1).
///
/// `Db` is cheap to clone; all access serializes on the internal mutex, which
/// is sufficient for a single-writer laptop process. The driver is
/// synchronous — in async code every call must run inside `spawn_blocking`.
/// Dropping the last clone closes the database file (rusqlite `Connection`
/// drop).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open the database at `path` (creating the file and its parent
    /// directories if absent), apply the D8 PRAGMAs and run the embedded
    /// migrations.
    ///
    /// Only Rust-created databases are supported: reopening one re-checks
    /// `PRAGMA user_version` and applies nothing further (design D3). Legacy
    /// Go-created `knowledge.db` is never opened, upgraded or migrated
    /// (decision 2026-08-18).
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        let path = path.as_ref();
        // SQLite does not create parent directories; the oracle does it in Open().
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut conn = Connection::open(path).map_err(DbError::from)?;
        // Migrations first, PRAGMAs last: rusqlite_migration manages
        // `PRAGMA foreign_keys` itself while migrating and leaves the final
        // setting to us, and `journal_mode=WAL` must not be set from inside a
        // transaction — so the (already committed) migration state is final
        // before we fix the connection's D8 PRAGMA state.
        migrate(&mut conn)?;
        apply_pragmas(&conn)?;
        Ok(Self::new(conn))
    }

    /// Wrap an already-opened, fully configured connection.
    ///
    /// Crate-internal: `test_util` uses it for the read-only fixture database,
    /// which skips both migration and PRAGMA setup by construction.
    pub(crate) fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Lock the shared connection. The guard derefs to [`Connection`] and
    /// implements [`DbExecutor`](crate::executor::DbExecutor); hold it only
    /// for the duration of one unit of work.
    ///
    /// This call BLOCKS until the mutex is available — run inside
    /// `spawn_blocking` from async code (design D1).
    pub fn lock(&self) -> Result<MutexGuard<'_, Connection>, DbError> {
        self.conn.lock().map_err(|_| DbError::Poisoned)
    }

    /// The `Arc`-backed handle, for callers that manage the lock themselves.
    pub fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// Run `f` inside a single SQLite transaction (design D2).
    ///
    /// Transaction lifecycle on the native rusqlite API:
    /// - `f` returns `Ok` → the transaction is **committed** (a commit
    ///   failure is mapped into `E`);
    /// - `f` returns `Err`, or **panics** → the transaction is **rolled back**
    ///   automatically by `Transaction` drop semantics. No manual
    ///   `BEGIN`/`COMMIT` strings, no leaked open transactions.
    ///
    /// A panic is captured so it rolls the transaction back and releases the
    /// shared mutex cleanly *before* being re-thrown — the `Db` stays usable
    /// afterwards instead of the mutex being poisoned (a deviation that makes
    /// this handle safer than a bare `Mutex` for a long-lived server).
    ///
    /// `E` must be constructible from [`DbError`] (commit failures); closures
    /// returning [`DbError`] need no extra work.
    pub fn exec_tx<T, E>(&self, f: impl FnOnce(&mut Transaction) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbError>,
    {
        // The guard (and thus the connection) stays locked for the whole
        // transaction; `tx` borrows it exclusively until commit/drop.
        let mut guard = self.lock()?;
        let mut tx = guard
            .transaction()
            .map_err(DbError::from)
            .map_err(E::from)?;

        // Run the closure under a panic boundary so a panic cannot unwind
        // straight through `guard` (which would poison the mutex). On panic we
        // drop `tx` (→ rollback) and `guard` (→ unlock) first, then re-throw.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut tx)));
        match outcome {
            Ok(Ok(value)) => tx
                .commit()
                .map_err(DbError::from)
                .map_err(E::from)
                .map(|()| value),
            // `tx` is dropped here with DropBehavior::Rollback: automatic rollback.
            Ok(Err(err)) => Err(err),
            Err(panic_payload) => {
                drop(tx); // roll the transaction back
                drop(guard); // release the mutex while still unwinding-free
                std::panic::resume_unwind(panic_payload); // re-throw the original panic
            }
        }
    }
}

/// Apply the embedded migrations; state is tracked in `PRAGMA user_version`
/// (the sole source of truth, design D3). Re-running on a migrated database
/// is a no-op.
fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    let migrations = Migrations::from_directory(&MIGRATIONS).map_err(DbError::from)?;
    migrations.to_latest(conn).map_err(DbError::from)?;
    Ok(())
}

/// D8 PRAGMA parity with the Go oracle (`database.go`: applyPRAGMAs plus the
/// DSN-level `busy_timeout`), in this FIXED order — the oracle iterates a Go
/// map, whose order is non-deterministic (conscious deviation, task report).
///
/// Note: `journal_mode=WAL` persists in the database header; the remaining
/// pragmas are per-connection, which is exact here because the connection is
/// owned and single (the Go pool applied them to one pooled connection only —
/// see task report).
fn apply_pragmas(conn: &Connection) -> Result<(), DbError> {
    const PRAGMAS: &[(&str, &str)] = &[
        ("journal_mode", "WAL"),
        ("synchronous", "NORMAL"),
        ("cache_size", "-64000"), // negative value = KiB (64 MB page cache in WAL mode)
        ("mmap_size", "268435456"), // 256 MB
        ("foreign_keys", "ON"),
        ("busy_timeout", "5000"),
    ];
    for (name, value) in PRAGMAS {
        conn.pragma_update(None, name, value)
            .map_err(DbError::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::test_util::in_memory_db;

    /// Unique temporary database file name under `std::env::temp_dir()`.
    fn temp_db_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "db-task11-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Deletes the database file and its `-wal`/`-shm` sidecars on drop.
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let base = &self.path;
            let _ = std::fs::remove_file(base);
            let _ = std::fs::remove_file(format!("{}-wal", base.display()));
            let _ = std::fs::remove_file(format!("{}-shm", base.display()));
        }
    }

    /// Open a fresh file-backed database, returning it with its cleanup guard.
    fn open_temp_db() -> (Db, TempDb) {
        let path = temp_db_path();
        let db = Db::open(&path).expect("open temp db");
        (db, TempDb::new(path))
    }

    // (a) open a nonexistent file → fresh v5 schema, user_version = 1,
    //     no _schema_migrations table.
    #[test]
    fn open_creates_fresh_v5_schema() {
        let (db, _temp) = open_temp_db();

        let user_version: i64 = db
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, 1, "PRAGMA user_version must be 1 after init");

        let tracking_rows: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = '_schema_migrations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tracking_rows, 0, "_schema_migrations must not exist (D3)");

        // Set comparison: the v5 schema surface is the contract, not the
        // ordering (byte-lexicographic sort disagrees with SQL ORDER BY on
        // names like `chunk_entities` vs `chunks`).
        let names: HashSet<String> = db
            .lock()
            .unwrap()
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let expected: HashSet<String> = [
            "app_kv",
            "chunk_entities",
            "chunks",
            "chunks_fts",
            "chunks_fts_config",
            "chunks_fts_data",
            "chunks_fts_docsize",
            "chunks_fts_idx",
            "documents",
            "entity_links",
            "entity_sources",
            "entities",
            "fact_sources",
            "facts",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(names, expected, "fresh DB must have exactly the v5 schema");

        let triggers: Vec<String> = db
            .lock()
            .unwrap()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'chunks_fts_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            triggers,
            vec!["chunks_fts_ad", "chunks_fts_ai", "chunks_fts_au"]
        );
    }

    // (b) D8 PRAGMA parity read back from a freshly opened database.
    // All reads happen under ONE lock: the mutex is not reentrant.
    #[test]
    fn pragmas_match_oracle_parity() {
        let (db, _temp) = open_temp_db();

        let guard = db.lock().unwrap();
        let read = |name: &str| -> i64 {
            guard
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .unwrap()
        };
        let journal_mode: String = guard
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(read("synchronous"), 1, "synchronous=NORMAL reads back as 1");
        assert_eq!(read("foreign_keys"), 1, "foreign_keys=ON reads back as 1");
        assert_eq!(read("busy_timeout"), 5000);
        assert_eq!(read("cache_size"), -64_000);
        assert_eq!(read("mmap_size"), 268_435_456);
    }

    // (c) reopening the same database does not re-run migrations and data
    //     survives.
    #[test]
    fn reopen_is_noop_migration_and_data_survives() {
        let path = temp_db_path();
        let _temp = TempDb::new(path.clone());

        {
            let db = Db::open(&path).expect("first open");
            db.exec_tx(|tx| {
                tx.execute("INSERT INTO app_kv (key, value) VALUES ('k', 'v')", [])
                    .map_err(DbError::from)
            })
            .expect("insert");
        } // db dropped → file closed

        // A migration re-run would fail on the existing tables, so a clean
        // reopen is proof that to_latest() skipped the init migration.
        let db = Db::open(&path).expect("reopen");
        let user_version: i64 = db
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, 1);
        let value: String = db
            .lock()
            .unwrap()
            .query_row("SELECT value FROM app_kv WHERE key = 'k'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "v");
    }

    // (d1) exec_tx success → COMMIT: data visible afterwards.
    #[test]
    fn exec_tx_commits_on_success() {
        let db = in_memory_db();
        db.exec_tx(|tx| {
            tx.execute("INSERT INTO app_kv (key, value) VALUES ('a', '1')", [])
                .map_err(DbError::from)?;
            tx.execute("INSERT INTO app_kv (key, value) VALUES ('b', '2')", [])
                .map_err(DbError::from)
        })
        .expect("commit");

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM app_kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "committed rows must be visible");
    }

    // (d2) exec_tx closure error → ROLLBACK: no data behind.
    #[test]
    fn exec_tx_rolls_back_on_error() {
        let db = in_memory_db();
        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                tx.execute("INSERT INTO app_kv (key, value) VALUES ('a', '1')", [])
                    .map_err(DbError::from)?;
                Err(DbError::Poisoned)
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Poisoned));

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM app_kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rolled-back rows must not be visible");
    }

    // (d3) exec_tx panic → ROLLBACK via Transaction drop semantics.
    #[test]
    fn exec_tx_rolls_back_on_panic() {
        let db = in_memory_db();
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _ = db.exec_tx(|tx| -> Result<(), DbError> {
                tx.execute("INSERT INTO app_kv (key, value) VALUES ('a', '1')", [])
                    .unwrap();
                panic!("simulated DAO panic");
            });
        }));
        assert!(panicked.is_err(), "the panic must propagate");

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM app_kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "panicked transaction must be rolled back");
    }

    // (e) in_memory_db yields a working migrated database.
    #[test]
    fn in_memory_db_is_migrated_and_writable() {
        let db = in_memory_db();
        let user_version: i64 = db
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, 1);

        db.exec_tx(|tx| {
            tx.execute(
                "INSERT INTO documents (source_type, original_path) VALUES ('markdown', '/x.md')",
                [],
            )
            .map_err(DbError::from)
        })
        .expect("insert");
        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
