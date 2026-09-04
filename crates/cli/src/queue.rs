//! `queue` subcommand body: inspect and repair the document job queue
//! (`document_jobs`, migration `2-document-jobs`).
//!
//! New Rust operational command (document-jobs-queue task 1.7): the document
//! job queue is a native concept with its own operational surface.
//!
//! - `queue status [--source PATH] [--status NAME]` prints the queue table
//!   (columns: path, source, status, attempts, last_error, next_attempt_at)
//!   from [`DocumentJobDao::list`];
//! - `queue reset-retries [--source PATH] [--path PATH]` re-queues `error`
//!   jobs via [`DocumentJobDao::reset_retries`] (status -> `pending`,
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
use db::{ConnectionOrTx, Db, DocumentJob, DocumentJobDao};

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
/// queue queries fail, [`CliError::Unsupported`] when no `error` job matches
/// `reset-retries --path`, [`CliError::Io`] when `out` cannot be written.
pub fn queue_flow(req: &QueueRequest, out: &mut dyn Write) -> Result<(), CliError> {
    // The queue commands need only the paths + dataset to locate the
    // knowledge database (no `validate`: the queue is operational state, not
    // ingestion config).
    let config = load_config(&req.cfg_path, req.dataset.as_deref())?;
    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir))?;

    match &req.action {
        QueueAction::Status { source, status } => {
            let jobs = list_jobs(&db, status.as_deref(), source.as_deref())?;
            print_status(out, &jobs)
        }
        QueueAction::ResetRetries { source, path } => {
            reset_retries(&db, source.as_deref(), path.as_deref(), out)
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
            message: "no dataset configured (dataset.name is empty); the document job \
                      queue lives in the dataset-bound knowledge database"
                .to_string(),
        }));
    }
    Ok(config)
}

/// The queue rows for the `status` action (exact-status and source-prefix
/// filters, both optional).
fn list_jobs(
    db: &Db,
    status: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<DocumentJob>, CliError> {
    Ok(db.with_conn(|conn| {
        DocumentJobDao::new(ConnectionOrTx::Connection(conn)).list(status, source)
    })??)
}

/// Renders the `queue status` table (columns per the cli-surface spec:
/// path, source, status, attempts, last_error, next_attempt_at).
///
/// `last_error` is the free-form column (never truncated; `-` when absent);
/// `next_attempt_at` is Unix seconds.
///
/// # Errors
///
/// [`CliError::Io`] when `out` cannot be written.
fn print_status(out: &mut dyn Write, jobs: &[DocumentJob]) -> Result<(), CliError> {
    writeln!(out, "Document Job Queue:")?;
    writeln!(out, "{}", "-".repeat(90))?;
    writeln!(
        out,
        "{:<40} {:<24} {:<10} {:<8} {:<40} NEXT_ATTEMPT_AT",
        "PATH", "SOURCE", "STATUS", "ATTEMPTS", "LAST_ERROR"
    )?;
    writeln!(out, "{}", "-".repeat(90))?;
    for job in jobs {
        writeln!(
            out,
            "{:<40} {:<24} {:<10} {:<8} {:<40} {}",
            job.path,
            job.source_path,
            job.status,
            job.attempts,
            job.last_error.as_deref().unwrap_or("-"),
            job.next_attempt_at,
        )?;
    }
    writeln!(out, "{}", "-".repeat(90))?;
    writeln!(out, "{} jobs", jobs.len())?;
    writeln!(out)?;
    Ok(())
}

/// The `queue reset-retries` action: re-queue `error` jobs (status ->
/// `pending`, attempts -> 0, next_attempt_at -> now).
///
/// With `path`, exactly that job is re-queued (an error when no `error` job
/// has it). Without, every `error` job (optionally narrowed to the `source`
/// prefix) is re-queued and the count is reported.
///
/// # Errors
///
/// [`CliError::Db`] when the queue queries fail,
/// [`CliError::Unsupported`] when no `error` job matches `--path`,
/// [`CliError::Io`] when `out` cannot be written.
fn reset_retries(
    db: &Db,
    source: Option<&str>,
    path: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match path {
        Some(path) => {
            let changed = db.with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).reset_retries(path)
            })??;
            if !changed {
                return Err(CliError::Unsupported(format!(
                    "no error job for path {path:?} (only jobs in status 'error' can be re-queued)"
                )));
            }
            writeln!(
                out,
                "1 job re-queued: {path} (error -> pending, attempts -> 0)"
            )?;
            Ok(())
        }
        None => {
            let jobs = list_jobs(db, Some("error"), source)?;
            let mut reset = 0usize;
            db.with_conn(|conn| -> Result<(), db::DbError> {
                let dao = DocumentJobDao::new(ConnectionOrTx::Connection(conn));
                for job in &jobs {
                    if dao.reset_retries(&job.path)? {
                        reset += 1;
                    }
                }
                Ok(())
            })??;
            writeln!(
                out,
                "{reset} job(s) re-queued (error -> pending, attempts -> 0)"
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    /// Seeds `path` into the queue and forces its state (status/attempts/
    /// last_error) directly: the DAO only reaches `error` through the
    /// worker's failure path, which the CLI tests do not exercise.
    fn seed_job(
        db: &Db,
        path: &str,
        source: &str,
        status: &str,
        attempts: i32,
        last_error: Option<&str>,
    ) {
        db.with_conn(|conn| -> Result<(), db::DbError> {
            let jobs = DocumentJobDao::new(ConnectionOrTx::Connection(conn));
            jobs.enqueue_index(path, source, None)?;
            conn.execute(
                "UPDATE document_jobs SET status = ?1, attempts = ?2, last_error = ?3 \
                 WHERE path = ?4",
                rusqlite::params![status, attempts, last_error, path],
            )?;
            Ok(())
        })
        .expect("with_conn")
        .expect("seed job");
    }

    /// The job row for `path` (must exist).
    fn get_job(db: &Db, path: &str) -> DocumentJob {
        db.with_conn(|conn| DocumentJobDao::new(ConnectionOrTx::Connection(conn)).get_by_path(path))
            .expect("with_conn")
            .expect("query job")
            .expect("the job must exist")
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

    /// The whitespace-separated columns of the status row for `path`
    /// (path, source, status, attempts, then the free-form last_error,
    /// then next_attempt_at).
    fn row_fields<'a>(stdout: &'a str, path: &str) -> Vec<&'a str> {
        stdout
            .lines()
            .find(|line| line.contains(path))
            .unwrap_or_else(|| panic!("no table row for {path}:\n{stdout}"))
            .split_whitespace()
            .collect()
    }

    // --- queue status ------------------------------------------------------

    #[test]
    fn status_prints_queue_table() {
        let f = QueueFixture::new("status");
        seed_job(
            &f.db,
            "/docs/a.md",
            "/docs",
            "error",
            3,
            Some("parse error: boom"),
        );
        seed_job(&f.db, "/docs/b.md", "/docs", "pending", 0, None);
        seed_job(&f.db, "/docs/c.md", "/docs", "done", 1, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::Status {
                source: None,
                status: None,
            },
        );
        result.expect("status must succeed");
        assert!(stdout.starts_with("Document Job Queue:\n"), "{stdout:?}");
        for column in [
            "PATH",
            "SOURCE",
            "STATUS",
            "ATTEMPTS",
            "LAST_ERROR",
            "NEXT_ATTEMPT_AT",
        ] {
            assert!(stdout.contains(column), "column {column}: {stdout:?}");
        }
        assert!(stdout.contains("3 jobs"), "{stdout:?}");

        let fields = row_fields(&stdout, "/docs/a.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["/docs/a.md", "/docs", "error", "3"],
            "a.md row: {fields:?}"
        );
        assert!(fields.contains(&"parse"), "a.md last_error: {fields:?}");
        let fields = row_fields(&stdout, "/docs/b.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["/docs/b.md", "/docs", "pending", "0"],
            "{fields:?}"
        );
        assert_eq!(fields[4], "-", "no last_error: {fields:?}");
        let fields = row_fields(&stdout, "/docs/c.md");
        assert_eq!(
            fields.iter().take(4).copied().collect::<Vec<_>>(),
            vec!["/docs/c.md", "/docs", "done", "1"],
            "{fields:?}"
        );
    }

    #[test]
    fn status_filters_by_status_and_source() {
        let f = QueueFixture::new("status-filter");
        seed_job(&f.db, "/src1/a.md", "/src1", "error", 3, Some("e1"));
        seed_job(&f.db, "/src1/b.md", "/src1", "pending", 0, None);
        seed_job(&f.db, "/src2/c.md", "/src2", "error", 3, Some("e2"));

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
        assert!(stdout.contains("2 jobs"), "{stdout:?}");

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
        assert!(stdout.contains("1 jobs"), "{stdout:?}");
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
        assert!(stdout.contains("Document Job Queue:"), "{stdout:?}");
        assert!(stdout.contains("0 jobs"), "{stdout:?}");
    }

    // --- queue reset-retries -----------------------------------------------

    #[test]
    fn reset_retries_path_flips_error_to_pending() {
        let f = QueueFixture::new("reset-path");
        seed_job(
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
                path: Some("/docs/a.md".to_string()),
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("1 job re-queued"), "{stdout:?}");

        let job = get_job(&f.db, "/docs/a.md");
        assert_eq!(job.status, "pending");
        assert_eq!(job.attempts, 0);
    }

    #[test]
    fn reset_retries_path_without_error_job_is_an_error() {
        let f = QueueFixture::new("reset-path-pending");
        seed_job(&f.db, "/docs/a.md", "/docs", "pending", 0, None);

        let (_, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                path: Some("/docs/a.md".to_string()),
            },
        );
        let err = result.expect_err("a pending job must not be re-queued");
        assert!(
            matches!(err, CliError::Unsupported(ref msg) if msg.contains("no error job")),
            "got: {err:?}"
        );
        assert_eq!(
            get_job(&f.db, "/docs/a.md").status,
            "pending",
            "the row must be untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_resets_only_error_jobs() {
        let f = QueueFixture::new("reset-bulk");
        seed_job(&f.db, "/docs/a.md", "/docs", "error", 3, Some("e1"));
        seed_job(&f.db, "/docs/b.md", "/docs", "error", 2, Some("e2"));
        seed_job(&f.db, "/docs/c.md", "/docs", "pending", 0, None);
        seed_job(&f.db, "/docs/d.md", "/docs", "done", 1, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                path: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("2 job(s) re-queued"), "{stdout:?}");

        assert_eq!(get_job(&f.db, "/docs/a.md").status, "pending");
        assert_eq!(get_job(&f.db, "/docs/a.md").attempts, 0);
        assert_eq!(get_job(&f.db, "/docs/b.md").status, "pending");
        assert_eq!(get_job(&f.db, "/docs/b.md").attempts, 0);
        assert_eq!(
            get_job(&f.db, "/docs/c.md").status,
            "pending",
            "the pending row is untouched"
        );
        assert_eq!(
            get_job(&f.db, "/docs/d.md").status,
            "done",
            "the done row is untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_source_filter() {
        let f = QueueFixture::new("reset-bulk-source");
        seed_job(&f.db, "/src1/a.md", "/src1", "error", 3, Some("e1"));
        seed_job(&f.db, "/src2/b.md", "/src2", "error", 3, Some("e2"));

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: Some("/src1".to_string()),
                path: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("1 job(s) re-queued"), "{stdout:?}");
        assert_eq!(get_job(&f.db, "/src1/a.md").status, "pending");
        assert_eq!(
            get_job(&f.db, "/src2/b.md").status,
            "error",
            "the other source is untouched"
        );
    }

    #[test]
    fn reset_retries_bulk_without_error_jobs_reports_zero() {
        let f = QueueFixture::new("reset-bulk-none");
        seed_job(&f.db, "/docs/a.md", "/docs", "pending", 0, None);

        let (stdout, result) = run_flow(
            &f,
            QueueAction::ResetRetries {
                source: None,
                path: None,
            },
        );
        result.expect("reset must succeed");
        assert!(stdout.contains("0 job(s) re-queued"), "{stdout:?}");
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
