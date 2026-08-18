//! One-shot provenance probe for the knowledge.db fixture (change
//! native-seam-spikes, task 0.1). Confirms that rusqlite with the source-compiled
//! SQLite build opens the real Oracle v5 database — with its legacy vec0 shadow
//! tables still present — and that FTS5 bm25 queries return content over it.
//!
//! Usage (from the repo root):
//!   cargo run -p spikes --bin probe_db            # defaults to fixtures/knowledge.db
//!   cargo run -p spikes --bin probe_db <path>     # or an explicit path

use std::collections::BTreeSet;
use std::env;

use rusqlite::{Connection, OpenFlags};

/// Tables every v5 knowledge.db must contain (../synopsis/migrations 001–005).
const EXPECTED_TABLES: &[&str] = &[
    "_schema_migrations",
    "app_kv",
    "chunk_entities",
    "chunks",
    // FTS5 external-content table plus its shadow family (migration 002).
    "chunks_fts",
    "chunks_fts_config",
    "chunks_fts_data",
    "chunks_fts_docsize",
    "chunks_fts_idx",
    // Legacy SQLite-vec0 store + shadow tables: must be ignored without error.
    "chunks_vec",
    "chunks_vec_chunks",
    "chunks_vec_info",
    "chunks_vec_rowids",
    "chunks_vec_vector_chunks00",
    "documents",
    "entities",
    "entity_links",
    "entity_sources",
    "fact_sources",
    "facts",
];

/// User tables whose row counts the probe prints (and sanity-checks > 0).
const COUNTED_TABLES: &[&str] = &[
    "app_kv",
    "chunk_entities",
    "chunks",
    "documents",
    "entities",
    "entity_links",
    "entity_sources",
    "fact_sources",
    "facts",
];

/// Fixed bm25 probe query: FTS5 MATCH joined back to chunks, ordered by rank.
const BM25_PROBE_QUERY: &str = "SELECT c.id, round(bm25(chunks_fts), 4) AS score \
     FROM chunks_fts f JOIN chunks c ON c.rowid = f.rowid \
     WHERE chunks_fts MATCH 'knowledge' ORDER BY bm25(chunks_fts) LIMIT 3";

fn main() -> Result<(), String> {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => "fixtures/knowledge.db".to_string(),
    };

    // Read-only + immutable: the probe must never mutate the fixture or spawn
    // -wal/-shm sidecars. `immutable=1` is safe here — a single local process,
    // and the copy has no WAL sidecar of its own (see fixtures/README.md).
    let uri = format!("file:{path}?mode=ro&immutable=1");
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open {path}: {e}"))?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("integrity_check: {e}"))?;
    if integrity != "ok" {
        return Err(format!("PRAGMA integrity_check failed: {integrity}"));
    }
    println!("integrity_check: ok");

    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|e| format!("list tables: {e}"))?;
    for row in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("iterate tables: {e}"))?
    {
        found.insert(row.map_err(|e| format!("read table name: {e}"))?);
    }
    let missing: Vec<&str> = EXPECTED_TABLES
        .iter()
        .copied()
        .filter(|t| !found.contains(*t))
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing expected tables: {missing:?}"));
    }
    println!(
        "tables: {} found, all {} expected present (incl. FTS5 shadow + legacy vec0 families)",
        found.len(),
        EXPECTED_TABLES.len()
    );

    let mut applied: Vec<i64> = Vec::new();
    let mut mstmt = conn
        .prepare("SELECT version FROM _schema_migrations")
        .map_err(|e| format!("read migrations: {e}"))?;
    for row in mstmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("iterate migrations: {e}"))?
    {
        applied.push(row.map_err(|e| format!("read migration version: {e}"))?);
    }
    for v in 1..=5i64 {
        if !applied.contains(&v) {
            return Err(format!(
                "migration {v} not recorded in _schema_migrations (have {applied:?})"
            ));
        }
    }
    println!("migrations: 1..=5 all applied");

    let mut counts = Vec::new();
    for t in COUNTED_TABLES {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))
            .map_err(|e| format!("count {t}: {e}"))?;
        counts.push((*t, n));
    }
    for (t, n) in &counts {
        println!("rows: {t} = {n}");
    }
    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
        .map_err(|e| format!("count chunks_fts: {e}"))?;
    println!("rows: chunks_fts (FTS5 content) = {fts_rows}");

    let chunks_rows = counts
        .iter()
        .find_map(|(t, n)| (*t == "chunks").then_some(*n))
        .unwrap_or(0);
    for required in ["chunks", "documents", "facts"] {
        if !counts.iter().any(|(t, n)| *t == required && *n > 0) {
            return Err(format!("table '{required}' must be non-empty"));
        }
    }
    if chunks_rows != fts_rows {
        return Err(format!(
            "FTS5 content rows ({fts_rows}) do not match chunk count ({chunks_rows})"
        ));
    }

    let mut bstmt = conn
        .prepare(BM25_PROBE_QUERY)
        .map_err(|e| format!("prepare bm25 probe: {e}"))?;
    for row in bstmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))
        .map_err(|e| format!("run bm25 probe: {e}"))?
    {
        let (id, score) = row.map_err(|e| format!("read bm25 row: {e}"))?;
        if !score.is_finite() {
            return Err(format!("non-finite bm25 score for chunk {id}: {score}"));
        }
        println!("bm25 MATCH 'knowledge': chunk id={id} score={score}");
    }

    let matched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'knowledge'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("count bm25 matches: {e}"))?;
    if matched == 0 {
        return Err("FTS5 MATCH 'knowledge' returned no rows".to_string());
    }

    println!(
        "probe_db: OK ({} tables, migrations 1..=5, chunks={chunks_rows}, fts_rows={fts_rows}, bm25 matches={matched})",
        found.len()
    );
    Ok(())
}
