//! `queue` subcommand body: inspect and repair the event task queue
//! (`queue_tasks`, init migration).
//!
//! New Rust operational command (event-queue-incremental-linking task 1.2):
//! the event queue is a native concept with its own operational surface.
//!
//! - `queue status [--source PATH] [--status NAME]` prints the queue table
//!   (columns: type, identity, status, attempts, last_error, next_attempt_at)
//!   from [`QueueTaskDao::list`];
//! - `queue reset-retries [--source PATH] [--identity PATH]` re-queues
//!   `error` tasks via [`QueueTaskDao::reset_retries`] (status -> `pending`,
//!   attempts -> 0, next_attempt_at -> now); the background worker then
//!   re-processes them.
//!
//! Like the other subcommands the command re-loads the config from the
//! resolved path; it needs only `paths` + `dataset` (the knowledge DB is
//! dataset-bound), so no embedding model or ONNX runtime is touched.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use config::{Config, ConfigError, load};
use db::{ConnectionOrTx, Db, QueueTask, QueueTaskDao};

use crate::cli::QueueAction;
use crate::error::CliError;
use crate::serve::bootstrap::open_db;

/// One `queue` invocation: the resolved config path plus the clap `queue`
/// sub-action and its flags.
pub struct QueueRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// `--dataset` dataset name override (wins over `config.dataset.name`).
    pub dataset: Option<String>,
    /// The queue sub-action.
    pub action: QueueAction,
}

/// The `queue` subcommand entry point.
///
/// Loads the config, opens the dataset-bound knowledge database, and
/// dispatches the sub-action. Human-readable output goes to stdout; on error
/// the message goes to stderr and the exit code is non-zero (every fatal
/// error exits 1).
pub fn run_queue(req: &QueueRequest) -> ExitCode {
    match queue_flow(req, &mut std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

/// Config load + knowledge-db open + sub-action dispatch (the production
/// path of [`run_queue`]).
///
/// `out` receives the human-readable table output (production: stdout); the
/// tests drive this seam with an in-memory buffer.
///
/// # Errors
///
/// [`CliError::Config`] for config load/validation failures,
/// [`CliError::Db`] when the knowledge database cannot be opened or the
/// queue queries fail, [`CliError::Unsupported`] when no `error` task
/// matches `reset-retries --identity`, [`CliError::Io`] when `out` cannot be
/// written.
pub fn queue_flow(req: &QueueRequest, out: &mut dyn Write) -> Result<(), CliError> {
    // The queue commands need only the paths + dataset to locate the
    // knowledge database (no `validate`: the queue is operational state, not
    // ingestion config).
    let config = load_config(&req.cfg_path, req.dataset.as_deref())?;
    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir))?;

    match &req.action {
        QueueAction::Status { source, status } => {
            let tasks = list_tasks(&db, status.as_deref(), source.as_deref())?;
            print_status(out, &tasks)
        }
        QueueAction::ResetRetries { source, identity } => {
            reset_retries(&db, source.as_deref(), identity.as_deref(), out)
        }
    }
}

/// Loads the config for the `queue` commands: load + defaults + the
/// `--dataset` override, and requires a non-empty dataset name (the queue
/// lives in the dataset-bound knowledge database).
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
            message: "no dataset configured (dataset.name is empty); the event task \
                      queue lives in the dataset-bound knowledge database"
                .to_string(),
        }));
    }
    Ok(config)
}

/// The queue rows for the `status` action (exact-status and source-prefix
/// filters, both optional).
fn list_tasks(
    db: &Db,
    status: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<QueueTask>, CliError> {
    db.with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(source, status))
        .map_err(CliError::Db)
        .and_then(|r| r.map_err(|e| CliError::Unsupported(format!("queue task: {e}"))))
}

/// Renders the `queue status` table (columns per the cli-surface spec:
/// type, identity, status, attempts, last_error, next_attempt_at).
///
/// `last_error` is the free-form column (never truncated; `-` when absent);
/// `next_attempt_at` is Unix seconds.
///
/// # Errors
///
/// [`CliError::Io`] when `out` cannot be written.
fn print_status(out: &mut dyn Write, tasks: &[QueueTask]) -> Result<(), CliError> {
    writeln!(out, "Event Queue:")?;
    writeln!(out, "{}", "-".repeat(90))?;
    writeln!(
        out,
        "{:<12} {:<40} {:<10} {:<8} {:<40} NEXT_ATTEMPT_AT",
        "TYPE", "IDENTITY", "STATUS", "ATTEMPTS", "LAST_ERROR"
    )?;
    writeln!(out, "{}", "-".repeat(90))?;
    for task in tasks {
        writeln!(
            out,
            "{:<12} {:<40} {:<10} {:<8} {:<40} {}",
            task.task_type,
            task.identity,
            task.status,
            task.attempts,
            task.last_error.as_deref().unwrap_or("-"),
            task.next_attempt_at,
        )?;
    }
    writeln!(out, "{}", "-".repeat(90))?;
    writeln!(out, "{} tasks", tasks.len())?;
    writeln!(out)?;
    Ok(())
}

/// The `queue reset-retries` action: re-queue `error` tasks (status ->
/// `pending`, attempts -> 0, next_attempt_at -> now).
///
/// With `identity`, exactly that task is re-queued (an error when no
/// `error` task has it). Without, every `error` task (optionally narrowed to
/// the `source` prefix) is re-queued and the count is reported.
///
/// # Errors
///
/// [`CliError::Db`] when the queue queries fail,
/// [`CliError::Unsupported`] when no `error` task matches `--identity`,
/// [`CliError::Io`] when `out` cannot be written.
fn reset_retries(
    db: &Db,
    source: Option<&str>,
    identity: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let reset = db
        .with_conn(|conn| {
            QueueTaskDao::new(ConnectionOrTx::Connection(conn)).reset_retries(source, identity)
        })
        .map_err(CliError::Db)?
        .map_err(|e| CliError::Unsupported(format!("queue task: {e}")))?;
    if identity.is_some() && reset == 0 {
        return Err(CliError::Unsupported(format!(
            "no error task for identity {identity:?} (only tasks in status 'error' can be re-queued)"
        )));
    }
    writeln!(
        out,
        "{reset} task(s) re-queued (error -> pending, attempts -> 0)"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use db::QueueTaskType;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-queue-{tag}-{}-{id}",
                std::process::id()
            ));
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
    /// knowledge database at the derived dataset path.
    struct QueueFixture {
        dir: TempDir,
        cfg_path: PathBuf,
        db: Db,
    }

    impl QueueFixture {
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
            Self { dir, cfg_path, db }
        }
    }

    /// Seeds a `doc:index` task at `identity` and forces its state
    /// (status/attempts/last_error) directly: the DAO only reaches `error`
    /// through the worker's failure path, which the CLI tests do not
    /// exercise.
    fn seed_task(
        db: &Db,
        identity: &str,
        source: &str,
        status: &str,
        attempts: i32,
        last_error: Option<&str>,
    ) {
        db.with_conn(|conn| -> Result<(), db::QueueTaskError> {
            let tasks = QueueTaskDao::new(ConnectionOrTx::Connection(conn));
            tasks.enqueue(
                QueueTaskType::DocIndex,
                identity,
                &db::DocIndexPayload {
                    source_path: source.to_owned(),
                    content_hash: None,
                    ops: vec![db::ReIndexOp::Full],
                },
                100,
            )?;
            conn.execute(
                "UPDATE queue_tasks SET status = ?1, attempts = ?2, last_error = ?3 \
                 WHERE identity = ?4 AND type = 'doc:index'",
                rusqlite::params![status, attempts, last_error, identity],
            )
            .map_err(|e| db::QueueTaskError::Db(e.into()))?;
            Ok(())
        })
        .expect("with_conn")
        .expect("seed task");
    }

    /// The task row for `identity` (must exist).
    fn get_task(db: &Db, identity: &str) -> QueueTask {
        db.with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
            .expect("with_conn")
            .expect("query task")
            .into_iter()
            .find(|t| t.identity == identity)
            .expect("the task must exist")
    }

    /// Runs the `queue` flow with the given action; returns the rendered
    /// output and the flow result.
    fn run_flow(f: &QueueFixture, action: QueueAction) -> (String, Result<(), CliError>) {
        let req = QueueRequest {
            cfg_path: f.cfg_path.clone(),
            dataset: None,
            action,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = queue_flow(&req, &mut out);
        (String::from_utf8_lossy(&out).into_owned(), result)
    }

    /// The whitespace-separated columns of the status row for `identity`.
    fn row_fields<'a>(stdout: &'a str, identity: &str) -> Vec<&'a str> {
        stdout
            .lines()
            .find(|line| line.contains(identity))
            .unwrap_or_else(|| panic!("no table row for {identity}:\n{stdout}"))
            .split_whitespace()
            .collect()
    }

    // --- queue status ------------------------------------------------------

    #[test]
    fn status_prints_queue_table() {
        let f = QueueFixture::new("status");
        seed_task(
            &f.db,
            "/docs/a.md",
            "/docs",
            "error",
            3,
            Some("parse error: boom"),
        );
        seed_task(&f.db, "/docs/b.md", "/docs", "pending", 0, None);
        seed_task(&f.db, "/docs/c.md", "/docs", "done", 1, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: None,
                status: None,
            },
        );
        result.expect("status must succeed");
        assert!(stdout.starts_with("Event Queue:\n"), "{stdout:?}");
        for column in [
            "TYPE",
            "IDENTITY",
            "STATUS",
            "ATTEMPTS",
            "LAST_ERROR",
            "NEXT_ATTEMPT_AT",
        ] {
            assert!(stdout.contains(column), "column {column}: {stdout:?}");
        }
        assert!(stdout.contains("3 tasks"), "{stdout:?}");

        let fields = row_fields(&stdout, "/docs/a.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["doc:index", "/docs/a.md", "error", "3"],
            "a.md row: {fields:?}"
        );
        assert!(fields.contains(&"parse"), "a.md last_error: {fields:?}");
        let fields = row_fields(&stdout, "/docs/b.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["doc:index", "/docs/b.md", "pending", "0"],
            "{fields:?}"
        );
        assert_eq!(fields[4], "-", "no last_error: {fields:?}");
        let fields = row_fields(&stdout, "/docs/c.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["doc:index", "/docs/c.md", "done", "1"],
            "{fields:?}"
        );
    }

    #[test]
    fn status_filters_by_status_and_source() {
        let f = QueueFixture::new("status-filter");
        seed_task(&f.db, "/src1/a.md", "/src1", "error", 3, Some("e1"));
        seed_task(&f.db, "/src1/b.md", "/src1", "pending", 0, None);
        seed_task(&f.db, "/src2/c.md", "/src2", "error", 3, Some("e2"));

        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: None,
                status: Some("error".to_string()),
            },
        );
        result.expect("status must succeed");
        assert!(stdout.contains("/src1/a.md"), "{stdout:?}");
        assert!(stdout.contains("/src2/c.md"), "{stdout:?}");
        assert!(
            !stdout.contains("/src1/b.md"),
            "the pending row must be filtered: {stdout:?}"
        );
        assert!(stdout.contains("2 tasks"), "{stdout:?}");

        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: Some("/src1".to_string()),
                status: None,
            },
        );
        result.expect("status must succeed");
        assert!(stdout.contains("/src1/a.md"), "{stdout:?}");
        assert!(stdout.contains("/src1/b.md"), "{stdout:?}");
        assert!(
            !stdout.contains("/src2/c.md"),
            "the other source must be filtered: {stdout:?}"
        );

        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: Some("/src1".to_string()),
                status: Some("pending".to_string()),
            },
        );
        result.expect("status must succeed");
        assert!(stdout.contains("/src1/b.md"), "{stdout:?}");
        assert!(stdout.contains("1 tasks"), "{stdout:?}");
    }

    #[test]
    fn status_empty_queue_prints_header_only() {
        let f = QueueFixture::new("status-empty");
        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: None,
                status: None,
            },
        );
        result.expect("status must succeed");
        assert!(stdout.contains("Event Queue:"), "{stdout:?}");
        assert!(stdout.contains("0 tasks"), "{stdout:?}");
    }

    // --- queue reset-retries -----------------------------------------------

    #[test]
    fn reset_retries_identity_flips_error_to_pending() {
        let f = QueueFixture::new("reset-identity");
        seed_task(
            &f.db,
            "/docs/a.md",
            "/docs",
            "error",
            3,
            Some("parse error: boom"),
        );

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                identity: Some("/docs/a.md".to_string()),
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("1 task(s) re-queued"), "{stdout:?}");

        let task = get_task(&f.db, "/docs/a.md");
        assert_eq!(task.status, "pending");
        assert_eq!(task.attempts, 0);
    }

    #[test]
    fn reset_retries_identity_without_error_task_is_an_error() {
        let f = QueueFixture::new("reset-identity-pending");
        seed_task(&f.db, "/docs/a.md", "/docs", "pending", 0, None);

        let (_, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                identity: Some("/docs/a.md".to_string()),
            },
        );
        let err = result.expect_err("a pending task must not be re-queued");
        assert!(
            matches!(err, CliError::Unsupported(ref msg) if msg.contains("no error task")),
            "got: {err:?}"
        );
        assert_eq!(
            get_task(&f.db, "/docs/a.md").status,
            "pending",
            "the row must be untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_resets_only_error_tasks() {
        let f = QueueFixture::new("reset-bulk");
        seed_task(&f.db, "/docs/a.md", "/docs", "error", 3, Some("e1"));
        seed_task(&f.db, "/docs/b.md", "/docs", "error", 2, Some("e2"));
        seed_task(&f.db, "/docs/c.md", "/docs", "pending", 0, None);
        seed_task(&f.db, "/docs/d.md", "/docs", "done", 1, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                identity: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("2 task(s) re-queued"), "{stdout:?}");

        assert_eq!(get_task(&f.db, "/docs/a.md").status, "pending");
        assert_eq!(get_task(&f.db, "/docs/a.md").attempts, 0);
        assert_eq!(get_task(&f.db, "/docs/b.md").status, "pending");
        assert_eq!(get_task(&f.db, "/docs/b.md").attempts, 0);
        assert_eq!(
            get_task(&f.db, "/docs/c.md").status,
            "pending",
            "the pending row is untouched"
        );
        assert_eq!(
            get_task(&f.db, "/docs/d.md").status,
            "done",
            "the done row is untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_source_filter() {
        let f = QueueFixture::new("reset-bulk-source");
        seed_task(&f.db, "/src1/a.md", "/src1", "error", 3, Some("e1"));
        seed_task(&f.db, "/src2/b.md", "/src2", "error", 3, Some("e2"));

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: Some("/src1".to_string()),
                identity: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("1 task(s) re-queued"), "{stdout:?}");
        assert_eq!(get_task(&f.db, "/src1/a.md").status, "pending");
        assert_eq!(
            get_task(&f.db, "/src2/b.md").status,
            "error",
            "the other source is untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_without_error_tasks_reports_zero() {
        let f = QueueFixture::new("reset-bulk-none");
        seed_task(&f.db, "/docs/a.md", "/docs", "pending", 0, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                identity: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("0 task(s) re-queued"), "{stdout:?}");
    }

    // --- config / exit codes -------------------------------------------------

    #[test]
    fn missing_dataset_is_an_error() {
        let dir = TempDir::new("no-dataset");
        let cfg_path = dir.as_ref().join("config.yaml");
        std::fs::write(&cfg_path, "paths:\n  workspace_dir: /tmp/nowhere\n").expect("write config");
        let req = QueueRequest {
            cfg_path,
            dataset: None,
            action: QueueAction::Status {
                source: None,
                status: None,
            },
        };
        let mut out: Vec<u8> = Vec::new();
        let err = queue_flow(&req, &mut out).expect_err("no dataset must fail");
        assert!(
            matches!(err, CliError::Config(ConfigError::Validation { .. })),
            "got: {err:?}"
        );
    }

    #[test]
    fn run_queue_maps_exit_codes() {
        let f = QueueFixture::new("exit-codes");
        let ok = run_queue(&QueueRequest {
            cfg_path: f.cfg_path.clone(),
            dataset: None,
            action: QueueAction::Status {
                source: None,
                status: None,
            },
        });
        assert_eq!(ok, ExitCode::SUCCESS, "status exits 0");

        let fail = run_queue(&QueueRequest {
            cfg_path: f.dir.as_ref().join("missing.yaml"),
            dataset: None,
            action: QueueAction::Status {
                source: None,
                status: None,
            },
        });
        assert_eq!(fail, ExitCode::FAILURE, "missing config exits 1");
    }
}
