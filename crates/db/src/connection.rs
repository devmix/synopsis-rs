//! Connection pool (design D1, re-decided 2026-08-20), PRAGMA parity (D8)
//! and the embedded squashed v5 init migration (D3).
//!
//! Oracle mapping: `../synopsis/internal/database/database.go` (Open /
//! applyPRAGMAs / Close and the `database/sql` pool semantics), re-anchored
//! on `PRAGMA user_version` per ADR 0001.
//!
//! The handle is an `r2d2` pool of `rusqlite::Connection`s: WAL + several
//! connections give concurrent readers, and a write transaction never
//! blocks readers (the application is read-heavy — human decision
//! 2026-08-20). There is deliberately no way to hold a raw connection
//! between units of work: access goes through [`Db::with_conn`] or
//! [`Db::exec_tx`] only (design D11), which removes the lock-based deadlock
//! class by construction. The driver is synchronous — in async code every
//! call must run inside `spawn_blocking`.

use std::cell::Cell;
use std::path::Path;

use include_dir::{Dir, include_dir};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, Transaction};
use rusqlite_migration::Migrations;

use crate::error::DbError;

/// The knowledge migration tree embedded at compile time from the repo-root
/// `migrations/knowledge` directory (D3): one squashed v5 init migration for
/// now; future migrations are added as numbered `<id>-<slug>/up.sql`
/// directories, forward-only, and shipped files are never edited.
pub(crate) static KNOWLEDGE_MIGRATIONS: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/../../migrations/knowledge");

/// The cache migration tree embedded at compile time from the repo-root
/// `migrations/cache` directory (task 1.9, storage-layout-restructure): the
/// cache database holds ONLY cache tables (`llm_ner_cache`,
/// `llm_linker_cache`, `app_kv`) — never the knowledge schema.
pub(crate) static CACHE_MIGRATIONS: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/../../migrations/cache");

/// Pool size (design D1, re-decided 2026-08-20): a laptop, read-heavy
/// workload; 4 concurrent connections is the agreed default.
pub(crate) const POOL_MAX_SIZE: u32 = 4;

// Set on the thread while its `Db::exec_tx` closure runs (design D10): a
// nested `exec_tx` on the same thread is rejected with
// [`DbError::NestedTransaction`] instead of silently starting an
// INDEPENDENT transaction on another pooled connection. (Plain comment:
// rustdoc does not document `thread_local!` items.)
thread_local! {
    static IN_TX: Cell<bool> = const { Cell::new(false) };
}

/// A SQLite database handle: a `r2d2` pool of `rusqlite::Connection`s
/// (design D1, re-decided 2026-08-20).
///
/// `Db` is cheap to clone (the pool is `Arc`-backed). All access goes
/// through [`Db::with_conn`] (checkout + closure) or [`Db::exec_tx`]
/// (checkout + transaction + return); WAL + several connections give
/// concurrent readers, and a write transaction never blocks readers.
///
/// The driver is synchronous — in async code every call must run inside
/// `spawn_blocking`.
#[derive(Clone)]
pub struct Db {
    pool: Pool<SqliteConnectionManager>,
}

impl Db {
    /// Open the KNOWLEDGE database at `path` (creating the file and its
    /// parent directories if absent), apply the D8 PRAGMAs to every pooled
    /// connection and run the knowledge migrations once.
    ///
    /// Only Rust-created databases are supported: reopening one re-checks
    /// `PRAGMA user_version` and applies nothing further (design D3). Legacy
    /// Go-created `knowledge.db` is never opened, upgraded or migrated
    /// (decision 2026-08-18).
    pub fn open_knowledge<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        Self::open_with(path, &KNOWLEDGE_MIGRATIONS)
    }

    /// Open the CACHE database at `path` (creating the file and its parent
    /// directories if absent), apply the D8 PRAGMAs to every pooled
    /// connection and run the cache migrations once.
    ///
    /// The cache database holds ONLY cache tables (`llm_ner_cache`,
    /// `llm_linker_cache`, `app_kv`) — it must never receive the knowledge
    /// schema (task 1.9, storage-layout-restructure).
    pub fn open_cache<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        Self::open_with(path, &CACHE_MIGRATIONS)
    }

    /// Open the knowledge database at `path` (equivalent to
    /// [`Db::open_knowledge`]).
    ///
    /// Kept for callers that do not need to name the database kind
    /// explicitly; new call sites should use [`Db::open_knowledge`] or
    /// [`Db::open_cache`].
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        Self::open_knowledge(path)
    }

    /// Shared open path: create the file and its parent directories if
    /// absent, run `migrations` on a SINGLE dedicated raw connection, and
    /// only then build the pool with the D8 PRAGMAs applied to every
    /// connection.
    ///
    /// Migrations must NOT run on a pooled connection (task 1.11): while the
    /// pool initializes its connections, the migration's write lock can race
    /// a pooled connection's startup and surface as
    /// `r2d2: database is locked`. The raw connection is dropped before the
    /// pool is built, and the migration state persists in the file
    /// (`PRAGMA user_version` + schema, D3), so the pooled connections open
    /// against an already-migrated database — the application never starts
    /// serving until migrations are applied.
    fn open_with<P: AsRef<Path>>(
        path: P,
        migrations: &'static Dir<'static>,
    ) -> Result<Self, DbError> {
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

        // Migrations run on a dedicated raw connection: while it is open no
        // other connection exists, so the migration's exclusive write lock
        // cannot race anything. `conn` is dropped at the end of this block,
        // releasing the lock BEFORE the pool opens its own connections; the
        // migrated schema + `PRAGMA user_version` persist in the file, so a
        // re-open is a no-op (D3).
        {
            let mut conn = Connection::open(path).map_err(DbError::from)?;
            // D8 PRAGMAs first: `busy_timeout` must be in place for the
            // migration's write transactions.
            apply_pragmas(&conn)?;
            apply_migrations(&mut conn, migrations)?;
        }

        // The D8 PRAGMAs are applied to EVERY connection via the manager
        // init callback (D1 re-decided 2026-08-20): `journal_mode=WAL`
        // persists in the database header, the rest are per-connection.
        let manager = SqliteConnectionManager::file(path).with_init(|conn| apply_pragmas(conn));
        Self::new(manager, POOL_MAX_SIZE)
    }

    /// Build a handle over a ready-made connection manager (test support:
    /// shared-cache in-memory databases, the read-only parity fixture).
    pub(crate) fn new(manager: SqliteConnectionManager, max_size: u32) -> Result<Self, DbError> {
        let pool = Pool::builder()
            .max_size(max_size)
            .build(manager)
            .map_err(DbError::from)?;
        Ok(Self { pool })
    }

    /// Run `migrations` on a pooled connection (D3). Re-running on a
    /// migrated database is a no-op.
    pub(crate) fn run_migrations(&self, migrations: &'static Dir<'static>) -> Result<(), DbError> {
        let mut conn = self.pool.get().map_err(DbError::from)?;
        apply_migrations(&mut conn, migrations)
    }

    /// Run `f` on a connection checked out of the pool (design D11).
    ///
    /// The connection is returned to the pool when `f` returns; it is also
    /// returned if `f` panics (`PooledConnection` drop semantics), so the
    /// pool never leaks a connection. Multiple `with_conn` calls may run
    /// concurrently (WAL); a write transaction in flight on another
    /// connection does not block readers.
    ///
    /// Suitable for reads and single-statement writes; multi-statement units
    /// of work belong to [`Db::exec_tx`].
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> Result<T, DbError> {
        let conn = self.pool.get().map_err(DbError::from)?;
        Ok(f(&conn))
    }

    /// Run `f` inside a single SQLite transaction on a pooled connection
    /// (design D2).
    ///
    /// Transaction lifecycle on the native rusqlite API:
    /// - `f` returns `Ok` → the transaction is **committed** (a commit
    ///   failure is mapped into `E`);
    /// - `f` returns `Err`, or **panics** → the transaction is **rolled back**
    ///   automatically by `Transaction` drop semantics. No manual
    ///   `BEGIN`/`COMMIT` strings, no leaked open transactions.
    ///
    /// The connection is checked out for the whole transaction and returned
    /// to the pool on every path (commit, error and panic — the panic is
    /// re-thrown after the rollback and the `IN_TX` flag reset).
    ///
    /// A nested `exec_tx` on the same thread is rejected with
    /// [`DbError::NestedTransaction`] (D10): with a pool it would silently
    /// start an INDEPENDENT transaction on another connection — a semantic
    /// trap worse than a deadlock.
    ///
    /// `E` must be constructible from [`DbError`] (checkout/commit failures);
    /// closures returning [`DbError`] need no extra work.
    pub fn exec_tx<T, E>(&self, f: impl FnOnce(&mut Transaction) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbError>,
    {
        if IN_TX.with(Cell::get) {
            return Err(E::from(DbError::NestedTransaction));
        }
        let mut conn = self.pool.get().map_err(DbError::from).map_err(E::from)?;
        let mut tx = conn.transaction().map_err(DbError::from).map_err(E::from)?;
        IN_TX.with(|flag| flag.set(true));

        // Run the closure under a panic boundary so a panic cannot unwind
        // straight through `tx`: on panic, drop `tx` (→ rollback) and `conn`
        // (→ returned to the pool) first, then re-throw the original panic.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut tx)));
        IN_TX.with(|flag| flag.set(false));
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
                std::panic::resume_unwind(panic_payload); // re-throw the original panic
            }
        }
    }
}

/// Apply `migrations`; state is tracked in `PRAGMA user_version` (the sole
/// source of truth, design D3). Re-running on a migrated database is a no-op.
fn apply_migrations(
    conn: &mut Connection,
    migrations: &'static Dir<'static>,
) -> Result<(), DbError> {
    let migrations = Migrations::from_directory(migrations).map_err(DbError::from)?;
    migrations.to_latest(conn).map_err(DbError::from)?;
    Ok(())
}

/// D8 PRAGMA parity with the Go oracle (`database.go`: applyPRAGMAs plus the
/// DSN-level `busy_timeout`), in this FIXED order — the oracle iterates a Go
/// map, whose order is non-deterministic (conscious deviation, task 1.1
/// report).
///
/// Applied to EVERY pooled connection via the manager init callback (design
/// D1, re-decided 2026-08-20): `journal_mode=WAL` persists in the database
/// header (a no-op after the first connection), the remaining pragmas are
/// per-connection.
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    const PRAGMAS: &[(&str, &str)] = &[
        ("journal_mode", "WAL"),
        ("synchronous", "NORMAL"),
        ("cache_size", "-64000"), // negative value = KiB (64 MB page cache in WAL mode)
        ("mmap_size", "268435456"), // 256 MB
        ("foreign_keys", "ON"),
        ("busy_timeout", "5000"),
    ];
    for (name, value) in PRAGMAS {
        conn.pragma_update(None, name, value)?;
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
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::test_util::in_memory_db;

    /// Unique temporary database file name under `std::env::temp_dir()`.
    fn temp_db_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "db-task19-{}-{}",
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

    /// Open a fresh file-backed knowledge database, returning it with its
    /// cleanup guard.
    fn open_temp_db() -> (Db, TempDb) {
        let path = temp_db_path();
        let db = Db::open_knowledge(&path).expect("open temp db");
        (db, TempDb::new(path))
    }

    /// Read back the D8 PRAGMA state of one connection (test helper).
    fn read_pragmas(conn: &Connection) {
        let read = |name: &str| -> i64 {
            conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .unwrap()
        };
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(read("synchronous"), 1, "synchronous=NORMAL reads back as 1");
        assert_eq!(read("foreign_keys"), 1, "foreign_keys=ON reads back as 1");
        assert_eq!(read("busy_timeout"), 5000);
        assert_eq!(read("cache_size"), -64_000);
        assert_eq!(read("mmap_size"), 268_435_456);
    }

    // (a, 1.1) open a nonexistent file → the full schema in one squashed init
    //     migration (base tables + document_jobs + usearch_vectors_log +
    //     chunks.search_text), user_version = 1, no _schema_migrations table.
    #[test]
    fn open_creates_fresh_v5_schema() {
        let (db, _temp) = open_temp_db();

        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(
            user_version, 1,
            "PRAGMA user_version must be 1 after the single squashed init migration"
        );

        let tracking_rows: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = '_schema_migrations'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(tracking_rows, 0, "_schema_migrations must not exist (D3)");

        // Set comparison: the v5 schema surface is the contract, not the
        // ordering (byte-lexicographic sort disagrees with SQL ORDER BY on
        // names like `chunk_entities` vs `chunks`).
        let names: HashSet<String> = db
            .with_conn(|conn| {
                conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
            })
            .unwrap();
        // `app_kv` is NOT in the set: task 1.9 moved it to the cache
        // migration (`migrations/cache`) — it is a cache, not knowledge.
        let expected: HashSet<String> = [
            "chunk_entities",
            "chunks",
            "chunks_fts",
            "chunks_fts_config",
            "chunks_fts_data",
            "chunks_fts_docsize",
            "chunks_fts_idx",
            "document_jobs",
            "documents",
            "entity_links",
            "entity_sources",
            "entities",
            "fact_sources",
            "facts",
            "usearch_vectors_log",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            names, expected,
            "fresh knowledge DB must have exactly the knowledge schema \
             (no app_kv / llm_ner_cache)"
        );

        let triggers: Vec<String> = db
            .with_conn(|conn| {
                conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'trigger' \
                     AND name LIKE 'chunks_fts_%' ORDER BY name",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
            })
            .unwrap();
        assert_eq!(
            triggers,
            vec!["chunks_fts_ad", "chunks_fts_ai", "chunks_fts_au"]
        );

        // The squashed init migration: `chunks` has the `search_text` column
        // (NOT NULL) and `chunks_fts` indexes it, not `chunk_text`.
        let chunk_columns: Vec<(String, i64)> = db
            .with_conn(|conn| {
                conn.prepare("PRAGMA table_info(chunks)")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(1)?, r.get(3)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            })
            .unwrap();
        assert!(
            chunk_columns
                .iter()
                .any(|(name, notnull)| { name == "search_text" && *notnull == 1 }),
            "chunks must have a NOT NULL search_text column: {chunk_columns:?}"
        );

        // The FTS5 external-content table stores its CREATE statement in
        // sqlite_master; the indexed column is the first fts5 argument.
        let fts_sql: String = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'chunks_fts'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert!(
            fts_sql.contains("search_text"),
            "chunks_fts must index search_text: {fts_sql}"
        );
        assert!(
            !fts_sql.contains("chunk_text"),
            "chunks_fts must no longer index chunk_text: {fts_sql}"
        );
    }

    // The squashed init migration (usearch-wal-persistence tasks 2.1/2.4,
    // folded in): the WAL table with the composite (segment_id, chunk_id) PK
    // and both indexes exist, and the table is writable with the documented
    // columns.
    #[test]
    fn usearch_vectors_log_table_and_index_exist() {
        let (db, _temp) = open_temp_db();

        let (table, flags_index, segment_index): (i64, i64, i64) = db
            .with_conn(|conn| {
                let table = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'table' AND name = 'usearch_vectors_log'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let flags_index = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'index' AND name = 'idx_usearch_vectors_log_flags'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let segment_index = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'index' AND name = 'idx_usearch_vectors_log_segment'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                (table, flags_index, segment_index)
            })
            .unwrap();
        assert_eq!(table, 1, "usearch_vectors_log table must exist");
        assert_eq!(flags_index, 1, "idx_usearch_vectors_log_flags must exist");
        assert_eq!(
            segment_index, 1,
            "idx_usearch_vectors_log_segment must exist"
        );

        // Composite PK (segment_id, chunk_id), all columns NOT NULL —
        // PRAGMA table_info columns: (name, notnull, pk position).
        let info: Vec<(String, i64, i64)> = db
            .with_conn(|conn| {
                conn.prepare("PRAGMA table_info(usearch_vectors_log)")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(1)?, r.get(3)?, r.get(5)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            })
            .unwrap();
        assert_eq!(
            info,
            vec![
                ("segment_id".to_string(), 1, 1),
                ("chunk_id".to_string(), 1, 2),
                ("flags".to_string(), 1, 0),
                ("created_at".to_string(), 1, 0),
            ],
            "WAL table must have (segment_id, chunk_id) PK, all columns NOT NULL"
        );

        // The table accepts a WAL row (INSERT OR REPLACE semantics are the
        // write path of task 3.4; here only the schema contract is checked).
        db.exec_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO usearch_vectors_log \
                 (segment_id, chunk_id, flags, created_at) \
                 VALUES (0, 1, 1, '2026-08-30T00:00:00Z')",
                [],
            )
            .map_err(DbError::from)
        })
        .expect("insert a WAL row");
        let (segment_id, chunk_id, flags): (i64, i64, i64) = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT segment_id, chunk_id, flags FROM usearch_vectors_log \
                     WHERE segment_id = 0 AND chunk_id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            (segment_id, chunk_id, flags),
            (0, 1, 1),
            "the WAL row must be stored as inserted"
        );
    }

    // (a, 1.9) a freshly created cache database contains EXACTLY the cache
    // tables — never the knowledge schema (task 1.9,
    // storage-layout-restructure: the defect was that open_cache ran the
    // full knowledge migration on the cache Db).
    #[test]
    fn open_cache_creates_cache_only_schema() {
        let path = temp_db_path();
        let _temp = TempDb::new(path.clone());
        let db = Db::open_cache(&path).expect("open cache db");

        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(user_version, 1, "PRAGMA user_version must be 1 after init");

        let names: HashSet<String> = db
            .with_conn(|conn| {
                conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
            })
            .unwrap();
        let expected: HashSet<String> = ["app_kv", "llm_linker_cache", "llm_ner_cache"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            names, expected,
            "fresh cache DB must have exactly the cache tables \
             (no knowledge schema)"
        );
    }

    // (a, 1.9) D8 PRAGMA parity on EVERY pooled connection: POOL_MAX_SIZE
    // simultaneous checkouts must be all distinct connections, and each one
    // reads back the full D8 state.
    #[test]
    fn pragmas_applied_to_every_pool_connection() {
        let (db, _temp) = open_temp_db();

        let mut handles = Vec::new();
        for _ in 0..POOL_MAX_SIZE {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                db.with_conn(read_pragmas).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    }

    // (b, 1.9) parallel reads: 8 threads with_conn at once — all complete
    // correctly, no deadlock.
    #[test]
    fn concurrent_with_conn_reads_do_not_deadlock() {
        let db = in_memory_db();
        db.exec_tx(|tx| {
            tx.execute(
                "INSERT INTO documents (source_type, original_path) \
                 VALUES ('markdown', '/x.md')",
                [],
            )
            .map_err(DbError::from)
        })
        .expect("seed commit");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                let count: i64 = db
                    .with_conn(|conn| {
                        conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                    })
                    .unwrap()
                    .unwrap();
                count
            }));
        }
        let counts: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            counts.iter().all(|count| *count == 1),
            "every reader must see the committed row, got {counts:?}"
        );
    }

    // (c, 1.9) a read is NOT blocked by an in-flight write transaction
    // (WAL): the reader completes while the writer still holds its
    // transaction open.
    #[test]
    fn read_not_blocked_by_write_transaction_in_wal() {
        let (db, _temp) = open_temp_db();
        db.exec_tx(|tx| {
            tx.execute(
                "INSERT INTO documents (source_type, original_path) \
                 VALUES ('markdown', '/seed.md')",
                [],
            )
            .map_err(DbError::from)
        })
        .expect("seed commit");

        let writer_db = db.clone();
        let (writer_started, rx_writer_started) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db.exec_tx(|tx| -> Result<(), DbError> {
                tx.execute(
                    "INSERT INTO documents (source_type, original_path) \
                     VALUES ('markdown', '/w.md')",
                    [],
                )?;
                writer_started.send(()).unwrap();
                // Hold the write transaction open while the reader runs.
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            })
        });

        // Wait until the write transaction is open, then read on another
        // pooled connection: in WAL mode the read must not block on it.
        rx_writer_started.recv().unwrap();
        let during: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(during, 1, "the reader sees pre-transaction data, unblocked");

        writer.join().unwrap().expect("writer commits");
        let after: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(after, 2, "the committed row is visible afterwards");
    }

    // (c, 1.1) reopening the same database does not re-run migrations and
    //     data survives.
    #[test]
    fn reopen_is_noop_migration_and_data_survives() {
        let path = temp_db_path();
        let _temp = TempDb::new(path.clone());

        {
            let db = Db::open_knowledge(&path).expect("first open");
            db.exec_tx(|tx| {
                tx.execute(
                    "INSERT INTO documents (source_type, original_path, content_hash) \
                     VALUES ('markdown', '/x.md', 'abc')",
                    [],
                )
                .map_err(DbError::from)
            })
            .expect("insert");
        } // db dropped → pool closed

        // A migration re-run would fail on the existing tables, so a clean
        // reopen is proof that to_latest() skipped the init migration.
        let db = Db::open_knowledge(&path).expect("reopen");
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(user_version, 1);
        let hash: String = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT content_hash FROM documents WHERE original_path = '/x.md'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(hash, "abc");
    }

    // (d1) exec_tx success → COMMIT: data visible afterwards.
    #[test]
    fn exec_tx_commits_on_success() {
        let db = in_memory_db();
        db.exec_tx(|tx| {
            tx.execute(
                "INSERT INTO documents (source_type, original_path) \
                 VALUES ('markdown', '/a.md')",
                [],
            )
            .map_err(DbError::from)?;
            tx.execute(
                "INSERT INTO documents (source_type, original_path) \
                 VALUES ('markdown', '/b.md')",
                [],
            )
            .map_err(DbError::from)
        })
        .expect("commit");

        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 2, "committed rows must be visible");
    }

    // (d2) exec_tx closure error → ROLLBACK: no data behind.
    #[test]
    fn exec_tx_rolls_back_on_error() {
        let db = in_memory_db();
        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                tx.execute(
                    "INSERT INTO documents (source_type, original_path) \
                     VALUES ('markdown', '/a.md')",
                    [],
                )?;
                // A genuine SQL failure after a partial write (CHECK violation).
                tx.execute(
                    "INSERT INTO facts (predicate, status) VALUES ('p', 'bogus')",
                    [],
                )?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 0, "rolled-back rows must not be visible");
    }

    // (d3) exec_tx panic → ROLLBACK via Transaction drop semantics.
    #[test]
    fn exec_tx_rolls_back_on_panic() {
        let db = in_memory_db();
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _ = db.exec_tx(|tx| -> Result<(), DbError> {
                tx.execute(
                    "INSERT INTO documents (source_type, original_path) \
                     VALUES ('markdown', '/a.md')",
                    [],
                )
                .unwrap();
                panic!("simulated DAO panic");
            });
        }));
        assert!(panicked.is_err(), "the panic must propagate");

        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 0, "panicked transaction must be rolled back");
    }

    // (e, 1.9) a nested exec_tx on the same thread → Err(NestedTransaction),
    // returned immediately (no checkout, no deadlock); the Db stays usable.
    #[test]
    fn nested_exec_tx_is_rejected_without_deadlock() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), DbError> {
            tx.execute(
                "INSERT INTO documents (source_type, original_path) \
                 VALUES ('markdown', '/a.md')",
                [],
            )?;
            let nested = db.exec_tx(|_| -> Result<(), DbError> { Ok(()) });
            assert!(
                matches!(nested, Err(DbError::NestedTransaction)),
                "nested exec_tx must be rejected, got {nested:?}"
            );
            Ok(())
        })
        .expect("outer transaction commits");

        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1, "the outer transaction committed");
    }

    // (g, 1.9) with_conn serves reads and single-statement writes.
    #[test]
    fn with_conn_reads_and_writes() {
        let db = in_memory_db();
        let rows: usize = db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO documents (source_type, original_path, content_hash) \
                     VALUES ('markdown', '/w.md', '1')",
                    [],
                )
            })
            .expect("checkout")
            .expect("single-statement write");
        assert_eq!(rows, 1);

        let hash: String = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT content_hash FROM documents WHERE original_path = '/w.md'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(hash, "1");
    }

    // (z, 1.9) in_memory_db: one shared database across the pool — a write
    // through one checkout is visible through another (the reader holds its
    // connection open, so the writer MUST use a different one).
    #[test]
    fn in_memory_db_shares_one_database_across_checkouts() {
        let db = in_memory_db();
        let (reader_ready, rx_reader_ready) = mpsc::channel();
        let (writer_done, rx_writer_done) = mpsc::channel();

        let reader_db = db.clone();
        let reader = std::thread::spawn(move || {
            reader_db
                .with_conn(|conn| {
                    reader_ready.send(()).unwrap(); // this checkout is held
                    rx_writer_done.recv().unwrap(); // wait for the writer's commit
                    let count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                        .unwrap();
                    count
                })
                .unwrap()
        });
        let writer_db = db.clone();
        let writer = std::thread::spawn(move || {
            writer_db
                .with_conn(|conn| {
                    conn.execute(
                        "INSERT INTO documents (source_type, original_path) \
                         VALUES ('markdown', '/a.md')",
                        [],
                    )
                    .unwrap();
                    writer_done.send(()).unwrap();
                })
                .unwrap()
        });

        rx_reader_ready.recv().unwrap();
        let count = reader.join().unwrap();
        writer.join().unwrap();
        assert_eq!(
            count, 1,
            "a write via one checkout must be visible via another"
        );
    }

    // (e, 1.1) in_memory_db yields a working migrated database.
    #[test]
    fn in_memory_db_is_migrated_and_writable() {
        let db = in_memory_db();
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
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
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
    }
}
