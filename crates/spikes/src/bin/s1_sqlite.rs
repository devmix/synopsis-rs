//! S1 spike: SQLite/FTS5 seam via source-compiled bundled SQLite (change
//! native-seam-spikes task 1.1; design D3 + D6).
//!
//! Proves that Rust owns the full FTS5 path with zero CGO flags:
//!   1. A fresh temporary database is initialized by `rusqlite_migration` from a single
//!      squashed v5 init migration (`migrations/1-init/up.sql`, embedded at compile time);
//!      afterwards `PRAGMA user_version` must be exactly 1 — the sole source of truth about
//!      schema state (no `_schema_migrations` table is created, design D6).
//!   2. The fresh DB's product schema matches fixtures/knowledge.db (Oracle v5 ground truth,
//!      opened read-only + immutable) across all tables/columns/indexes/triggers; Go artifacts
//!      are excluded by an explicit list.
//!   3. The PRAGMAs from ../synopsis/configs/config.default.yaml apply without error on the
//!      bundled SQLite and `journal_mode` reads back as WAL.
//!   4. Chunk texts copied read-only from the fixture are inserted into the fresh DB (FTS5
//!      content is maintained by the sync triggers fixed in the init DDL); four fixed bm25
//!      MATCH queries must reproduce the recorded Oracle expectations: exact hit counts,
//!      exact top-k chunk ids and order, scores within a 1e-3 relative tolerance.
//!
//! Usage (from repo root): `cargo run -p spikes --bin s1_sqlite [fixture-path]`
//! Prints one PASS line per check and exits 0 on success; prints FAIL with the reason and
//! exits 1 on the first failed check.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};
use rusqlite::{Connection, OpenFlags, params};
use rusqlite_migration::Migrations;

/// Migration directory embedded at compile time (design D6: `from-directory` + include_dir).
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// Go artifacts excluded from the fresh DB and from the schema diff (explicit list, task 1.1):
/// `_schema_migrations` (Go migration tracking; replaced by PRAGMA user_version) and the
/// legacy SQLite-vec0 store `chunks_vec` with its shadow tables (384-dim, never read or migrated).
const GO_ARTIFACTS: &[&str] = &[
    "_schema_migrations",
    "chunks_vec",
    "chunks_vec_info",
    "chunks_vec_chunks",
    "chunks_vec_rowids",
    "chunks_vec_vector_chunks00",
];

/// PRAGMAs from ../synopsis/configs/config.default.yaml (`database.pragma`), in file order.
const CONFIG_PRAGMAS: &[(&str, &str)] = &[
    ("mmap_size", "268435456"), // 256 MB
    ("journal_mode", "WAL"),
    ("synchronous", "NORMAL"),
    ("cache_size", "-64000"), // negative value: KiB (64 MB) in WAL mode
];

/// One recorded Oracle expectation for the bm25 parity queries.
struct Expected {
    /// Human-readable query label for PASS/FAIL lines.
    label: &'static str,
    /// FTS5 MATCH expression to run.
    expr: &'static str,
    /// Expected number of matching chunks (must match exactly).
    hits: usize,
    /// Expected top rows in rank order: exact chunk ids + expected bm25 scores
    /// (scores compared with a 1e-3 relative tolerance; an empty slice means the hit
    /// count alone is the recorded expectation).
    top: &'static [(i64, f64)],
}

/// Recorded Oracle expectations from task 1.1 (verified against fixtures/knowledge.db).
const EXPECTATIONS: &[Expected] = &[
    Expected {
        label: "term 'knowledge'",
        expr: "knowledge",
        hits: 17,
        top: &[(247, -4.4453), (30, -3.573), (106, -3.4522)],
    },
    Expected {
        label: "term 'RAG'",
        expr: "RAG",
        hits: 1,
        top: &[],
    },
    Expected {
        label: "phrase '\"knowledge graph\"'",
        expr: "\"knowledge graph\"",
        hits: 1,
        top: &[],
    },
    Expected {
        // Corrected by human decision (task 1.1 revision 4): the originally recorded
        // "3 hits" was a LIMIT 3 measurement artifact; FTS5 OR semantics give the union
        // of 17 (`knowledge`) + 1 (`RAG`) with no overlap -> exactly 18 hits.
        label: "operators 'knowledge OR RAG'",
        expr: "knowledge OR RAG",
        hits: 18,
        top: &[],
    },
];

/// Relative tolerance for bm25 score comparison (acceptance criterion of task 1.1).
const SCORE_TOL: f64 = 1e-3;

/// One `PRAGMA table_info` row: (name, declared type, notnull flag, default value, pk flag).
#[derive(Debug, Clone, PartialEq)]
struct Col {
    name: String,
    ty: String,
    notnull: i64,
    dflt: Option<String>,
    pk: i64,
}

/// Product-schema snapshot of one database (Go artifacts and sqlite_% internals excluded).
#[derive(Default)]
struct Schema {
    /// table name -> ordered column list from PRAGMA table_info.
    tables: BTreeMap<String, Vec<Col>>,
    /// index name -> (unique flag, origin, ordered indexed columns) from
    /// PRAGMA index_list / PRAGMA index_info.
    indexes: BTreeMap<String, (bool, String, Vec<String>)>,
    /// trigger names.
    triggers: BTreeSet<String>,
}

/// A chunk row copied from the fixture.
struct ChunkRow {
    id: i64,
    doc_id: i64,
    chunk_text: String,
    sequence_num: i64,
    start_offset: Option<i64>,
    end_offset: Option<i64>,
    created_at: Option<String>,
}

/// Deletes the temporary database files on drop.
struct TempDb {
    paths: Vec<PathBuf>,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = fs::remove_file(p);
        }
    }
}

/// Quote an identifier for use inside PRAGMA statements.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("s1_sqlite: FAIL — {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let fixture_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "fixtures/knowledge.db".to_string());

    // Fixture is dev-time ground truth only: read-only + immutable, never mutated, no sidecars.
    let uri = format!("file:{fixture_path}?mode=ro&immutable=1");
    let fixture = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open fixture {fixture_path}: {e}"))?;

    // Fresh temporary database (task 1.1: Rust always builds its DB from scratch).
    let stem = format!("s1-spike-{}", std::process::id());
    let db = env::temp_dir().join(format!("{stem}.db"));
    let wal_path = env::temp_dir().join(format!("{stem}.db-wal"));
    let shm_path = env::temp_dir().join(format!("{stem}.db-shm"));
    for p in [&db, &wal_path, &shm_path] {
        let _ = fs::remove_file(p); // idempotent: drop stale files from a previous run
    }

    let mut conn =
        Connection::open(&db).map_err(|e| format!("open fresh DB {}: {e}", db.display()))?;
    let temp_db = TempDb {
        paths: vec![db.clone(), wal_path, shm_path],
    };

    // SQLite build provenance (bundled source-compiled, design D3) — informational.
    let (sqlite_version, sqlite_source_id): (String, String) = conn
        .query_row(
            "SELECT sqlite_version(), COALESCE(sqlite_source_id(), 'n/a')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("read sqlite version: {e}"))?;
    println!("info: bundled SQLite {sqlite_version} (source id {sqlite_source_id})");

    // -- Check 1: init migration on the fresh DB + PRAGMA user_version == 1 (design D6).
    let migrations = Migrations::from_directory(&MIGRATIONS)
        .map_err(|e| format!("load embedded migrations from directory: {e}"))?;
    migrations
        .to_latest(&mut conn)
        .map_err(|e| format!("apply init migration to fresh DB: {e}"))?;
    // rusqlite_migration turns PRAGMA foreign_keys ON while migrating and leaves the setting
    // to the caller. The spike copies chunk rows without their parent documents rows (task
    // scope), so FK enforcement is switched off for this connection explicitly.
    conn.pragma_update(None, "foreign_keys", "OFF")
        .map_err(|e| format!("set PRAGMA foreign_keys=OFF after migration: {e}"))?;
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("read PRAGMA user_version: {e}"))?;
    if user_version != 1 {
        return Err(format!(
            "user_version check: expected PRAGMA user_version == 1 after `1-init`, got {user_version}"
        ));
    }
    println!(
        "PASS user_version: PRAGMA user_version == 1 (single squashed v5 init migration applied, no _schema_migrations table)"
    );

    // -- Check 2: product-schema diff fresh vs fixture.
    let fresh_schema = snapshot(&conn).map_err(|e| format!("snapshot fresh schema: {e}"))?;
    let fixture_schema = snapshot(&fixture).map_err(|e| format!("snapshot fixture schema: {e}"))?;
    let mismatches = diff_schemas(&fresh_schema, &fixture_schema);
    if !mismatches.is_empty() {
        return Err(format!("schema check: {}", mismatches.join("; ")));
    }
    println!(
        "PASS schema: {} tables ({} columns total), {} indexes, {} triggers identical to fixture \
         (Go artifacts excluded by explicit list: _schema_migrations, chunks_vec*)",
        fresh_schema.tables.len(),
        fresh_schema.tables.values().map(|c| c.len()).sum::<usize>(),
        fresh_schema.indexes.len(),
        fresh_schema.triggers.len()
    );

    // -- Check 3: PRAGMAs from configs/config.default.yaml on bundled SQLite.
    for (name, value) in CONFIG_PRAGMAS {
        conn.execute_batch(&format!("PRAGMA {name} = {value};"))
            .map_err(|e| format!("apply PRAGMA {name} = {value}: {e}"))?;
    }
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(|e| format!("read back journal_mode: {e}"))?;
    if journal_mode != "wal" {
        return Err(format!(
            "pragma check: PRAGMA journal_mode expected 'wal', got '{journal_mode}'"
        ));
    }
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .map_err(|e| format!("read back synchronous: {e}"))?;
    let cache_size: i64 = conn
        .query_row("PRAGMA cache_size", [], |r| r.get(0))
        .map_err(|e| format!("read back cache_size: {e}"))?;
    let mmap_size: i64 = conn
        .query_row("PRAGMA mmap_size", [], |r| r.get(0))
        .map_err(|e| format!("read back mmap_size: {e}"))?;
    if synchronous != 1 || cache_size != -64_000 || mmap_size <= 0 {
        return Err(format!(
            "pragma check: read-back mismatch (synchronous={synchronous}, cache_size={cache_size}, \
             mmap_size={mmap_size}); expected NORMAL(1), -64000, >0"
        ));
    }
    println!(
        "PASS pragma: journal_mode=wal, synchronous=NORMAL({synchronous}), cache_size={cache_size} KiB, \
         mmap_size={mmap_size} — config.default.yaml values applied without error on bundled SQLite"
    );

    // -- Check 4: data parity via FTS5 bm25 (fixture chunk texts -> fresh DB).
    let chunk_count = load_chunks(&mut conn, &fixture)?;
    println!(
        "PASS fts-content: {chunk_count} chunk rows inserted, FTS5 index populated by sync triggers"
    );

    for exp in EXPECTATIONS {
        check_bm25(&conn, exp)?;
    }

    println!(
        "s1_sqlite: OK — all checks passed (fresh v5 DB from rusqlite_migration + bundled SQLite)"
    );
    drop(temp_db);
    Ok(())
}

/// Snapshot the product schema of a database: tables with column lists, indexes with
/// their uniqueness/origin/columns, trigger names. sqlite_% internals and GO_ARTIFACTS excluded.
fn is_go_artifact(name: &str) -> bool {
    GO_ARTIFACTS.contains(&name)
}

fn snapshot(conn: &Connection) -> Result<Schema, String> {
    let mut schema = Schema::default();

    let mut stmt = conn
        .prepare("SELECT type, name FROM sqlite_master WHERE type IN ('table', 'index', 'trigger') AND name NOT LIKE 'sqlite_%'")
        .map_err(|e| format!("list sqlite_master objects: {e}"))?;
    let objects = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("iterate sqlite_master: {e}"))?;
    for obj in objects {
        let (ty, name) = obj.map_err(|e| format!("read object row: {e}"))?;
        if is_go_artifact(&name) {
            continue; // explicit Go-artifact exclusion list
        }
        match ty.as_str() {
            "table" => {
                let mut cols = Vec::new();
                let tbl_ident = ident(&name);
                let mut cstmt = conn
                    .prepare(&format!("PRAGMA table_info({tbl_ident})"))
                    .map_err(|e| format!("PRAGMA table_info({name}): {e}"))?;
                for row in cstmt
                    .query_map([], |r| {
                        Ok(Col {
                            name: r.get(1)?,
                            ty: r.get(2)?,
                            notnull: r.get(3)?,
                            dflt: r.get(4)?,
                            pk: r.get(5)?,
                        })
                    })
                    .map_err(|e| format!("iterate table_info({name}): {e}"))?
                {
                    cols.push(row.map_err(|e| format!("read column of {name}: {e}"))?);
                }
                schema.tables.insert(name.clone(), cols);

                // All indexes of the table (named + sqlite_autoindex_*), collected via
                // PRAGMA index_list(table) and per-index PRAGMA index_info.
                let mut istmt = conn
                    .prepare(&format!("PRAGMA index_list({tbl_ident})"))
                    .map_err(|e| format!("PRAGMA index_list({name}): {e}"))?;
                for row in istmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(1)?,
                            r.get::<_, bool>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| format!("iterate index_list({name}): {e}"))?
                {
                    let (idx_name, unique, origin) =
                        row.map_err(|e| format!("read index of {name}: {e}"))?;
                    let mut columns = Vec::new();
                    let idx_ident = ident(&idx_name);
                    let mut cstmt = conn
                        .prepare(&format!("PRAGMA index_info({idx_ident})"))
                        .map_err(|e| format!("PRAGMA index_info({idx_name}): {e}"))?;
                    for crow in cstmt
                        .query_map([], |r| r.get::<_, Option<String>>(2))
                        .map_err(|e| format!("iterate index_info({idx_name}): {e}"))?
                    {
                        columns.push(
                            crow.map_err(|e| format!("read column of {idx_name}: {e}"))?
                                .unwrap_or_default(),
                        );
                    }
                    schema.indexes.insert(idx_name, (unique, origin, columns));
                }
            }
            // Index objects are recorded under their table above; nothing to do here.
            "index" => {}
            "trigger" => {
                schema.triggers.insert(name);
            }
            other => return Err(format!("unexpected object type '{other}' for {name}")),
        }
    }
    Ok(schema)
}

/// Structural diff of two schema snapshots; returns human-readable mismatches (empty = equal).
fn diff_schemas(fresh: &Schema, fixture: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    for name in fresh.tables.keys().chain(fixture.tables.keys()) {
        match (fresh.tables.get(name), fixture.tables.get(name)) {
            (Some(a), Some(b)) if a != b => out.push(format!(
                "table '{name}' columns differ: fresh={a:?} fixture={b:?}"
            )),
            // Present on both sides with identical column lists, or (impossible for union keys)
            // absent from both.
            (Some(_), Some(_)) | (None, None) => {}
            _ => out.push(format!(
                "table '{name}' present on one side only (fresh={} fixture={})",
                fresh.tables.contains_key(name),
                fixture.tables.contains_key(name)
            )),
        }
    }
    for name in fresh.indexes.keys().chain(fixture.indexes.keys()) {
        match (fresh.indexes.get(name), fixture.indexes.get(name)) {
            (Some(a), Some(b)) if a != b => {
                out.push(format!("index '{name}' differs: fresh={a:?} fixture={b:?}"))
            }
            (Some(_), Some(_)) | (None, None) => {}
            _ => out.push(format!(
                "index '{name}' present on one side only (fresh={} fixture={})",
                fresh.indexes.contains_key(name),
                fixture.indexes.contains_key(name)
            )),
        }
    }
    for name in fresh.triggers.symmetric_difference(&fixture.triggers) {
        out.push(format!("trigger '{name}' present on one side only"));
    }
    out
}

/// Copy all chunk rows from the fixture (read-only) into the fresh DB inside one transaction.
/// The FTS5 external-content index is filled by the sync triggers fixed in the init DDL.
fn load_chunks(conn: &mut Connection, fixture: &Connection) -> Result<usize, String> {
    let mut fstmt = fixture
        .prepare("SELECT id, doc_id, chunk_text, sequence_num, start_offset, end_offset, created_at FROM chunks ORDER BY id")
        .map_err(|e| format!("read fixture chunks: {e}"))?;
    let rows = fstmt
        .query_map([], |r| {
            Ok(ChunkRow {
                id: r.get(0)?,
                doc_id: r.get(1)?,
                chunk_text: r.get(2)?,
                sequence_num: r.get(3)?,
                start_offset: r.get(4)?,
                end_offset: r.get(5)?,
                created_at: r.get(6)?,
            })
        })
        .map_err(|e| format!("iterate fixture chunks: {e}"))?;
    let mut chunks = Vec::new();
    for row in rows {
        chunks.push(row.map_err(|e| format!("read fixture chunk row: {e}"))?);
    }

    let tx = conn
        .transaction()
        .map_err(|e| format!("begin insert transaction: {e}"))?;
    {
        let mut ins = tx
            .prepare("INSERT INTO chunks (id, doc_id, chunk_text, sequence_num, start_offset, end_offset, created_at) \
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
            .map_err(|e| format!("prepare chunk insert: {e}"))?;
        for c in &chunks {
            ins.execute(params![
                c.id,
                c.doc_id,
                c.chunk_text,
                c.sequence_num,
                c.start_offset,
                c.end_offset,
                c.created_at
            ])
            .map_err(|e| format!("insert chunk {}: {e}", c.id))?;
        }
    } // `ins` dropped here so the transaction can be committed by value.
    tx.commit()
        .map_err(|e| format!("commit insert transaction: {e}"))?;

    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
        .map_err(|e| format!("count FTS5 content rows: {e}"))?;
    if (fts_rows as usize) != chunks.len() {
        return Err(format!(
            "FTS5 content rows ({fts_rows}) do not match inserted chunk count ({})",
            chunks.len()
        ));
    }
    Ok(chunks.len())
}

/// Run one recorded bm25 expectation against the fresh DB and report PASS/FAIL.
fn check_bm25(conn: &Connection, exp: &Expected) -> Result<(), String> {
    let sql = "SELECT c.id AS id, bm25(chunks_fts) AS score \
               FROM chunks_fts JOIN chunks c ON c.rowid = chunks_fts.rowid \
               WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts)";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare bm25 query: {e}"))?;
    let rows = stmt
        .query_map(params![exp.expr], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })
        .map_err(|e| format!("run bm25 MATCH '{}': {e}", exp.expr))?;
    let mut hits: Vec<(i64, f64)> = Vec::new();
    for row in rows {
        hits.push(row.map_err(|e| format!("read bm25 row: {e}"))?);
    }

    if hits.len() != exp.hits {
        return Err(format!(
            "bm25 check [{}]: expected {} hits, got {}",
            exp.label,
            exp.hits,
            hits.len()
        ));
    }
    for (i, (exp_id, exp_score)) in exp.top.iter().enumerate() {
        let Some((got_id, got_score)) = hits.get(i) else {
            return Err(format!(
                "bm25 check [{}]: missing top-{} row",
                exp.label,
                i + 1
            ));
        };
        if got_id != exp_id {
            return Err(format!(
                "bm25 check [{}]: top-{} chunk id mismatch: expected {exp_id}, got {got_id}",
                exp.label,
                i + 1
            ));
        }
        let delta = (got_score - exp_score).abs();
        if !got_score.is_finite() || delta > (*exp_score).abs() * SCORE_TOL {
            return Err(format!(
                "bm25 check [{}]: top-{} score mismatch: expected ≈ {exp_score}, got {got_score} \
                 (delta {delta} exceeds 1e-3 relative — record as NO-GO risk in ADR 0001)",
                exp.label,
                i + 1
            ));
        }
    }

    let top_desc: Vec<String> = hits
        .iter()
        .take(3)
        .map(|(id, s)| format!("({id}, {s:.4})"))
        .collect();
    println!(
        "PASS bm25 [{}]: {} hit(s), top-3 = {}",
        exp.label,
        hits.len(),
        if top_desc.is_empty() {
            "-".to_string()
        } else {
            top_desc.join(", ")
        }
    );
    Ok(())
}
