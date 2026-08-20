//! FTS5 bm25 parity against the Go-oracle fixture (`fixtures/knowledge.db`).
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

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use db::test_util::fixture_db;
use db::{ChunkDao, ConnectionOrTx};

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

/// Run `f` with a DAO over the read-only fixture; skips when the fixture
/// binary is absent so CI (fresh checkout) stays green.
fn with_fixture(f: impl FnOnce(&ChunkDao<'_>)) {
    if !fixture_path().exists() {
        eprintln!("skip: fixture {} not present", fixture_path().display());
        return;
    }
    let db = fixture_db();
    db.with_conn(|conn| f(&ChunkDao::new(ConnectionOrTx::Connection(conn))))
        .expect("fixture checkout must not time out");
}

/// (г) term `knowledge`: 17 hits, top-3 chunk ids and bm25 scores match the
/// recorded oracle values.
#[test]
fn fts5_parity_knowledge_term() {
    with_fixture(|chunks| {
        let hits = chunks
            .search_fts("knowledge", 20, None)
            .expect("search must not fail");
        assert_eq!(hits.len(), 17, "term 'knowledge' must hit 17 chunks");

        let top: Vec<i64> = hits.iter().take(3).map(|h| h.chunk.id).collect();
        assert_eq!(top, [247, 30, 106], "top-3 chunk ids in bm25 rank order");

        // Scores within 1e-3 relative tolerance of the spike-recorded values.
        let expected = [-4.4453f64, -3.573, -3.4522];
        for (hit, want) in hits.iter().take(3).zip(expected) {
            let got = hit.score;
            assert!(
                (got - want).abs() <= SCORE_TOL * want.abs(),
                "chunk {} score {got} outside 1e-3 relative tolerance of recorded {want}",
                hit.chunk.id
            );
        }

        // bm25-ranked: scores non-decreasing (more negative = better).
        for (cur, next) in hits.iter().zip(hits.iter().skip(1)) {
            assert!(cur.score <= next.score, "results must be bm25-ranked");
        }
    });
}

/// The remaining spike S1 hit-count expectations.
#[test]
fn fts5_parity_spike_hit_counts() {
    with_fixture(|chunks| {
        for (expr, want) in [
            ("RAG", 1usize),
            ("\"knowledge graph\"", 1),
            ("knowledge OR RAG", 18),
        ] {
            let hits = chunks
                .search_fts(expr, 100, None)
                .unwrap_or_else(|err| panic!("search {expr:?} must not fail: {err}"));
            assert_eq!(hits.len(), want, "expr {expr:?}");
        }
    });
}

/// Domain filter on the fixture: per-domain hit counts for `knowledge`.
#[test]
fn fts5_parity_domain_filter() {
    with_fixture(|chunks| {
        for (domain, want) in [("hr", 5usize), ("it", 3), ("product", 15), ("finance", 0)] {
            let hits = chunks
                .search_fts("knowledge", 20, Some(domain))
                .unwrap_or_else(|err| panic!("domain {domain} search must not fail: {err}"));
            assert_eq!(hits.len(), want, "domain {domain}");
        }
    });
}
