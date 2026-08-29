//! `db` subcommand body: dataset statistics and full dataset clear
//! (remove-direct-ingest task 1.1).
//!
//! New Rust operational command: the Go oracle has no equivalent (it clears
//! data only inside `sync --rebuild`, which is removed by this change), so
//! there is no oracle mapping and no parity requirement.
//!
//! - `db stats` opens the dataset-bound knowledge database and prints row
//!   counts gathered through the existing DAOs (documents, chunks, entities,
//!   entity_links, facts and `document_jobs` queue rows). Read-only: no
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
    ChunkDao, ConnectionOrTx, Db, DbError, DocumentDao, DocumentFilter, DocumentJobDao, EntityDao,
    EntityFilter, EntityLinkDao, FactDao,
};

use crate::cli::DbAction;
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
    print_stats(out, &stats)?;

    match &req.action {
        DbAction::Stats => Ok(()),
        DbAction::Clear => {
            if !confirm_deletion(out, input)? {
                writeln!(out, "aborted: dataset unchanged")?;
                return Ok(());
            }
            // Close the connection BEFORE removing the directory it lives in.
            drop(db);
            let state_path = config.dataset.state_path(&config.paths.workspace_dir);
            clear_dataset(&state_path)?;
            writeln!(out, "cleared dataset state at {}", state_path.display())?;
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
    /// `document_jobs` (queue) row count.
    queue_jobs: i64,
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
    Ok(db.with_conn(|conn| -> Result<Stats, DbError> {
        let exec = ConnectionOrTx::Connection(conn);
        Ok(Stats {
            documents: DocumentDao::new(exec).count(&DocumentFilter::default())?,
            chunks: ChunkDao::new(exec).count()?,
            entities: EntityDao::new(exec).count(&EntityFilter::default())?,
            entity_links: EntityLinkDao::new(exec).count()?,
            facts: FactDao::new(exec).count()?,
            // `usize` row counts fit in `i64` on every supported platform.
            queue_jobs: DocumentJobDao::new(exec).list(None, None)?.len() as i64,
        })
    })??)
}

/// Renders the statistics block to `out`.
///
/// # Errors
///
/// [`CliError::Io`] when `out` cannot be written.
fn print_stats(out: &mut dyn Write, stats: &Stats) -> Result<(), CliError> {
    writeln!(out, "Dataset Statistics:")?;
    writeln!(out, "{}", "-".repeat(60))?;
    writeln!(out, "Documents:    {}", stats.documents)?;
    writeln!(out, "Chunks:       {}", stats.chunks)?;
    writeln!(out, "Entities:     {}", stats.entities)?;
    writeln!(out, "Entity links: {}", stats.entity_links)?;
    writeln!(out, "Facts:        {}", stats.facts)?;
    writeln!(out, "Queue jobs:   {}", stats.queue_jobs)?;
    writeln!(out, "{}", "-".repeat(60))?;
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
/// action; reused by the `serve` force-rebuild recovery path,
/// remove-direct-ingest task 1.2).
///
/// The state directory holds the knowledge database
/// (`state/db/knowledge.db`) and the vector index (`state/vectors/`), so
/// both go away in one shot — no per-table deletes, no cascade ordering.
/// After a clear, a `serve` restart re-enqueues the source files at startup
/// reconcile and the worker re-embeds them, recreating the directory.
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
            DocumentJobDao::new(exec).enqueue_index("/docs/a.md", "/docs", None)?;
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
        assert_eq!(table_count(db, "document_jobs"), 1);
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
        assert!(stdout.contains("Documents:    1"), "{stdout:?}");
        assert!(stdout.contains("Chunks:       2"), "{stdout:?}");
        assert!(stdout.contains("Entities:     3"), "{stdout:?}");
        assert!(stdout.contains("Entity links: 1"), "{stdout:?}");
        assert!(stdout.contains("Facts:        1"), "{stdout:?}");
        assert!(stdout.contains("Queue jobs:   1"), "{stdout:?}");
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
