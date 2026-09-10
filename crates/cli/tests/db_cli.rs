//! Binary-level tests for the `db` subcommand (remove-direct-ingest task
//! 1.1; `merge-entities` added by multilingual-entity-resolution task 6.1):
//! `db stats` prints the dataset counts and changes nothing; `db clear`
//! prints the same stats, then deletes the dataset's entire state directory
//! on a piped `y` and aborts (state directory + DB unchanged) on a piped `n`
//! or no answer; `db merge-entities <id> --into <id>` merges one entity into
//! another on a piped `y` (duplicate row gone, both names aliased) and aborts
//! (DB unchanged) on a piped `n`, a nonexistent id, or a type / domain
//! mismatch.

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use db::{
    ChunkDao, ConnectionOrTx, Db, DocumentDao, EntityAliasDao, EntityDao, EntityLink,
    EntityLinkDao, FactDao, QueueTaskDao, QueueTaskType,
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

/// The dataset knowledge database path for a fixture directory.
fn db_path(dir: &Path) -> PathBuf {
    dir.join("workspace")
        .join("datasets")
        .join("edtech")
        .join("state")
        .join("db")
        .join("knowledge.db")
}

/// Opens the fixture knowledge database (a fresh handle).
fn open_db(dir: &Path) -> Db {
    Db::open_knowledge(db_path(dir)).expect("open knowledge db")
}

/// The entity id of the row with the given (type, name, domain); panics if
/// absent.
fn entity_id(dir: &Path, etype: &str, name: &str, domain: &str) -> i64 {
    let db = open_db(dir);
    let id = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT id FROM entities WHERE type = ?1 AND name = ?2 AND domain = ?3",
                rusqlite::params![etype, name, domain],
                |r| r.get(0),
            )
        })
        .expect("with_conn")
        .expect("entity exists");
    drop(db);
    id
}

/// Whether an entity with the given (type, name, domain) exists.
fn entity_exists(dir: &Path, etype: &str, name: &str, domain: &str) -> bool {
    let db = open_db(dir);
    let found = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM entities WHERE type = ?1 AND name = ?2 AND domain = ?3",
                rusqlite::params![etype, name, domain],
                |_| Ok(()),
            )
        })
        .expect("with_conn")
        .is_ok();
    drop(db);
    found
}

/// Adds an entity to the fixture DB (the domain-mismatch merge test); returns
/// its id.
fn add_entity(dir: &Path, etype: &str, name: &str, domain: &str) -> i64 {
    let db = open_db(dir);
    let id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(etype, name, domain, None, None, None)
        })
        .expect("with_conn")
        .expect("create entity");
    drop(db);
    id
}

/// The aliases recorded for an entity (ordered by alias).
fn aliases_of(dir: &Path, entity_id: i64) -> Vec<String> {
    let db = open_db(dir);
    let aliases = db
        .with_conn(|conn| {
            EntityAliasDao::new(ConnectionOrTx::Connection(conn)).aliases_of(entity_id)
        })
        .expect("with_conn")
        .expect("aliases");
    drop(db);
    aliases
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

// --- db merge-entities (multilingual-entity-resolution task 6.1) -------------

/// The `db merge-entities <from> --into <into>` argument list.
fn merge_args(f: &Fixture, from: i64, into: i64) -> Vec<String> {
    vec![
        "--config".to_string(),
        f.cfg.to_str().unwrap().to_string(),
        "db".to_string(),
        "merge-entities".to_string(),
        from.to_string(),
        "--into".to_string(),
        into.to_string(),
    ]
}

/// Runs `db merge-entities` with the given ids and stdin text.
fn run_merge(f: &Fixture, from: i64, into: i64, stdin_text: &str) -> Output {
    let owned = merge_args(f, from, into);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    run_stdin(&args, stdin_text)
}

#[test]
fn db_help_lists_merge_entities() {
    let out = run(&["db", "--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("merge-entities"),
        "db --help lists the action: {stdout:?}"
    );
}

#[test]
fn db_merge_entities_help_shows_into_flag() {
    let out = run(&["db", "merge-entities", "--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--into"), "shows --into: {stdout:?}");
    assert!(stdout.contains("ID"), "shows the ID positional: {stdout:?}");
}

#[test]
fn db_merge_entities_piped_y_merges_and_records_aliases() {
    let f = fixture("merge-y");
    let survivor = entity_id(&f.dir, "system", "CRM", "hr");
    let dup = entity_id(&f.dir, "system", "ERP", "hr");
    let out = run_merge(&f, dup, survivor, "y\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The stats block, then the prompt, then the summary.
    assert!(stdout.contains("Dataset Statistics:"), "{stdout:?}");
    assert!(stdout.contains("Confirm merge? [y/N]"), "{stdout:?}");
    assert!(stdout.contains("Merge Summary:"), "{stdout:?}");
    assert!(stdout.contains("CRM"), "surviving name: {stdout:?}");
    assert!(stdout.contains("ERP"), "merged name: {stdout:?}");
    // The duplicate row is gone; the survivor remains.
    assert_eq!(table_count(&f.dir, "entities"), 2, "one entity deleted");
    assert!(
        entity_exists(&f.dir, "system", "CRM", "hr"),
        "survivor remains"
    );
    assert!(
        !entity_exists(&f.dir, "system", "ERP", "hr"),
        "duplicate gone"
    );
    // Both names are recorded as aliases of the survivor (ordered).
    let aliases = aliases_of(&f.dir, survivor);
    assert_eq!(
        aliases,
        vec!["CRM".to_string(), "ERP".to_string()],
        "both names aliased: {aliases:?}"
    );
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_uppercase_y_merges() {
    let f = fixture("merge-Y");
    let survivor = entity_id(&f.dir, "system", "CRM", "hr");
    let dup = entity_id(&f.dir, "system", "ERP", "hr");
    let out = run_merge(&f, dup, survivor, "Y\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(table_count(&f.dir, "entities"), 2, "one entity deleted");
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_piped_n_aborts_unchanged() {
    let f = fixture("merge-n");
    let survivor = entity_id(&f.dir, "system", "CRM", "hr");
    let dup = entity_id(&f.dir, "system", "ERP", "hr");
    let out = run_merge(&f, dup, survivor, "n\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an aborted merge exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Confirm merge? [y/N]"), "{stdout:?}");
    assert!(stdout.contains("aborted"), "{stdout:?}");
    assert!(
        !stdout.contains("Merge Summary"),
        "no merge must be reported: {stdout:?}"
    );
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_without_answer_aborts_unchanged() {
    // `output()` closes stdin immediately: EOF is not a confirmation.
    let f = fixture("merge-eof");
    let survivor = entity_id(&f.dir, "system", "CRM", "hr");
    let dup = entity_id(&f.dir, "system", "ERP", "hr");
    let args = merge_args(&f, dup, survivor);
    let out = synopsis().args(&args).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "an aborted merge exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("aborted"), "{stdout:?}");
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_nonexistent_id_errors_unchanged() {
    let f = fixture("merge-badid");
    let survivor = entity_id(&f.dir, "system", "CRM", "hr");
    // 999999 does not exist.
    let out = run_merge(&f, 999999, survivor, "y\n");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a nonexistent id exits 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("does not exist"), "clear error: {stderr:?}");
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_different_type_errors_unchanged() {
    let f = fixture("merge-type");
    let crm = entity_id(&f.dir, "system", "CRM", "hr");
    let alice = entity_id(&f.dir, "person", "Alice", "hr");
    // CRM (system) and Alice (person) differ in type.
    let out = run_merge(&f, alice, crm, "y\n");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a type mismatch exits 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("type mismatch"), "clear error: {stderr:?}");
    assert_seeded(&f.dir);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn db_merge_entities_different_domain_errors_unchanged() {
    let f = fixture("merge-domain");
    let crm_hr = entity_id(&f.dir, "system", "CRM", "hr");
    // A same-type, different-domain entity (added to the fixture DB).
    let crm_finance = add_entity(&f.dir, "system", "CRM", "finance");
    let out = run_merge(&f, crm_finance, crm_hr, "y\n");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a domain mismatch exits 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("domain mismatch"),
        "clear error: {stderr:?}"
    );
    // No modification: the merge did not run (the added entity remains).
    assert_eq!(table_count(&f.dir, "entities"), 4, "no entity deleted");
    let _ = std::fs::remove_dir_all(&f.dir);
}
