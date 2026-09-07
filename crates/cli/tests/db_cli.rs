//! Binary-level tests for the `db` subcommand (remove-direct-ingest task
//! 1.1): `db stats` prints the dataset counts and changes nothing;
//! `db clear` prints the same stats, then deletes the dataset's entire
//! state directory on a piped `y` and aborts (state directory + DB
//! unchanged) on a piped `n` or no answer.

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use db::{
    ChunkDao, ConnectionOrTx, Db, DocumentDao, EntityDao, EntityLink, EntityLinkDao, FactDao,
    QueueTaskDao, QueueTaskType,
};

fn synopsis() -> Command {
    Command::new(env!("CARGO_BIN_EXE_synopsis"))
}

fn run(args: &[&str]) -> Output {
    synopsis().args(args).output().unwrap()
}

/// Runs the binary with `stdin_text` piped to the process's stdin (the
/// `db clear` confirmation answer); stdin is closed before waiting.
fn run_stdin(args: &[&str], stdin_text: &str) -> Output {
    let mut child = synopsis()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    child.stdin.take(); // close the pipe: a missing answer is EOF
    child.wait_with_output().unwrap()
}

/// A temp-dir fixture: the config (paths + dataset `edtech`), the migrated
/// knowledge database at the derived dataset path, and the seeded rows:
/// 1 document, 2 chunks, 3 entities, 1 entity link, 1 fact, 1 queue job,
/// and one row in each join table.
struct Fixture {
    dir: PathBuf,
    cfg: PathBuf,
}

impl Fixture {
    /// The dataset state directory
    /// (`<workspace>/datasets/edtech/state`).
    fn state_dir(&self) -> PathBuf {
        self.dir
            .join("workspace")
            .join("datasets")
            .join("edtech")
            .join("state")
    }
}

fn fixture(tag: &str) -> Fixture {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "synopsis-cli-db-bin-{tag}-{}-{ns}",
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
    seed(&db);
    drop(db);

    Fixture { dir, cfg }
}

/// Seeds the fixture rows (see [`fixture`]).
fn seed(db: &Db) {
    db.with_conn(|conn| -> Result<(), db::DbError> {
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
        let fact =
            FactDao::new(exec).create(Some(e1), "works_at", Some(e2), "hr", None, None, None)?;
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
                QueueTaskType::DocIndex,
                "/docs/a.md",
                &db::DocIndexPayload {
                    source_path: "/docs".to_owned(),
                    content_hash: None,
                    ops: vec![db::ReIndexOp::Full],
                },
                0,
            )
            .expect("enqueue task");
        Ok(())
    })
    .expect("with_conn")
    .expect("seed rows");
}

/// The `TABLE` row count (direct SQL: join tables included).
fn table_count(dir: &Path, table: &str) -> i64 {
    let db = Db::open_knowledge(
        dir.join("workspace")
            .join("datasets")
            .join("edtech")
            .join("state")
            .join("db")
            .join("knowledge.db"),
    )
    .expect("reopen knowledge db");
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count = db
        .with_conn(|conn| conn.query_row(&sql, [], |r| r.get(0)))
        .expect("with_conn")
        .expect("count");
    drop(db);
    count
}

/// The seeded baseline (the pre-clear counts).
fn assert_seeded(dir: &Path) {
    assert_eq!(table_count(dir, "documents"), 1);
    assert_eq!(table_count(dir, "chunks"), 2);
    assert_eq!(table_count(dir, "entities"), 3);
    assert_eq!(table_count(dir, "entity_links"), 1);
    assert_eq!(table_count(dir, "facts"), 1);
    assert_eq!(table_count(dir, "queue_tasks"), 1);
    assert_eq!(table_count(dir, "chunk_entities"), 1);
    assert_eq!(table_count(dir, "fact_sources"), 1);
    assert_eq!(table_count(dir, "entity_sources"), 1);
}

#[test]
fn db_stats_prints_counts_and_changes_nothing() {
    let f = fixture("stats");
    let out = run(&["--config", f.cfg.to_str().unwrap(), "db", "stats"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Dataset Statistics:"), "{stdout:?}");
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
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_clear_piped_y_deletes_state_dir() {
    let f = fixture("clear-y");
    let state_dir = f.state_dir();
    let out = run_stdin(&["--config", f.cfg.to_str().unwrap(), "db", "clear"], "y\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The stats block is printed before the prompt (spec scenario).
    assert!(stdout.contains("Documents:    1"), "{stdout:?}");
    assert!(stdout.contains("Confirm deletion? [y/N]"), "{stdout:?}");
    assert!(stdout.contains("cleared dataset state at"), "{stdout:?}");
    assert!(!state_dir.exists(), "the state directory must be gone");
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_clear_piped_n_aborts_unchanged() {
    let f = fixture("clear-n");
    let state_dir = f.state_dir();
    let out = run_stdin(&["--config", f.cfg.to_str().unwrap(), "db", "clear"], "n\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Confirm deletion? [y/N]"), "{stdout:?}");
    assert!(stdout.contains("aborted"), "{stdout:?}");
    assert!(
        !stdout.contains("cleared dataset state"),
        "no clear must be reported: {stdout:?}"
    );
    assert!(state_dir.exists(), "the state directory must remain");
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_clear_without_answer_aborts_unchanged() {
    // `output()` closes stdin immediately: EOF is not a confirmation.
    let f = fixture("clear-eof");
    let out = run(&["--config", f.cfg.to_str().unwrap(), "db", "clear"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("aborted"), "{stdout:?}");
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}
