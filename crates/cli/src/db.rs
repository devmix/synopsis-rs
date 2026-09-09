//! `db` subcommand body: dataset statistics and full dataset clear
//! (remove-direct-ingest task 1.1).
//!
//! New Rust operational command: dataset statistics and clear are first-class
//! operations, not a side effect of another subcommand.
//!
//! - `db stats` opens the dataset-bound knowledge database and prints row
//!   counts gathered through the existing DAOs (documents, chunks, entities,
//!   entity_links, facts and `queue_tasks` queue rows). Read-only: no
//!   prompt, no deletion.
//! - `db clear` prints the same statistics, then prompts
//!   `Confirm deletion? [y/N]` on stdin; only `y`/`Y` proceeds to
//!   [`clear_dataset`], which deletes the dataset's ENTIRE state directory
//!   (`<workspace_dir>/datasets/<name>/state`: the knowledge DB plus the
//!   vector index) from disk in one shot.
//!
//! Like the `queue` command the command re-loads the config from the
//! resolved path; it needs only `paths` + `dataset` (the knowledge DB is
//! dataset-bound), so no embedding model or ONNX runtime is touched.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use config::{Config, ConfigError, load};
use db::{
    ChunkDao, ConnectionOrTx, Db, DbError, DocumentDao, DocumentFilter, EntityDao, EntityFilter,
    EntityLinkDao, FactDao, QueueTaskDao,
};

use crate::cli::DbAction;
use crate::console::Console;
use crate::error::CliError;
use crate::serve::bootstrap::open_db;

/// One `db` invocation: the resolved config path plus the clap `db`
/// sub-action.
pub struct DbRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// `--dataset` dataset name override (wins over `config.dataset.name`).
    pub dataset: Option<String>,
    /// The db sub-action.
    pub action: DbAction,
}

/// The `db` subcommand entry point.
///
/// Loads the config, opens the dataset-bound knowledge database, and
/// dispatches the sub-action. Human-readable output goes to stdout; the
/// `clear` confirmation answer is read from stdin. On error the message goes
/// to stderr and the exit code is non-zero (every fatal error exits 1).
pub fn run_db(req: &DbRequest) -> ExitCode {
    let mut input = std::io::stdin().lock();
    match db_flow(req, &mut std::io::stdout(), &mut input) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

/// Config load + knowledge-db open + sub-action dispatch (the production
/// path of [`run_db`]).
///
/// `out` receives the human-readable output (production: stdout) and
/// `input` supplies the `clear` confirmation line (production: stdin); the
/// tests drive this seam with in-memory buffers.
///
/// # Errors
///
/// [`CliError::Config`] for config load/validation failures,
/// [`CliError::Db`] when the knowledge database cannot be opened or the
/// stats / clear queries fail, [`CliError::Io`] when `out` or `input`
/// cannot be written to / read from.
pub fn db_flow(
    req: &DbRequest,
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Result<(), CliError> {
    let config = load_config(&req.cfg_path, req.dataset.as_deref())?;
    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir))?;
    let stats = collect_stats(&db)?;
    // One console per flow: TTY/NO_COLOR/width gating lives in the
    // constructor (design D2/D3), the handlers only render strings.
    let console = Console::stdout();
    print_stats(out, &stats, &console)?;

    match &req.action {
        DbAction::Stats => Ok(()),
        DbAction::Clear => {
            if !confirm_deletion(out, input)? {
                writeln!(out, "{}", console.line("aborted: dataset unchanged"))?;
                return Ok(());
            }
            // Close the connection BEFORE removing the directory it lives in.
            drop(db);
            let state_path = config.dataset.state_path(&config.paths.workspace_dir);
            clear_dataset(&state_path)?;
            writeln!(
                out,
                "{}",
                console.line(&format!(
                    "cleared dataset state at {}",
                    state_path.display()
                ))
            )?;
            Ok(())
        }
    }
}

/// The dataset statistics block (the `db stats` lines).
struct Stats {
    /// `documents` row count.
    documents: i64,
    /// `chunks` row count.
    chunks: i64,
    /// `entities` row count.
    entities: i64,
    /// `entity_links` row count.
    entity_links: i64,
    /// `facts` row count.
    facts: i64,
    /// `queue_tasks` (queue) row count.
    queue_tasks: i64,
}

/// Loads the config for the `db` commands: load + defaults + the
/// `--dataset` override, and requires a non-empty dataset name (the
/// knowledge database is dataset-bound).
///
/// # Errors
///
/// [`CliError::Config`] for load failures or an empty dataset name.
fn load_config(cfg_path: &Path, dataset_override: Option<&str>) -> Result<Config, CliError> {
    let mut config = load(cfg_path)?;
    config.apply_defaults();
    if let Some(name) = dataset_override {
        config.dataset.name = name.to_string();
    }
    if config.dataset.name.is_empty() {
        return Err(CliError::Config(ConfigError::Validation {
            message: "no dataset configured (dataset.name is empty); the knowledge database \
                      is dataset-bound"
                .to_string(),
        }));
    }
    Ok(config)
}

/// The row counts of the dataset knowledge database (all-matching filters:
/// a `None`/empty filter member is not applied).
fn collect_stats(db: &Db) -> Result<Stats, CliError> {
    let base = db
        .with_conn(|conn| -> Result<(i64, i64, i64, i64, i64), DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            Ok((
                DocumentDao::new(exec).count(&DocumentFilter::default())?,
                ChunkDao::new(exec).count()?,
                EntityDao::new(exec).count(&EntityFilter::default())?,
                EntityLinkDao::new(exec).count()?,
                FactDao::new(exec).count()?,
            ))
        })
        .map_err(CliError::Db)?
        .map_err(CliError::Db)?;
    let queue_count = db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .map_err(CliError::Db)?
        .map_err(|e| CliError::Unsupported(format!("queue task: {e}")))?
        .len() as i64;
    Ok(Stats {
        documents: base.0,
        chunks: base.1,
        entities: base.2,
        entity_links: base.3,
        facts: base.4,
        queue_tasks: queue_count,
    })
}

/// Renders the statistics block to `out`: a section header plus a
/// borderless kv block (label column auto-width, design D5).
///
/// # Errors
///
/// [`CliError::Io`] when `out` cannot be written.
fn print_stats(out: &mut dyn Write, stats: &Stats, console: &Console) -> Result<(), CliError> {
    let documents = stats.documents.to_string();
    let chunks = stats.chunks.to_string();
    let entities = stats.entities.to_string();
    let entity_links = stats.entity_links.to_string();
    let facts = stats.facts.to_string();
    let queue_tasks = stats.queue_tasks.to_string();
    writeln!(out, "{}", console.header("Dataset Statistics:"))?;
    writeln!(
        out,
        "{}",
        console.kv(&[
            ("Documents", &documents),
            ("Chunks", &chunks),
            ("Entities", &entities),
            ("Entity links", &entity_links),
            ("Facts", &facts),
            ("Queue tasks", &queue_tasks),
        ])
    )?;
    Ok(())
}

/// The `clear` confirmation: prints `Confirm deletion? [y/N]` to `out` and
/// reads one answer line from `input`. Proceeds only on `y` or `Y`; every
/// other answer (including EOF) aborts.
///
/// # Errors
///
/// [`CliError::Io`] when the prompt cannot be written or the answer line
/// cannot be read.
fn confirm_deletion(out: &mut dyn Write, input: &mut dyn BufRead) -> Result<bool, CliError> {
    write!(out, "Confirm deletion? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y"))
}

/// Deletes the dataset's ENTIRE state directory
/// (`<workspace_dir>/datasets/<name>/state`) from disk (the `db clear`
/// action, remove-direct-ingest task 1.1).
///
/// The state directory holds the knowledge database
/// (`state/db/knowledge.db`) and the vector index (`state/vectors/`), so
/// both go away in one shot — no per-table deletes, no cascade ordering.
/// The `db clear` command closes the database connection BEFORE calling
/// this. After a clear, a `serve` restart re-enqueues the source files at
/// startup reconcile and the worker re-embeds them, recreating the
/// directory.
///
/// A missing directory is not an error: clearing an already-empty dataset
/// succeeds.
///
/// # Errors
///
/// [`CliError::Io`] when the directory cannot be removed.
pub(crate) fn clear_dataset(state_path: &Path) -> Result<(), CliError> {
    if state_path.exists() {
        std::fs::remove_dir_all(state_path)?;
    }
    Ok(())
}

/// Clears the knowledge database tables IN PLACE (the `serve` forced-rebuild
/// recovery, remove-direct-ingest task 1.2). The `serve` process holds the
/// `Db` handle open and borrowed by the runner / job queue / worker, so the
/// state directory must NOT be deleted here (that would orphan the pooled
/// connections and later writes would land on a deleted inode) — the rows
/// go, the file stays.
///
/// One transaction deleting in dependency order: `entity_links`, `facts`,
/// `chunks`, `entities`, `documents`, `queue_tasks`. The join tables
/// (`chunk_entities`, `fact_sources`, `entity_sources`) are cleared by the
/// schema's `ON DELETE CASCADE` (`foreign_keys=ON` on every pooled
/// connection), and the `chunks_fts` FTS5 external-content index follows the
/// `chunks` deletes through its triggers.
///
/// # Errors
///
/// [`CliError::Db`] when a delete fails or the transaction cannot commit.
pub(crate) fn clear_dataset_tables(db: &Db) -> Result<(), CliError> {
    db.exec_tx(|tx| -> Result<(), DbError> {
        tx.execute_batch(
            "DELETE FROM entity_links;
             DELETE FROM facts;
             DELETE FROM chunks;
             DELETE FROM entities;
             DELETE FROM documents;
             DELETE FROM queue_tasks;",
        )?;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use db::EntityLink;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("synopsis-cli-db-{tag}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    /// A fixture: a config (paths + dataset `edtech`) and the migrated
    /// knowledge database at the derived dataset path, seeded with one
    /// document, two chunks, three entities, one link, one fact, one queue
    /// job and one row in every join table.
    struct DbFixture {
        dir: TempDir,
        cfg_path: PathBuf,
        db: Db,
    }

    impl DbFixture {
        /// The dataset state directory
        /// (`<workspace>/datasets/edtech/state`).
        fn state_dir(&self) -> PathBuf {
            self.dir
                .as_ref()
                .join("workspace")
                .join("datasets")
                .join("edtech")
                .join("state")
        }

        fn new(tag: &str) -> Self {
            let dir = TempDir::new(tag);
            let workspace = dir.as_ref().join("workspace");
            let cfg_path = dir.as_ref().join("config.yaml");
            std::fs::write(
                &cfg_path,
                format!(
                    "paths:\n  workspace_dir: {workspace}\ndataset:\n  name: edtech\n",
                    workspace = workspace.display()
                ),
            )
            .expect("write config");
            let db = open_db(
                &workspace
                    .join("datasets")
                    .join("edtech")
                    .join("state")
                    .join("db")
                    .join("knowledge.db"),
            )
            .expect("open db");
            seed(&db);
            Self { dir, cfg_path, db }
        }
    }

    /// Seeds the fixture rows (counts: 1 document, 2 chunks, 3 entities,
    /// 1 link, 1 fact, 1 job, 1 row per join table).
    fn seed(db: &Db) {
        db.with_conn(|conn| -> Result<(), DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let doc = DocumentDao::new(exec).create("markdown", "/docs/a.md", None, None)?;
            let chunk = ChunkDao::new(exec).create(doc, "first chunk text", 0, None, None)?;
            ChunkDao::new(exec).create(doc, "second chunk text", 1, None, None)?;
            let e1 = EntityDao::new(exec).create("system", "CRM", "hr", None, None, None)?;
            let e2 = EntityDao::new(exec).create("system", "ERP", "hr", None, None, None)?;
            EntityDao::new(exec).create("person", "Alice", "hr", None, None, None)?;
            EntityLinkDao::new(exec).create(&EntityLink {
                subject_entity_id: e1,
                target_entity_id: e2,
                relation_type: "same_entity".to_string(),
                method: "rule".to_string(),
                confidence: 0.9,
                evidence: None,
            })?;
            let fact = FactDao::new(exec).create(
                Some(e1),
                "works_at",
                Some(e2),
                "hr",
                None,
                None,
                None,
            )?;
            conn.execute(
                "INSERT INTO fact_sources (fact_id, document_id, quote) \
                 VALUES (?1, ?2, 'exact quote')",
                rusqlite::params![fact, doc],
            )?;
            conn.execute(
                "INSERT INTO entity_sources (entity_id, document_id) VALUES (?1, ?2)",
                rusqlite::params![e1, doc],
            )?;
            conn.execute(
                "INSERT INTO chunk_entities (chunk_id, entity_id) VALUES (?1, ?2)",
                rusqlite::params![chunk, e1],
            )?;
            QueueTaskDao::new(exec)
                .enqueue(
                    db::QueueTaskType::DocIndex,
                    "/docs/a.md",
                    &db::DocIndexPayload {
                        source_path: "/docs".to_owned(),
                        content_hash: None,
                        ops: vec![db::ReIndexOp::Full],
                    },
                    100,
                )
                .expect("enqueue task");
            Ok(())
        })
        .expect("with_conn")
        .expect("seed rows");
    }

    /// Runs the `db` flow with the given action and stdin text; returns the
    /// rendered output and the flow result.
    fn run_flow(f: &DbFixture, action: DbAction, stdin: &str) -> (String, Result<(), CliError>) {
        let req = DbRequest {
            cfg_path: f.cfg_path.clone(),
            dataset: None,
            action,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut input = stdin.as_bytes();
        let result = db_flow(&req, &mut out, &mut input);
        (String::from_utf8_lossy(&out).into_owned(), result)
    }

    /// The `TABLE` row count (direct SQL: join tables included).
    fn table_count(db: &Db, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        db.with_conn(|conn| conn.query_row(&sql, [], |r| r.get(0)))
            .expect("with_conn")
            .expect("count")
    }

    /// Asserts the seeded count set (the pre-clear baseline).
    fn assert_seeded(db: &Db) {
        assert_eq!(table_count(db, "documents"), 1);
        assert_eq!(table_count(db, "chunks"), 2);
        assert_eq!(table_count(db, "entities"), 3);
        assert_eq!(table_count(db, "entity_links"), 1);
        assert_eq!(table_count(db, "facts"), 1);
        assert_eq!(table_count(db, "queue_tasks"), 1);
        assert_eq!(table_count(db, "chunk_entities"), 1);
        assert_eq!(table_count(db, "fact_sources"), 1);
        assert_eq!(table_count(db, "entity_sources"), 1);
    }

    // --- db stats --------------------------------------------------------------

    #[test]
    fn stats_prints_counts_and_changes_nothing() {
        let f = DbFixture::new("stats");
        let (stdout, result) = run_flow(&f, DbAction::Stats, "");
        result.expect("stats must succeed");
        assert!(stdout.starts_with("Dataset Statistics:\n"), "{stdout:?}");
        // Non-TTY rendering: no ANSI escapes, every line within 120 columns.
        assert!(!stdout.contains('\x1b'), "{stdout:?}");
        for line in stdout.lines() {
            assert!(line.chars().count() <= 120, "line fits 120: {line:?}");
        }
        assert!(stdout.contains("Documents:    1"), "{stdout:?}");
        assert!(stdout.contains("Chunks:       2"), "{stdout:?}");
        assert!(stdout.contains("Entities:     3"), "{stdout:?}");
        assert!(stdout.contains("Entity links: 1"), "{stdout:?}");
        assert!(stdout.contains("Facts:        1"), "{stdout:?}");
        assert!(stdout.contains("Queue tasks:  1"), "{stdout:?}");
        assert!(
            !stdout.contains("Confirm deletion"),
            "stats must not prompt: {stdout:?}"
        );
        assert_seeded(&f.db);
    }

    // --- db clear ----------------------------------------------------------------

    #[test]
    fn clear_with_y_deletes_state_dir() {
        let f = DbFixture::new("clear-y");
        let state_dir = f.state_dir();
        let (stdout, result) = run_flow(&f, DbAction::Clear, "y\n");
        result.expect("clear must succeed");
        // The stats block is printed before the prompt (spec scenario).
        assert!(stdout.contains("Documents:    1"), "{stdout:?}");
        assert!(stdout.contains("Confirm deletion? [y/N]"), "{stdout:?}");
        assert!(stdout.contains("cleared dataset state at"), "{stdout:?}");
        assert!(!state_dir.exists(), "the state directory must be gone");
    }

    #[test]
    fn clear_with_uppercase_y_deletes_state_dir() {
        let f = DbFixture::new("clear-Y");
        let state_dir = f.state_dir();
        let (_, result) = run_flow(&f, DbAction::Clear, "Y\n");
        result.expect("clear must succeed");
        assert!(!state_dir.exists(), "the state directory must be gone");
    }

    #[test]
    fn clear_with_n_aborts_unchanged() {
        let f = DbFixture::new("clear-n");
        let (stdout, result) = run_flow(&f, DbAction::Clear, "n\n");
        result.expect("an aborted clear exits 0");
        assert!(stdout.contains("Confirm deletion? [y/N]"), "{stdout:?}");
        assert!(stdout.contains("aborted"), "{stdout:?}");
        assert!(
            !stdout.contains("cleared dataset state"),
            "no clear must be reported: {stdout:?}"
        );
        assert!(f.state_dir().exists(), "the state directory must remain");
        assert_seeded(&f.db);
    }

    #[test]
    fn clear_without_answer_aborts_unchanged() {
        let f = DbFixture::new("clear-eof");
        // No answer at all (EOF on stdin) aborts.
        let (stdout, result) = run_flow(&f, DbAction::Clear, "");
        result.expect("an aborted clear exits 0");
        assert!(stdout.contains("aborted"), "{stdout:?}");
        assert_seeded(&f.db);
    }

    #[test]
    fn clear_with_yes_aborts_unchanged() {
        let f = DbFixture::new("clear-yes");
        // `yes` is NOT `y`/`Y`: the spec proceeds only on `y`/`Y`.
        let (stdout, result) = run_flow(&f, DbAction::Clear, "yes\n");
        result.expect("an aborted clear exits 0");
        assert!(stdout.contains("aborted"), "{stdout:?}");
        assert_seeded(&f.db);
    }

    #[test]
    fn clear_dataset_removes_the_engine_subdirectory() {
        // Task 1.5: the state directory holds the vector engine subdirectory
        // (vectors/usearch); the whole-directory clear removes it in one
        // shot.
        let dir = TempDir::new("clear-engines");
        let state = dir.as_ref().join("workspace/datasets/edtech/state");
        let usearch = state.join("vectors/usearch");
        std::fs::create_dir_all(&usearch).expect("usearch fixture");
        // ADR 0004 §1 layout: the RAM snapshot + sidecar and the DISK segment
        // directory (task 3.10: the old single-file index fixture is gone).
        std::fs::write(usearch.join("ram.usearch"), b"ram").expect("ram file");
        std::fs::write(usearch.join("ram.keys"), b"keys").expect("ram sidecar");
        let segments = usearch.join("segments");
        std::fs::create_dir_all(&segments).expect("segments fixture");
        std::fs::write(segments.join("segment-1.usearch"), b"segment").expect("segment file");
        std::fs::write(segments.join("segment-1.keys"), b"keys").expect("segment sidecar");
        std::fs::create_dir_all(state.join("db")).expect("db fixture");
        std::fs::write(state.join("db/knowledge.db"), b"db").expect("db file");

        clear_dataset(&state).expect("clear must succeed");

        assert!(!state.exists(), "the state directory must be gone");
        assert!(
            !usearch.exists(),
            "the usearch engine subdirectory must be gone"
        );
    }

    // --- clear_dataset_tables (serve forced-rebuild, task 1.2) -------------------

    #[test]
    fn clear_dataset_tables_empties_every_table_in_place() {
        let f = DbFixture::new("clear-tables");
        assert_seeded(&f.db);

        clear_dataset_tables(&f.db).expect("clear succeeds");

        // Every knowledge table is empty (join tables via ON DELETE CASCADE).
        for table in [
            "documents",
            "chunks",
            "entities",
            "entity_links",
            "facts",
            "queue_tasks",
            "chunk_entities",
            "fact_sources",
            "entity_sources",
        ] {
            assert_eq!(table_count(&f.db, table), 0, "{table} must be empty");
        }

        // The database file is untouched and still usable in place (the
        // serve process keeps its pooled connections open).
        assert!(f.state_dir().exists(), "the state directory must remain");
        let doc =
            f.db.with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                    "markdown",
                    "/docs/after.md",
                    None,
                    None,
                )
            })
            .expect("with_conn")
            .expect("post-clear insert must work");
        assert!(doc > 0);
    }

    // --- config / exit codes ------------------------------------------------------

    #[test]
    fn missing_dataset_is_an_error() {
        let dir = TempDir::new("no-dataset");
        let cfg_path = dir.as_ref().join("config.yaml");
        std::fs::write(&cfg_path, "paths:\n  workspace_dir: /tmp/nowhere\n").expect("write config");
        let req = DbRequest {
            cfg_path,
            dataset: None,
            action: DbAction::Stats,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut input = &b""[..];
        let err = db_flow(&req, &mut out, &mut input).expect_err("no dataset must fail");
        assert!(
            matches!(err, CliError::Config(ConfigError::Validation { .. })),
            "got: {err:?}"
        );
    }

    #[test]
    fn run_db_maps_exit_codes() {
        let f = DbFixture::new("exit-codes");
        let ok = run_db(&DbRequest {
            cfg_path: f.cfg_path.clone(),
            dataset: None,
            action: DbAction::Stats,
        });
        assert_eq!(ok, ExitCode::SUCCESS, "stats exits 0");

        let fail = run_db(&DbRequest {
            cfg_path: f.dir.as_ref().join("missing.yaml"),
            dataset: None,
            action: DbAction::Stats,
        });
        assert_eq!(fail, ExitCode::FAILURE, "missing config exits 1");
    }
}
