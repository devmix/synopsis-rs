//! Binary-level tests for the `queue` subcommand (document-jobs-queue task
//! 1.7): `queue status` prints the queue table for a seeded `document_jobs`
//! table (with the `--status` filter), and `queue reset-retries` re-queues
//! `error` jobs (verified by a subsequent `queue status`).

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};

use db::{ConnectionOrTx, Db, QueueTaskDao};

fn synopsis() -> Command {
    Command::new(env!("CARGO_BIN_EXE_synopsis"))
}

fn run(args: &[&str]) -> Output {
    synopsis().args(args).output().unwrap()
}

/// A temp-dir fixture: the config (paths + dataset `edtech`) and the
/// migrated knowledge database at the derived dataset path, seeded with
/// three `document_jobs` rows: two `error` (attempts 3 and 1) and one
/// `pending`.
struct Fixture {
    dir: PathBuf,
    cfg: PathBuf,
}

/// The config + migrated knowledge DB at the derived dataset path (the
/// fixture boilerplate shared by [`fixture`] and [`entity_link_fixture`]).
fn base_fixture(tag: &str) -> (PathBuf, PathBuf, Db) {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "synopsis-cli-queue-bin-{tag}-{}-{ns}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = dir.join("workspace");
    let cfg = dir.join("config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "paths:\n  workspace_dir: {workspace}\ndataset:\n  name: edtech\n",
            workspace = workspace.display()
        ),
    )
    .unwrap();

    let db = Db::open_knowledge(
        workspace
            .join("datasets")
            .join("edtech")
            .join("state")
            .join("db")
            .join("knowledge.db"),
    )
    .expect("open knowledge db");
    (dir, cfg, db)
}

fn fixture(tag: &str) -> Fixture {
    let (dir, cfg, db) = base_fixture(tag);
    seed(
        &db,
        "/docs/a.md",
        "/docs",
        "error",
        3,
        Some("parse error: boom"),
    );
    seed(&db, "/docs/b.md", "/docs", "error", 1, Some("ner timeout"));
    seed(&db, "/docs/c.md", "/docs", "pending", 0, None);
    drop(db);

    Fixture { dir, cfg }
}

/// Like [`fixture`] but with one `entity:link` row (identity `"42"`: the
/// document id) in `error` status, so the binary-level tests can assert the
/// new task type's visibility and re-queue.
fn entity_link_fixture(tag: &str) -> Fixture {
    let (dir, cfg, db) = base_fixture(tag);
    seed(
        &db,
        "/docs/a.md",
        "/docs",
        "error",
        3,
        Some("parse error: boom"),
    );
    seed(&db, "/docs/c.md", "/docs", "pending", 0, None);
    seed_entity_link(
        &db,
        "42",
        vec![7, 8],
        "error",
        3,
        Some("linker failure: boom"),
    );
    drop(db);

    Fixture { dir, cfg }
}

/// Seeds `path` into the queue and forces its state (status/attempts/
/// last_error) directly: the DAO only reaches `error` through the worker's
/// failure path, which these tests do not exercise.
fn seed(db: &Db, path: &str, source: &str, status: &str, attempts: i32, last_error: Option<&str>) {
    db.with_conn(|conn| -> Result<(), db::QueueTaskError> {
        let tasks = QueueTaskDao::new(ConnectionOrTx::Connection(conn));
        tasks.enqueue(
            db::QueueTaskType::DocIndex,
            path,
            &db::DocIndexPayload {
                source_path: source.to_owned(),
                content_hash: None,
                ops: vec![db::ReIndexOp::Full],
            },
            0,
        )?;
        conn.execute(
            "UPDATE queue_tasks SET status = ?1, attempts = ?2, last_error = ?3 \
             WHERE identity = ?4 AND type = 'doc:index'",
            rusqlite::params![status, attempts, last_error, path],
        )
        .map_err(|e| db::QueueTaskError::Db(e.into()))?;
        Ok(())
    })
    .expect("with_conn")
    .expect("seed task");
}

/// Seeds an `entity:link` task at `identity` (the document id) and forces
/// its state (status/attempts/last_error) directly.
fn seed_entity_link(
    db: &Db,
    identity: &str,
    entity_ids: Vec<i64>,
    status: &str,
    attempts: i32,
    last_error: Option<&str>,
) {
    db.with_conn(|conn| -> Result<(), db::QueueTaskError> {
        let tasks = QueueTaskDao::new(ConnectionOrTx::Connection(conn));
        tasks.enqueue(
            db::QueueTaskType::EntityLink,
            identity,
            &db::EntityLinkPayload { entity_ids },
            0,
        )?;
        conn.execute(
            "UPDATE queue_tasks SET status = ?1, attempts = ?2, last_error = ?3 \
             WHERE identity = ?4 AND type = 'entity:link'",
            rusqlite::params![status, attempts, last_error, identity],
        )
        .map_err(|e| db::QueueTaskError::Db(e.into()))?;
        Ok(())
    })
    .expect("with_conn")
    .expect("seed task");
}

/// The `│`-separated cell values of the status row for `path` (type,
/// identity, status, attempts, then the free-form last_error, then
/// next_attempt_at). The row is the first box-drawing line (leading `│`)
/// where the identity appears as a WHOLE whitespace field: the binary also
/// prints tracing log lines to stdout, and a substring match would collide
/// with the temp-dir timestamps they carry.
fn row_fields<'a>(stdout: &'a str, path: &str) -> Vec<&'a str> {
    stdout
        .lines()
        .find(|line| line.starts_with('│') && line.split_whitespace().any(|field| field == path))
        .unwrap_or_else(|| panic!("no table row for {path}:\n{stdout}"))
        .split('│')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect()
}

#[test]
fn queue_status_prints_seeded_queue_table_and_exits_zero() {
    let f = fixture("status");
    let out = run(&["--config", f.cfg.to_str().unwrap(), "queue", "status"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Event Queue:"), "{stdout:?}");
    // Every console-rendered table line stays within 120 display columns
    // (whole-table `Width::wrap`). (Tracing log lines on the same stdout are
    // the binary's own logging, not the console output.)
    for line in stdout.lines().filter(|line| line.starts_with('│')) {
        assert!(line.chars().count() <= 120, "line fits 120: {line:?}");
    }
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
    assert!(stdout.contains("/docs/a.md"), "{stdout:?}");
    assert!(stdout.contains("parse error: boom"), "{stdout:?}");
    assert!(stdout.contains("/docs/b.md"), "{stdout:?}");
    assert!(stdout.contains("ner timeout"), "{stdout:?}");
    assert!(stdout.contains("/docs/c.md"), "{stdout:?}");
    assert!(stdout.contains("3 tasks"), "{stdout:?}");

    let fields = row_fields(&stdout, "/docs/a.md");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["doc:index", "/docs/a.md", "error", "3"],
        "{fields:?}"
    );
    let fields = row_fields(&stdout, "/docs/c.md");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["doc:index", "/docs/c.md", "pending", "0"],
        "{fields:?}"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn queue_status_status_filter_shows_only_matching_rows() {
    let f = fixture("status-filter");
    let out = run(&[
        "--config",
        f.cfg.to_str().unwrap(),
        "queue",
        "status",
        "--status",
        "error",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("/docs/a.md"), "{stdout:?}");
    assert!(stdout.contains("/docs/b.md"), "{stdout:?}");
    assert!(
        !stdout.contains("/docs/c.md"),
        "the pending row must be filtered: {stdout:?}"
    );
    assert!(stdout.contains("2 tasks"), "{stdout:?}");
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn queue_reset_retries_path_flips_error_to_pending() {
    let f = fixture("reset-path");
    let out = run(&[
        "--config",
        f.cfg.to_str().unwrap(),
        "queue",
        "reset-retries",
        "--identity",
        "/docs/a.md",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 task(s) re-queued"), "{stdout:?}");

    // The task acceptance: verified by a subsequent `queue status`.
    let out = run(&["--config", f.cfg.to_str().unwrap(), "queue", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fields = row_fields(&stdout, "/docs/a.md");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["doc:index", "/docs/a.md", "pending", "0"],
        "a.md must be re-queued: {fields:?}"
    );
    // The other rows are untouched.
    let fields = row_fields(&stdout, "/docs/b.md");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["doc:index", "/docs/b.md", "error", "1"],
        "b.md must stay error: {fields:?}"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn queue_reset_retries_bulk_resets_all_error_jobs() {
    let f = fixture("reset-bulk");
    let out = run(&[
        "--config",
        f.cfg.to_str().unwrap(),
        "queue",
        "reset-retries",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("2 task(s) re-queued"), "{stdout:?}");

    let out = run(&["--config", f.cfg.to_str().unwrap(), "queue", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for path in ["/docs/a.md", "/docs/b.md", "/docs/c.md"] {
        let fields = row_fields(&stdout, path);
        assert_eq!(fields[2], "pending", "{path} must be pending: {fields:?}");
        assert_eq!(fields[3], "0", "{path} attempts must be 0: {fields:?}");
    }
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn queue_reset_retries_unknown_path_exits_one() {
    let f = fixture("reset-missing");
    let out = run(&[
        "--config",
        f.cfg.to_str().unwrap(),
        "queue",
        "reset-retries",
        "--identity",
        "/docs/missing.md",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no error task"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// The `entity:link` row is visible in `queue status` (identity = the
/// document id, with status/attempts/last_error), and
/// `queue reset-retries --identity <doc_id>` re-queues it.
#[test]
fn queue_status_shows_entity_link_tasks_and_reset_requeues_them() {
    let f = entity_link_fixture("entity-link");
    let out = run(&["--config", f.cfg.to_str().unwrap(), "queue", "status"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("entity:link"), "{stdout:?}");
    assert!(stdout.contains("3 tasks"), "{stdout:?}");
    let fields = row_fields(&stdout, "42");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["entity:link", "42", "error", "3"],
        "{fields:?}"
    );
    assert!(
        fields.iter().any(|cell| cell.contains("linker")),
        "last_error column: {fields:?}"
    );

    // `queue reset-retries --identity <doc_id>` resets the entity:link task.
    let out = run(&[
        "--config",
        f.cfg.to_str().unwrap(),
        "queue",
        "reset-retries",
        "--identity",
        "42",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 task(s) re-queued"), "{stdout:?}");

    // Verified by a subsequent `queue status`.
    let out = run(&["--config", f.cfg.to_str().unwrap(), "queue", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fields = row_fields(&stdout, "42");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["entity:link", "42", "pending", "0"],
        "42 must be re-queued: {fields:?}"
    );
    // The other rows are untouched.
    let fields = row_fields(&stdout, "/docs/a.md");
    assert_eq!(
        fields.iter().take(4).copied().collect::<Vec<_>>(),
        vec!["doc:index", "/docs/a.md", "error", "3"],
        "a.md must stay error: {fields:?}"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}
