//! FTS5 bm25 parity against the recorded fixture (`fixtures/knowledge.db`,
//! a v5-shape knowledge database).
//!
//! Recorded expectations (spike S1, `.archive/spikes/src/bin/s1_sqlite.rs`,
//! verified once against the fixture):
//! - term `knowledge` → 17 hits, top-3 chunk ids (247, 30, 106) in bm25 rank
//!   order, scores within 1e-3 relative tolerance of the recorded values;
//! - term `RAG` → 1 hit; phrase `"knowledge graph"` → 1 hit;
//!   `knowledge OR RAG` → 18 hits (FTS5 OR union, no overlap);
//! - domain filter on the fixture: hr → 5, it → 3, product → 15,
//!   finance → 0 (multi-domain documents match each of their domains).
//!
//! The fixture is a gitignored binary (provenance in `fixtures/README.md`);
//! the tests skip cleanly when it is absent (fresh checkout).
//!
//! The fixture is the v5 shape: its `chunks` table has no `search_text`
//! column and its `chunks_fts` indexes `chunk_text`, so the [`ChunkDao`]
//! queries (which read `search_text`, a column the Rust init migration
//! adds) cannot run against it. The tests therefore issue the equivalent
//! raw SQL — the same MATCH + bm25 join the DAO issues, minus the
//! `search_text` column — to keep verifying FTS5 engine behavior
//! (identical bm25 scoring on identical data).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use db::test_util::fixture_db;
use rusqlite::{Connection, params};

/// Relative tolerance for bm25 score comparison (spike S1).
const SCORE_TOL: f64 = 1e-3;

/// The fixture path at the repo root (mirrors `db::test_util::fixture_path`).
fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("knowledge.db")
}

/// Run `f` with a pooled connection over the read-only fixture; skips when
/// the fixture binary is absent so CI (fresh checkout) stays green.
fn with_fixture(f: impl FnOnce(&Connection)) {
    if !fixture_path().exists() {
        eprintln!("skip: fixture {} not present", fixture_path().display());
        return;
    }
    let db = fixture_db();
    db.with_conn(f).expect("fixture checkout must not time out");
}

/// The DAO's `FTS_QUERY` in its fixture-compatible form: FTS5 MATCH +
/// `bm25()` ranking + `ORDER BY bm25` (the DAO ranks its results), without
/// the Rust `search_text` column the fixture does not have.
fn match_ranked(conn: &Connection, expr: &str, limit: i64) -> Vec<(i64, f64)> {
    conn.prepare(
        "SELECT c.id, bm25(chunks_fts) \
         FROM chunks c \
         INNER JOIN chunks_fts ON chunks_fts.rowid = c.id \
         WHERE chunks_fts MATCH ?1 \
         ORDER BY bm25(chunks_fts) \
         LIMIT ?2",
    )
    .expect("match query compiles")
    .query_map(params![expr, limit], |row| Ok((row.get(0)?, row.get(1)?)))
    .expect("match query runs")
    .map(|row| row.expect("row decodes"))
    .collect()
}

/// The same join with the DAO's domain filter (`$.domain` string or array
/// member), applied before the `LIMIT`.
fn match_ranked_domain(conn: &Connection, expr: &str, domain: &str, limit: i64) -> Vec<(i64, f64)> {
    conn.prepare(
        "SELECT c.id, bm25(chunks_fts) \
         FROM chunks c \
         INNER JOIN chunks_fts ON chunks_fts.rowid = c.id \
         INNER JOIN documents d ON d.id = c.doc_id \
         WHERE chunks_fts MATCH ?1 \
           AND d.metadata_json IS NOT NULL AND json_valid(d.metadata_json) \
           AND EXISTS (SELECT 1 FROM json_each(d.metadata_json, '$.domain') \
                       WHERE json_each.value = ?2) \
         ORDER BY bm25(chunks_fts) \
         LIMIT ?3",
    )
    .expect("domain match query compiles")
    .query_map(params![expr, domain, limit], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
    .expect("domain match query runs")
    .map(|row| row.expect("row decodes"))
    .collect()
}

/// (г) term `knowledge`: 17 hits, top-3 chunk ids and bm25 scores match the
/// recorded expected values.
#[test]
fn fts5_parity_knowledge_term() {
    with_fixture(|conn| {
        let hits = match_ranked(conn, "knowledge", 20);
        assert_eq!(hits.len(), 17, "term 'knowledge' must hit 17 chunks");

        let top: Vec<i64> = hits.iter().take(3).map(|(id, _)| *id).collect();
        assert_eq!(top, [247, 30, 106], "top-3 chunk ids in bm25 rank order");

        // Scores within 1e-3 relative tolerance of the spike-recorded values.
        let expected = [-4.4453f64, -3.573, -3.4522];
        for ((_, got), want) in hits.iter().take(3).zip(expected) {
            assert!(
                (got - want).abs() <= SCORE_TOL * want.abs(),
                "score {got} outside 1e-3 relative tolerance of recorded {want}"
            );
        }

        // bm25-ranked: scores non-decreasing (more negative = better).
        for (cur, next) in hits.iter().zip(hits.iter().skip(1)) {
            assert!(cur.1 <= next.1, "results must be bm25-ranked");
        }
    });
}

/// The remaining spike S1 hit-count expectations.
#[test]
fn fts5_parity_spike_hit_counts() {
    with_fixture(|conn| {
        for (expr, want) in [
            ("RAG", 1usize),
            ("\"knowledge graph\"", 1),
            ("knowledge OR RAG", 18),
        ] {
            let hits = match_ranked(conn, expr, 100);
            assert_eq!(hits.len(), want, "expr {expr:?}");
        }
    });
}

/// Domain filter on the fixture: per-domain hit counts for `knowledge`.
#[test]
fn fts5_parity_domain_filter() {
    with_fixture(|conn| {
        for (domain, want) in [("hr", 5usize), ("it", 3), ("product", 15), ("finance", 0)] {
            let hits = match_ranked_domain(conn, "knowledge", domain, 20);
            assert_eq!(hits.len(), want, "domain {domain}");
        }
    });
}
