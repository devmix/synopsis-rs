//! Reciprocal Rank Fusion with BM25 calibration (design D4).
//!
//! Faithful port of the oracle's `rrf.go` — every numeric behavior is
//! preserved and differential parity with `../synopsis/internal/search/
//! rrf_test.go` is an acceptance criterion. One internal simplification,
//! no behavior change: the oracle detects "semantic-only" entries via a
//! `BM25Score == 0` sentinel combined with a source-type check; here a
//! missing BM25 score is `Option::<f64>::None`, so a lexical entry with a
//! raw BM25 score of exactly 0.0 is handled by the same code path as any
//! other lexical entry (the oracle's sentinel logic did the same, via the
//! source-type guard).
//!
//! Algorithm (design D4):
//! 1. `score += 1 / (k + rank)` per list the chunk appears in (1-based
//!    ranks); `k <= 0` falls back to [`DEFAULT_RRF_K`].
//! 2. BM25 min-max normalization over **lexical/hybrid entries only**
//!    (semantic-only entries get neutral 0.5); all-equal range → neutral
//!    0.5 for everyone.
//! 3. RRF scores min-max normalized to `[0, 1]`; all-equal → neutral 0.5.
//! 4. Final score = 0.7·rrf_norm + 0.3·bm25_norm; sort descending score
//!    with ascending `chunk_id` tiebreak; `top_n <= 0` → no truncation;
//!    1-based ranks assigned after the sort.

use std::collections::HashMap;

use crate::{LexicalHit, SearchResult, SemanticHit, SourceType};

/// Calibrated RRF constant `k` applied when the caller passes `k <= 0`
/// (oracle parity: lower `k` increases rank sensitivity, ~8× vs `k = 60`).
pub const DEFAULT_RRF_K: i32 = 20;

/// RRF weight in the final calibrated score (design D4).
const RRF_WEIGHT: f64 = 0.7;

/// BM25 weight in the final calibrated score (design D4).
const BM25_WEIGHT: f64 = 0.3;

/// Neutral normalization value: used for entries with no BM25 data and for
/// every score pool with zero spread (all-equal values).
const NEUTRAL: f64 = 0.5;

/// Fused entry accumulated per chunk across both ranked lists (oracle
/// `rrfEntry`). Private: the public surface is [`SearchResult`].
#[derive(Debug)]
struct FusionEntry {
    chunk_id: i64,
    chunk_text: String,
    document_id: i64,
    sequence_num: i64,
    start_offset: Option<i64>,
    end_offset: Option<i64>,
    /// Sum of `1 / (k + rank)` over the lists the chunk appears in;
    /// replaced in place by the min-max normalized value.
    rrf_score: f64,
    /// Raw BM25 score (lower is better) when the chunk is in the lexical
    /// list; `None` for semantic-only chunks. Replaced in place by the
    /// min-max normalized value (always `Some` after
    /// [`normalize_bm25`]).
    bm25: Option<f64>,
    source_type: SourceType,
}

/// Merge two ranked result lists with RRF + BM25 calibration (design D4).
///
/// `lexical` and `semantic` are pre-ranked (index = rank − 1). `k <= 0`
/// falls back to [`DEFAULT_RRF_K`]; `top_n <= 0` returns the full pool.
/// The result is sorted by descending calibrated score, ties broken by
/// ascending `chunk_id` (deterministic), with 1-based ranks assigned
/// after the sort.
pub fn reciprocal_rank_fusion(
    lexical: &[LexicalHit],
    semantic: &[SemanticHit],
    k: i32,
    top_n: i32,
) -> Vec<SearchResult> {
    let k = if k <= 0 { DEFAULT_RRF_K } else { k };

    let mut pool: HashMap<i64, FusionEntry> = HashMap::new();

    // Lexical list: the raw BM25 score is carried (lower is better in
    // SQLite FTS5).
    for (rank, hit) in lexical.iter().enumerate() {
        let entry = pool.entry(hit.chunk_id).or_insert_with(|| FusionEntry {
            chunk_id: hit.chunk_id,
            chunk_text: hit.chunk_text.clone(),
            document_id: hit.document_id,
            sequence_num: hit.sequence_num,
            start_offset: hit.start_offset,
            end_offset: hit.end_offset,
            rrf_score: 0.0,
            bm25: Some(hit.score),
            source_type: SourceType::Lexical,
        });
        entry.rrf_score += 1.0 / (k + rank as i32 + 1) as f64; // rank is 1-based
    }

    // Semantic list: no BM25 data; a chunk already in the lexical list
    // becomes hybrid.
    for (rank, hit) in semantic.iter().enumerate() {
        let entry = pool.entry(hit.chunk_id).or_insert_with(|| FusionEntry {
            chunk_id: hit.chunk_id,
            chunk_text: hit.chunk_text.clone(),
            document_id: hit.document_id,
            sequence_num: hit.sequence_num,
            start_offset: hit.start_offset,
            end_offset: hit.end_offset,
            rrf_score: 0.0,
            bm25: None,
            source_type: SourceType::Semantic,
        });
        if entry.source_type == SourceType::Lexical {
            entry.source_type = SourceType::Hybrid;
        }
        entry.rrf_score += 1.0 / (k + rank as i32 + 1) as f64; // rank is 1-based
    }

    let mut entries: Vec<FusionEntry> = pool.into_values().collect();

    // Min-max normalize BM25 over lexical entries only (semantic-only get
    // NEUTRAL), then RRF over the whole pool; calibrate the final score.
    normalize_bm25(&mut entries);
    normalize_rrf(&mut entries);

    let mut results: Vec<SearchResult> = entries
        .into_iter()
        .map(|e| SearchResult {
            chunk_id: e.chunk_id,
            chunk_text: e.chunk_text,
            document_id: e.document_id,
            sequence_num: e.sequence_num,
            start_offset: e.start_offset,
            end_offset: e.end_offset,
            document_path: String::new(), // filled by the enricher (task 4.3)
            score: RRF_WEIGHT * e.rrf_score + BM25_WEIGHT * e.bm25.unwrap_or(NEUTRAL),
            rank: 0, // assigned after the sort
            source_type: e.source_type,
            metadata: serde_json::Map::new(), // filled by tasks 4.3–4.5
            entities: Vec::new(),             // filled by the enricher (task 4.3)
        })
        .collect();

    // Descending score; ascending chunk_id as the deterministic tiebreak.
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });

    // Trim to top_n (top_n <= 0 means no truncation).
    if top_n > 0 {
        let limit = top_n as usize;
        if results.len() > limit {
            results.truncate(limit);
        }
    }

    // 1-based ranks, assigned after sorting.
    for (i, result) in results.iter_mut().enumerate() {
        result.rank = i + 1;
    }

    results
}

/// Min-max normalize BM25 scores in place (oracle `normalizeBM25`).
///
/// BM25 in SQLite FTS5 is "lower is better" (distance-like); the
/// normalization inverts so higher is better. The min/max range is computed
/// only over entries with actual BM25 data (lexical/hybrid), so
/// semantic-only entries do not skew it — they receive [`NEUTRAL`]. A
/// zero-spread range (all-equal scores) yields [`NEUTRAL`] for everyone.
fn normalize_bm25(entries: &mut [FusionEntry]) {
    let (min_bm25, max_bm25) = entries
        .iter()
        .filter_map(|e| e.bm25)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    if min_bm25 == f64::INFINITY {
        // No entry has BM25 data (all semantic-only): neutral for all.
        for entry in entries.iter_mut() {
            entry.bm25 = Some(NEUTRAL);
        }
        return;
    }

    let range = max_bm25 - min_bm25;
    if range == 0.0 {
        // All BM25 scores equal: neutral for all.
        for entry in entries.iter_mut() {
            entry.bm25 = Some(NEUTRAL);
        }
        return;
    }

    for entry in entries.iter_mut() {
        entry.bm25 = Some(match entry.bm25 {
            // Invert (lower-is-better → higher-is-better): (max - v) / range.
            Some(v) => (max_bm25 - v) / range,
            // Semantic-only result: no BM25 data, neutral value.
            None => NEUTRAL,
        });
    }
}

/// Min-max normalize RRF scores in place to `[0, 1]` (oracle
/// `normalizeRRF`) so they blend fairly with the normalized BM25 scores.
/// A zero-spread pool yields [`NEUTRAL`] for everyone.
fn normalize_rrf(entries: &mut [FusionEntry]) {
    let (min_score, max_score) = entries
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), e| {
            (lo.min(e.rrf_score), hi.max(e.rrf_score))
        });
    if min_score == f64::INFINITY {
        return; // empty pool
    }

    let range = max_score - min_score;
    if range == 0.0 {
        for entry in entries.iter_mut() {
            entry.rrf_score = NEUTRAL;
        }
        return;
    }

    for entry in entries.iter_mut() {
        entry.rrf_score = (entry.rrf_score - min_score) / range;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // Table-driven parity cases use wide tuples (oracle rrf_test.go shape).
    #![allow(clippy::type_complexity)]

    use super::*;

    fn lexical(chunk_id: i64, text: &str, document_id: i64) -> LexicalHit {
        LexicalHit {
            chunk_id,
            chunk_text: text.to_owned(),
            document_id,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            score: 0.0, // BM25 zero value (oracle default)
        }
    }

    fn lexical_scored(chunk_id: i64, text: &str, document_id: i64, score: f64) -> LexicalHit {
        LexicalHit {
            score,
            ..lexical(chunk_id, text, document_id)
        }
    }

    fn semantic(chunk_id: i64, text: &str, document_id: i64, score: f64) -> SemanticHit {
        SemanticHit {
            chunk_id,
            chunk_text: text.to_owned(),
            document_id,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            score, // cosine distance
        }
    }

    fn entry(chunk_id: i64, bm25: Option<f64>, source_type: SourceType) -> FusionEntry {
        FusionEntry {
            chunk_id,
            chunk_text: String::new(),
            document_id: 0,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            rrf_score: 0.0,
            bm25,
            source_type,
        }
    }

    // Parity with oracle TestReciprocalRankFusion (rrf_test.go).
    // Named `fusion_table` (not `reciprocal_rank_fusion`) so this test item
    // does not shadow the function under test for the rest of the module.
    #[test]
    fn fusion_table() {
        let cases: Vec<(
            &str,
            Vec<LexicalHit>,
            Vec<SemanticHit>,
            i32,
            i32,
            usize,
            Option<i64>,
            Option<f64>,
        )> = vec![
            ("empty inputs", vec![], vec![], 60, 10, 0, None, None),
            (
                "only lexical results",
                vec![
                    lexical(1, "alpha", 1),
                    lexical(2, "beta", 1),
                    lexical(3, "gamma", 2),
                ],
                vec![],
                60,
                10,
                3,
                Some(1),
                Some(0.15), // calibrated: 0.7·rrf + 0.3·bm25_norm; BM25 all zero → norm=0.5
            ),
            (
                "only semantic results",
                vec![],
                vec![
                    semantic(10, "vector_a", 3, 0.1),
                    semantic(11, "vector_b", 3, 0.2),
                ],
                60,
                10,
                2,
                Some(10),
                Some(0.14), // calibrated: semantic-only gets BM25 norm=0.5 neutral
            ),
            (
                "overlapping results — shared chunk gets higher score",
                vec![
                    lexical(1, "shared text", 1),
                    lexical(2, "only lexical", 1),
                    lexical(3, "third lexical", 2),
                ],
                vec![
                    semantic(1, "shared text", 1, 0.05),
                    semantic(4, "only semantic", 2, 0.1),
                ],
                60,
                10,
                4,
                Some(1), // chunk 1 appears in both lists → highest RRF score
                Some(0.17),
            ),
            (
                "topN truncation",
                (1..=5).map(|i| lexical(i, "text", 1)).collect(),
                vec![],
                60,
                2,
                2,
                Some(1),
                None,
            ),
            (
                "custom k parameter",
                vec![lexical(1, "a", 1)],
                vec![],
                10, // smaller k → higher RRF scores
                10,
                1,
                Some(1),
                Some(0.2), // calibrated with BM25 norm; raw RRF ≈ 0.0909
            ),
            (
                "default k when zero",
                vec![lexical(42, "hello", 5)],
                vec![],
                0, // should default to 20 (calibrated)
                10,
                1,
                Some(42),
                Some(0.15), // calibrated with k=20 and BM25 norm
            ),
        ];

        for (
            name,
            lexical_hits,
            semantic_hits,
            k,
            top_n,
            want_count,
            want_first_id,
            want_top_score_gt,
        ) in cases
        {
            let got = reciprocal_rank_fusion(&lexical_hits, &semantic_hits, k, top_n);

            assert_eq!(got.len(), want_count, "{name}: count mismatch");

            if let Some(first_id) = want_first_id {
                assert!(!got.is_empty(), "{name}: expected at least one result");
                assert_eq!(got[0].chunk_id, first_id, "{name}: top chunk_id mismatch");
            }

            if let Some(min_score) = want_top_score_gt {
                assert!(
                    got[0].score > min_score,
                    "{name}: top score {} should be > {min_score}",
                    got[0].score
                );
            }

            // Ranks are 1-based and assigned after the sort.
            for (i, result) in got.iter().enumerate() {
                assert_eq!(result.rank, i + 1, "{name}: rank mismatch at {i}");
            }

            // Scores are in descending order.
            for pair in got.iter().zip(got.iter().skip(1)) {
                assert!(
                    pair.0.score >= pair.1.score,
                    "{name}: scores not sorted: {} > {}",
                    pair.0.score,
                    pair.1.score
                );
            }

            // Source type is one of the three wire words.
            for result in &got {
                let word = result.source_type.as_str();
                assert!(
                    matches!(word, "lexical" | "semantic" | "hybrid"),
                    "{name}: unexpected source_type {word:?} for chunk {}",
                    result.chunk_id
                );
            }
        }
    }

    // Parity with oracle TestReciprocalRankFusion_ScoreCalculation.
    #[test]
    fn score_calculation() {
        let k = 60;
        let lexical = vec![
            lexical(1, "a", 1), // rank 1 in lexical → rrf = 1/61
            lexical(2, "b", 1), // rank 2 in lexical → rrf = 1/62
        ];
        let sem = vec![
            semantic(2, "b", 1, 0.1), // rank 1 in semantic → rrf += 1/61
            semantic(3, "c", 2, 0.2), // rank 2 in semantic → rrf = 1/62
        ];

        let got = reciprocal_rank_fusion(&lexical, &sem, k, 10);
        assert_eq!(got.len(), 3, "count mismatch");

        let chunk2 = got.iter().find(|r| r.chunk_id == 2).unwrap();

        // Chunk 2 appears in both lists: rrfScore = 1/62 + 1/61 ≈ 0.0321.
        // With BM25 calibration (all BM25=0 → norm=0.5): final = 0.7·rrf + 0.3·0.5.
        let raw_rrf = 1.0 / (k + 2) as f64 + 1.0 / (k + 1) as f64; // rank 2 lexical + rank 1 semantic
        let expected_min = RRF_WEIGHT * raw_rrf + BM25_WEIGHT * 0.5;
        assert!(
            chunk2.score >= expected_min - 0.0001,
            "chunk 2 score {} < calibrated minimum {expected_min} (raw RRF {raw_rrf})",
            chunk2.score
        );

        // Chunk 2 should be ranked #1 (highest combined score).
        assert_eq!(got[0].chunk_id, 2, "top result chunk_id mismatch");
    }

    // Parity with oracle TestReciprocalRankFusion_SourceType.
    #[test]
    fn source_type() {
        let cases: Vec<(&str, Vec<LexicalHit>, Vec<SemanticHit>, i64, SourceType)> = vec![
            (
                "chunk only in lexical",
                vec![lexical(1, "a", 1)],
                vec![],
                1,
                SourceType::Lexical,
            ),
            (
                "chunk only in semantic",
                vec![],
                vec![semantic(2, "b", 1, 0.1)],
                2,
                SourceType::Semantic,
            ),
            (
                "chunk in both lists",
                vec![lexical(3, "c", 1)],
                vec![semantic(3, "c", 1, 0.05)],
                3,
                SourceType::Hybrid,
            ),
        ];

        for (name, lexical_hits, semantic_hits, chunk_id, want) in cases {
            let got = reciprocal_rank_fusion(&lexical_hits, &semantic_hits, 60, 10);
            let found = got.iter().find(|r| r.chunk_id == chunk_id);
            assert!(found.is_some(), "{name}: chunk {chunk_id} not found");
            assert_eq!(
                found.unwrap().source_type,
                want,
                "{name}: source_type mismatch for chunk {chunk_id}"
            );
        }
    }

    // Parity with oracle TestReciprocalRankFusion_StableTiebreak:
    // equal-score chunks are ordered by ascending chunk_id (deterministic).
    #[test]
    fn stable_tiebreak() {
        let cases: Vec<(&str, Vec<LexicalHit>, Vec<SemanticHit>, i32, i32, Vec<i64>)> = vec![
            (
                "equal scores — lower ChunkID first",
                vec![lexical(5, "a", 1)],
                vec![semantic(3, "b", 1, 0.1)],
                60,
                10,
                vec![3, 5], // chunk 3 < chunk 5, equal scores → 3 first
            ),
            (
                "three chunks — two tied, one lower",
                vec![lexical(10, "a", 1)], // rank 1 in lexical
                vec![
                    semantic(20, "b", 1, 0.1), // rank 1 in semantic (tied with chunk 10)
                    semantic(5, "c", 1, 0.1),  // rank 2 in semantic (lower RRF)
                ],
                60,
                10,
                vec![10, 20, 5], // tied chunks 10 & 20 by ChunkID asc, then chunk 5
            ),
        ];

        for (name, lexical_hits, semantic_hits, k, top_n, want_ids) in cases {
            let got = reciprocal_rank_fusion(&lexical_hits, &semantic_hits, k, top_n);
            assert_eq!(got.len(), want_ids.len(), "{name}: result count mismatch");
            for (i, want_id) in want_ids.iter().enumerate() {
                assert_eq!(
                    got[i].chunk_id, *want_id,
                    "{name}: result[{i}].chunk_id mismatch"
                );
            }

            // Determinism: a second call yields the identical order.
            let again = reciprocal_rank_fusion(&lexical_hits, &semantic_hits, k, top_n);
            for (i, (a, b)) in got.iter().zip(again.iter()).enumerate() {
                assert_eq!(a.chunk_id, b.chunk_id, "{name}: non-deterministic at {i}");
            }
        }
    }

    // Parity with oracle TestReciprocalRankFusion_TopNGuard: top_n <= 0
    // returns all results without truncation (and does not panic).
    #[test]
    fn top_n_guard() {
        let lexical = vec![lexical(1, "a", 1), lexical(2, "b", 1), lexical(3, "c", 1)];

        for (name, top_n, want_count) in [
            ("topN zero — return all", 0, 3),
            ("topN negative — return all", -1, 3),
            ("topN positive — truncate normally", 2, 2),
        ] {
            let got = reciprocal_rank_fusion(&lexical, &[], 60, top_n);
            assert_eq!(got.len(), want_count, "{name}: count mismatch");
        }
    }

    // Parity with oracle TestNormalizeBM25_MixedPool: semantic-only entries
    // (no BM25 data) do not skew the min/max range; they receive neutral
    // 0.5 while lexical entries span [0, 1].
    #[test]
    fn normalize_bm25_mixed_pool() {
        let mut entries = vec![
            entry(1, Some(0.5), SourceType::Lexical), // best BM25 (lower is better)
            entry(2, Some(4.0), SourceType::Lexical), // worse BM25
            entry(3, None, SourceType::Semantic),     // no BM25 data → neutral 0.5
        ];

        normalize_bm25(&mut entries);

        assert_eq!(entries[2].bm25, Some(0.5), "semantic-only must be neutral");
        assert_eq!(
            entries[0].bm25,
            Some(1.0),
            "best lexical must normalize to 1.0"
        );
        assert_eq!(
            entries[1].bm25,
            Some(0.0),
            "worst lexical must normalize to 0.0"
        );

        // The semantic-only entry must not have pulled minBM25 to 0: if it
        // had, the range would be [0, 4] and the best BM25 (0.5) would
        // normalize to 0.875 instead of 1.0.
        assert!(
            entries[0].bm25.unwrap() >= 0.99,
            "best lexical BM25 norm ({:?}) too low — semantic-only entry skewed the range",
            entries[0].bm25
        );
    }

    // Parity with oracle TestReciprocalRankFusion_BM25Calibration: BM25
    // scores are normalized and blended into the final calibrated score.
    #[test]
    fn bm25_calibration() {
        // (1) BM25 differentiation — lower BM25 (better) ranks higher.
        let lexical = vec![
            lexical_scored(1, "best match", 1, 0.5),
            lexical_scored(2, "ok match", 1, 2.0),
            lexical_scored(3, "weak match", 1, 5.0),
        ];
        let got = reciprocal_rank_fusion(&lexical, &[], 60, 10);
        assert_eq!(got.len(), 3, "differentiation: count mismatch");
        assert_eq!(got[0].chunk_id, 1, "best BM25 must rank first");
        assert_eq!(got[1].chunk_id, 2, "second BM25 must rank second");
        assert_eq!(got[2].chunk_id, 3, "weakest BM25 must rank third");
        assert!(
            got[0].score > got[1].score,
            "chunk 1 score ({}) should exceed chunk 2 ({})",
            got[0].score,
            got[1].score
        );

        // (2) Semantic-only gets neutral BM25 → positive final score.
        let sem = vec![semantic(10, "vector result", 3, 0.1)];
        let got = reciprocal_rank_fusion(&[], &sem, 60, 10);
        assert_eq!(got.len(), 1, "semantic-only: count mismatch");
        assert!(got[0].score > 0.0, "semantic-only score must be positive");

        // (3) Hybrid result combines RRF and BM25: the calibrated score
        // exceeds the pure (unnormalized) RRF sum.
        let lexical = vec![lexical_scored(1, "shared", 1, 1.0)];
        let sem = vec![semantic(1, "shared", 1, 0.5)];
        let got = reciprocal_rank_fusion(&lexical, &sem, 60, 10);
        assert_eq!(got.len(), 1, "hybrid: count mismatch");
        assert_eq!(
            got[0].source_type,
            SourceType::Hybrid,
            "expected hybrid source type"
        );
        let pure_rrf = 1.0 / 61.0 + 1.0 / 61.0; // both lists, rank 1
        assert!(
            got[0].score > pure_rrf,
            "calibrated score ({}) should exceed pure RRF ({pure_rrf})",
            got[0].score
        );
    }
}
