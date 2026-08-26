//! Pre-ingest database backup and source clearing (task 3.6, design D6).
//!
//! Oracle reference: `internal/ingestion/ingester.go`
//! (`createBackup`, `getDBPath`, `clearSourceData`), re-architected per the
//! no-1:1-copy directive:
//!
//! - **Free functions, not methods:** both operations need only the
//!   database handle (the oracle held the pool on the struct); the
//!   [`super::Ingester`] calls them at its hooks.
//! - **Backup failures are warnings, never fatal (design D6):** a failed
//!   snapshot is logged with `eprintln!` and reported as `Ok(false)` so the
//!   ingest continues. Only a database that cannot even be read
//!   (`PRAGMA database_list` failure — a storage-level fault) returns
//!   `Err`, per design D8 ("DB failure fails that source").
//! - **UTC timestamps:** the oracle stamped local time; the backup name is
//!   an identifier, and UTC is unambiguous (no DST, no locale). Recorded
//!   deviation.
//! - **Component-boundary prefix match:** the oracle's
//!   `strings.HasPrefix(clean(doc), clean(root))` matched SIBLING
//!   directories (`/data/docs2/…` under root `/data/docs`) — a bug.
//!   [`under_root`] matches at a path-component boundary instead.
//! - **Single quotes escaped in `VACUUM INTO`:** the oracle interpolated
//!   the path raw; the database file name is config-controlled, so the
//!   literal is escaped (belt-and-braces on top of the internal-path
//!   caveat).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use db::{ConnectionOrTx, Db, DocumentDao};
use utils::temporal::format_backup_stamp;

use crate::error::IngestionError;

/// Create a WAL-consistent `VACUUM INTO` snapshot of the database (design
/// D6) in `<db_dir>/backups/<base>_backup_<ts><ext>` and report whether a
/// snapshot was written.
///
/// The timestamp format is `%Y-%m-%dT%H-%M-%S-<ms>` (design D6; UTC — see
/// the module docs). In-memory and unnamed databases have no file to
/// snapshot and skip with `Ok(false)`. A snapshot failure (unwritable
/// backups directory, disk error, …) is a WARNING: it is logged with
/// `eprintln!` and reported as `Ok(false)` — a backup failure must never
/// abort an ingest (oracle parity).
///
/// # Errors
///
/// `Err` only when the database cannot be read at all (pool checkout or
/// `PRAGMA database_list` failure) — a storage-level fault that would fail
/// the very next ingest write anyway (design D8).
pub(crate) fn create_backup(db: &Db) -> Result<bool, IngestionError> {
    let db_file: String = db
        .with_conn(|conn| conn.query_row("PRAGMA database_list", [], |row| row.get(2)))
        .map_err(IngestionError::from)?
        .map_err(|source| IngestionError::Db(db::DbError::Sqlite { source }))?;
    if db_file.is_empty() || db_file == ":memory:" {
        // In-memory / unnamed database: nothing to snapshot (design D6).
        return Ok(false);
    }

    let db_path = PathBuf::from(&db_file);
    let backups_dir = db_path
        .parent()
        .map(|parent| parent.join("backups"))
        .unwrap_or_else(|| PathBuf::from("backups"));
    let (stem, ext) = file_stem_and_ext(&db_path);
    let backup_path = backups_dir.join(format!(
        "{stem}_backup_{stamp}{ext}",
        stamp = format_backup_stamp(SystemTime::now())
    ));

    let result = fs::create_dir_all(&backups_dir)
        .map_err(|source| IngestionError::Io {
            path: backups_dir.clone(),
            source,
        })
        .and_then(|()| vacuum_into(db, &backup_path));
    match result {
        Ok(()) => {
            eprintln!("backup: {}", backup_path.display());
            Ok(true)
        }
        Err(err) => {
            eprintln!("warning: backup failed: {err}");
            Ok(false)
        }
    }
}

/// Delete every document whose cleaned `original_path` is under the cleaned
/// `source_root`, in ONE transaction (a rebuild that fails mid-way must not
/// leave the database partially cleared), and return the number deleted.
/// Zero matches is a no-op (no transaction at all).
///
/// # Errors
///
/// Storage errors (listing, deletion, commit).
pub(crate) fn clear_source_data(db: &Db, source_root: &Path) -> Result<usize, IngestionError> {
    let root = clean_path(source_root);
    let ids: Vec<i64> = db.with_conn(|conn| -> Result<Vec<i64>, db::DbError> {
        let docs = DocumentDao::new(ConnectionOrTx::Connection(conn)).list()?;
        Ok(docs
            .into_iter()
            .filter(|doc| under_root(&doc.original_path, &root))
            .map(|doc| doc.id)
            .collect())
    })??;
    if ids.is_empty() {
        return Ok(0);
    }
    db.exec_tx(|tx| -> Result<(), IngestionError> {
        let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
        for id in &ids {
            // `delete` reports `false` (not an error) for a missing id
            // (DAO contract); the list was just read, so absence means a
            // concurrent delete — nothing to do.
            docs.delete(*id)?;
        }
        Ok(())
    })?;
    Ok(ids.len())
}

/// Run `VACUUM INTO '<backup_path>'` on a pooled connection.
///
/// SQLite does not support bound parameters in `VACUUM INTO` — the path is
/// interpolated as a string literal. This is safe because the path is
/// generated internally (the database directory, a fixed `backups`
/// subdirectory, the database file name and a timestamp), never user input
/// (same caveat as the oracle); single quotes are additionally doubled
/// (the oracle did not escape).
fn vacuum_into(db: &Db, backup_path: &Path) -> Result<(), IngestionError> {
    let literal = format!("'{}'", backup_path.to_string_lossy().replace('\'', "''"));
    db.with_conn(|conn| conn.execute_batch(&format!("VACUUM INTO {literal}")))
        .map_err(IngestionError::from)?
        .map_err(|source| IngestionError::Db(db::DbError::Sqlite { source }))?;
    Ok(())
}

/// The database file name split into (stem, extension-including-dot),
/// mirroring Go's `filepath.Base` + `filepath.Ext`: the extension is the
/// suffix after the FINAL dot, and a leading dot is not an extension
/// (`.hidden` → (`.hidden`, ``)).
fn file_stem_and_ext(db_path: &Path) -> (String, String) {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("database");
    let (stem, ext) = match file_name.rfind('.') {
        Some(0) => (file_name, ""),
        Some(pos) => file_name.split_at(pos),
        None => (file_name, ""),
    };
    (stem.to_owned(), ext.to_owned())
}

/// `filepath.Clean` equivalent built on [`std::path::Components`]: redundant
/// separators collapse, `.` segments drop, `..` segments pop — clamped at
/// the root (the oracle kept leading `..` for relative inputs; ingest roots
/// and stored document paths are absolute in practice).
fn clean_path(path: &Path) -> PathBuf {
    let mut components: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                }
            }
            other => components.push(other),
        }
    }
    let mut result = PathBuf::new();
    for component in components {
        result.push(component.as_os_str());
    }
    result
}

/// Whether a stored document path lives under the source root: the cleaned
/// path equals the cleaned root or extends it at a PATH-COMPONENT boundary
/// (the oracle's plain string prefix matched sibling directories like
/// `/data/docs2` under root `/data/docs` — fixed here).
fn under_root(original_path: &str, root: &Path) -> bool {
    // `starts_with` matches at component boundaries and includes the
    // equality case (a document stored exactly at the root).
    let doc = clean_path(Path::new(original_path));
    doc.starts_with(root)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use db::test_util::in_memory_db;
    use db::{ChunkDao, DocumentDao};

    use super::*;

    /// A unique temp directory, removed on drop (tests run in parallel).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synopsis-backup-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // ── create_backup ─────────────────────────────────────────────────────

    #[test]
    fn file_database_snapshot_is_created_in_backups_dir() {
        let dir = TempDir::new();
        let db = Db::open(dir.0.join("knowledge.db")).unwrap();
        db.exec_tx(|tx| -> Result<(), IngestionError> {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", "/docs/a.md", None, None)?;
            Ok(())
        })
        .unwrap();

        let created = create_backup(&db).unwrap();
        assert!(created, "a file-backed database must be snapshotted");

        let mut entries = fs::read_dir(dir.0.join("backups"))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1, "exactly one snapshot");
        let name = entries
            .pop()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("knowledge_backup_"), "{name}");
        assert!(name.ends_with(".db"), "{name}");
        // The timestamp segment: 19 chars of %Y-%m-%dT%H-%M-%S, a dash,
        // and 3 millisecond digits.
        let ts = name
            .trim_start_matches("knowledge_backup_")
            .trim_end_matches(".db");
        assert_eq!(ts.len(), 23, "{name}");
        assert_eq!(&ts[10..11], "T", "{name}");
        assert_eq!(&ts[19..20], "-", "{name}");

        // The snapshot is a valid, openable copy of the database.
        let snapshot = Db::open(dir.0.join("backups").join(&name)).unwrap();
        let count: i64 = snapshot
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1, "the seeded document must be in the snapshot");
    }

    #[test]
    fn in_memory_database_skips_the_backup() {
        let db = in_memory_db();
        let created = create_backup(&db).unwrap();
        assert!(!created, "an in-memory database has no file to snapshot");
    }

    #[test]
    fn blocked_backups_dir_degrades_to_a_warning() {
        let dir = TempDir::new();
        let db = Db::open(dir.0.join("knowledge.db")).unwrap();
        // A regular FILE where the backups directory must go: create_dir_all
        // fails, and the snapshot must not fail the run either.
        fs::write(dir.0.join("backups"), "blocker").unwrap();

        let created = create_backup(&db).unwrap();
        assert!(
            !created,
            "a blocked backups directory must degrade to a warning"
        );
    }

    // ── clear_source_data ─────────────────────────────────────────────────

    #[test]
    fn clear_source_data_deletes_only_documents_under_the_root() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), IngestionError> {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", "/data/docs/a.md", None, None)?;
            docs.create("markdown", "/data/docs/sub/b.md", None, None)?;
            docs.create("markdown", "/data/docs2/c.md", None, None)?; // sibling
            docs.create("markdown", "/other/d.md", None, None)?;
            Ok(())
        })
        .unwrap();

        // Trailing slash: the root is normalized before matching.
        let cleared = clear_source_data(&db, Path::new("/data/docs/")).unwrap();
        assert_eq!(cleared, 2, "the root's documents only, not the sibling");

        let paths: HashSet<String> = db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn))
                    .list()
                    .unwrap()
                    .into_iter()
                    .map(|doc| doc.original_path)
                    .collect()
            })
            .unwrap();
        assert_eq!(
            paths,
            ["/data/docs2/c.md", "/other/d.md"]
                .into_iter()
                .map(str::to_owned)
                .collect::<HashSet<_>>(),
            "the sibling and foreign documents must survive"
        );

        // A dot-segment root cleans to the same path; nothing left to clear.
        assert_eq!(
            clear_source_data(&db, Path::new("/data/./docs")).unwrap(),
            0
        );
    }

    #[test]
    fn clear_source_data_without_matches_is_a_noop() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), IngestionError> {
            let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
            docs.create("markdown", "/data/docs/a.md", None, None)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            clear_source_data(&db, Path::new("/elsewhere")).unwrap(),
            0,
            "no match → nothing deleted"
        );
        let empty = in_memory_db();
        assert_eq!(
            clear_source_data(&empty, Path::new("/data/docs")).unwrap(),
            0,
            "empty database → nothing deleted"
        );
    }

    #[test]
    fn clear_source_data_cascades_chunks_and_is_idempotent() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), IngestionError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let docs = DocumentDao::new(exec);
            let chunks = ChunkDao::new(exec);
            let doc_id = docs.create("markdown", "/data/docs/a.md", None, None)?;
            chunks.create(doc_id, "chunk text", 0, None, None)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(clear_source_data(&db, Path::new("/data/docs")).unwrap(), 1);
        assert_eq!(
            clear_source_data(&db, Path::new("/data/docs")).unwrap(),
            0,
            "the second clear is a no-op"
        );

        let (docs, chunks) = db
            .with_conn(|conn| {
                let exec = ConnectionOrTx::Connection(conn);
                (
                    DocumentDao::new(exec).list().unwrap().len(),
                    ChunkDao::new(exec).list_all().unwrap().len(),
                )
            })
            .unwrap();
        assert_eq!(docs, 0);
        assert_eq!(chunks, 0, "chunks cascade with their document");
    }

    // ── pure helpers ──────────────────────────────────────────────────────

    #[test]
    fn file_name_split_mirrors_go_filepath_ext() {
        assert_eq!(
            file_stem_and_ext(Path::new("/d/knowledge.db")),
            ("knowledge".to_owned(), ".db".to_owned())
        );
        assert_eq!(
            file_stem_and_ext(Path::new("/d/a.b.db")),
            ("a.b".to_owned(), ".db".to_owned())
        );
        assert_eq!(
            file_stem_and_ext(Path::new("/d/nodot")),
            ("nodot".to_owned(), String::new())
        );
        assert_eq!(
            file_stem_and_ext(Path::new("/d/.hidden")),
            (".hidden".to_owned(), String::new())
        );
    }

    #[test]
    fn clean_path_collapses_redundant_segments() {
        assert_eq!(clean_path(Path::new("/a/b/")).as_path(), Path::new("/a/b"));
        assert_eq!(
            clean_path(Path::new("/a/b/../c")).as_path(),
            Path::new("/a/c")
        );
        assert_eq!(clean_path(Path::new("/a/./b")).as_path(), Path::new("/a/b"));
        assert_eq!(clean_path(Path::new("/..")).as_path(), Path::new("/"));
        assert_eq!(clean_path(Path::new("a/../b")).as_path(), Path::new("b"));
    }

    #[test]
    fn under_root_matches_at_component_boundaries() {
        let root = Path::new("/data/docs");
        assert!(under_root("/data/docs", root), "the root itself matches");
        assert!(under_root("/data/docs/a.md", root));
        assert!(under_root("/data/docs/sub/a.md", root));
        assert!(
            !under_root("/data/docs2/a.md", root),
            "a sibling directory must NOT match"
        );
        assert!(!under_root("/data/other/a.md", root));
    }
}
