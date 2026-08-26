//! Post-fusion reranking: business rules, freshness and authority boosts
//! (design D7).
//!
//! Oracle mapping: `../synopsis/internal/search/reranker.go`, re-architected
//! per the 2026-08-19 migration principles (functional copy, not a code
//! copy).
//!
//! [`Reranker::rerank`] applies, in order:
//!
//! 1. **business rules** — `is_deprecated` × `deprecated_boost`,
//!    `is_official` × `official_boost`, expired (`valid_to` before now)
//!    × 0.1; the factors compose multiplicatively;
//! 2. **freshness** — `updated_at` (enricher-normalized RFC3339) strictly
//!    after `now − recent_days` × `recent_boost`;
//! 3. **authority** — `document_source_type` present in the
//!    `authority_boost` map × that factor (absent → ×1.0);
//!
//! then re-sorts the pool by score descending and reassigns 1-based ranks.
//!
//! **Conscious deviations from the oracle:**
//! - `rerank` mutates the pool in place (`&mut [SearchResult]`) — the same
//!   idiom as the enricher (design D6); the oracle returned the slice it
//!   mutated;
//! - the stage methods are module-private: the public surface is `rerank`
//!   only (the oracle exposed every stage);
//! - "now" comes from [`std::time::SystemTime`]; the recency window is
//!   `recent_days × 86400` seconds — the oracle's `time.AddDate(0, 0, -n)`
//!   is calendar-day arithmetic, identical for UTC instants;
//! - timestamp parsing goes through the workspace date/time seam
//!   [`utils::temporal::parse_epoch_seconds`], so the SQLite
//!   `CURRENT_TIMESTAMP` layout (`"YYYY-MM-DD HH:MM:SS"`) is also accepted
//!   — the oracle's strict `time.Parse(time.RFC3339, …)` silently ignored
//!   such values (notably for `valid_to`, where an expired document then
//!   escaped the penalty).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use config::preset::SearchConfig;
use utils::temporal::parse_epoch_seconds;

use crate::SearchResult;

/// Default `deprecated_boost` (design D7).
const DEFAULT_DEPRECATED_BOOST: f64 = 0.2;
/// Default `official_boost` (design D7).
const DEFAULT_OFFICIAL_BOOST: f64 = 1.5;
/// Default `recent_boost` (design D7).
const DEFAULT_RECENT_BOOST: f64 = 1.2;
/// Default `recent_days` (design D7).
const DEFAULT_RECENT_DAYS: i32 = 90;
/// Severe penalty for expired documents (`valid_to` before now).
const EXPIRED_PENALTY: f64 = 0.1;
/// Seconds in a day (the recency window is `recent_days × 86400`).
const SECONDS_PER_DAY: i64 = 86_400;

/// Applies business rules, freshness and authority boosts to fused results.
///
/// Boost factors default to 0.2 / 1.5 / 1.2 / 90 days (design D7); a
/// [`SearchConfig`] overrides a factor only when it is `> 0`.
#[derive(Debug)]
pub struct Reranker {
    /// Deprecated-document multiplier.
    deprecated_boost: f64,
    /// Official-document multiplier.
    official_boost: f64,
    /// Freshness multiplier for documents updated within `recent_days`.
    recent_boost: f64,
    /// Freshness window in days.
    recent_days: i32,
    /// Document source type → authority multiplier (absent → 1.0).
    authority_boost: HashMap<String, f64>,
}

impl Reranker {
    /// Create a reranker with the default boost factors, overridden by
    /// `config` when its factors are `> 0` (a `None` config keeps all
    /// defaults; a non-empty `authority_boost` map replaces the default).
    pub fn new(config: Option<&SearchConfig>) -> Self {
        let mut reranker = Self {
            deprecated_boost: DEFAULT_DEPRECATED_BOOST,
            official_boost: DEFAULT_OFFICIAL_BOOST,
            recent_boost: DEFAULT_RECENT_BOOST,
            recent_days: DEFAULT_RECENT_DAYS,
            authority_boost: HashMap::new(),
        };
        if let Some(config) = config {
            if config.deprecated_boost > 0.0 {
                reranker.deprecated_boost = config.deprecated_boost;
            }
            if config.official_boost > 0.0 {
                reranker.official_boost = config.official_boost;
            }
            if config.recent_boost > 0.0 {
                reranker.recent_boost = config.recent_boost;
            }
            if config.recent_days > 0 {
                reranker.recent_days = config.recent_days;
            }
            if !config.authority_boost.is_empty() {
                reranker.authority_boost = config.authority_boost.clone();
            }
        }
        reranker
    }

    /// Apply all boosts to `results` in place, re-sort by score
    /// descending and reassign 1-based ranks. An empty pool is a no-op.
    pub fn rerank(&self, results: &mut [SearchResult]) {
        if results.is_empty() {
            return;
        }
        self.apply_business_rules(results);
        self.apply_freshness_boost(results);
        self.apply_authority_boost(results);

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (rank, result) in results.iter_mut().enumerate() {
            result.rank = rank + 1;
        }
    }

    /// Business rules: deprecated × `deprecated_boost`, official ×
    /// `official_boost`, expired (`valid_to` before now) × 0.1 — the
    /// factors compose multiplicatively (a deprecated *and* official
    /// document gets both).
    fn apply_business_rules(&self, results: &mut [SearchResult]) {
        for result in results.iter_mut() {
            result.score *= self.boost_factor(&result.metadata);
        }
    }

    /// Freshness: documents whose `updated_at` is strictly after
    /// `now − recent_days` get × `recent_boost`. Absent or unparseable
    /// timestamps are ignored (no boost).
    fn apply_freshness_boost(&self, results: &mut [SearchResult]) {
        let Some(now) = now_unix_seconds() else {
            return;
        };
        let threshold = now - i64::from(self.recent_days) * SECONDS_PER_DAY;
        for result in results.iter_mut() {
            if let Some(serde_json::Value::String(updated_at)) = result.metadata.get("updated_at")
                && parse_epoch_seconds(updated_at).is_some_and(|updated| updated > threshold)
            {
                result.score *= self.recent_boost;
            }
        }
    }

    /// Authority: documents whose `document_source_type` is a key of the
    /// `authority_boost` map get × that factor; any other type (or an
    /// absent key) is × 1.0.
    fn apply_authority_boost(&self, results: &mut [SearchResult]) {
        for result in results.iter_mut() {
            if let Some(serde_json::Value::String(source_type)) =
                result.metadata.get("document_source_type")
                && let Some(boost) = self.authority_boost.get(source_type)
            {
                result.score *= *boost;
            }
        }
    }

    /// Combined business-rule multiplier for one result (1.0 when no rule
    /// fires; wrong-typed flags are ignored, as in the oracle).
    fn boost_factor(&self, metadata: &serde_json::Map<String, serde_json::Value>) -> f64 {
        let mut factor = 1.0;
        if metadata
            .get("is_deprecated")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            factor *= self.deprecated_boost;
        }
        if metadata
            .get("is_official")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            factor *= self.official_boost;
        }
        if let Some(serde_json::Value::String(valid_to)) = metadata.get("valid_to")
            && !valid_to.is_empty()
            && let (Some(expires), Some(now)) = (parse_epoch_seconds(valid_to), now_unix_seconds())
            && expires < now
        {
            factor *= EXPIRED_PENALTY;
        }
        factor
    }
}

/// Current time as seconds since the Unix epoch; `None` only if the system
/// clock precedes the epoch (or overflows `i64`).
fn now_unix_seconds() -> Option<i64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_secs()).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use db::test_util::in_memory_db;
    use db::{ChunkDao, ChunkEntityDao, Db, DocumentDao};
    use serde_json::json;

    use super::*;
    use crate::Enricher;

    /// A result with the given ids, score and metadata.
    fn result(
        chunk_id: i64,
        document_id: i64,
        score: f64,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> SearchResult {
        SearchResult {
            chunk_id,
            chunk_text: String::new(),
            document_id,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            document_path: String::new(),
            score,
            rank: 0,
            source_type: String::new(),
            metadata,
            entities: Vec::new(),
        }
    }

    /// A metadata bag from a JSON object literal.
    fn meta(object: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let serde_json::Value::Object(map) = object else {
            panic!("expected a JSON object");
        };
        map
    }

    /// Scores within 1e-4 of the expected values (the oracle's absDiff).
    fn assert_scores(results: &[SearchResult], expected: &[f64]) {
        assert_eq!(results.len(), expected.len(), "pool size");
        for (result, expected) in results.iter().zip(expected) {
            assert!(
                (result.score - expected).abs() <= 1e-4,
                "score = {}, want {}",
                result.score,
                expected
            );
        }
    }

    /// Chunk ids in the given order.
    fn assert_ids(results: &[SearchResult], expected: &[i64]) {
        let ids: Vec<i64> = results.iter().map(|r| r.chunk_id).collect();
        assert_eq!(ids.as_slice(), expected);
    }

    /// Scores non-increasing and ranks 1..=n sequential (oracle
    /// TestReranker_Rerank's invariants).
    fn assert_sorted_and_ranked(results: &[SearchResult]) {
        for pair in results.windows(2) {
            assert!(
                pair[1].score <= pair[0].score,
                "scores not descending: {} > {}",
                pair[0].score,
                pair[1].score
            );
        }
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.rank, i + 1, "rank at position {i}");
        }
    }

    // ── constructor ──────────────────────────────────────────────────────

    // Oracle TestNewReranker.
    #[test]
    fn new_defaults() {
        let reranker = Reranker::new(None);
        assert_eq!(reranker.deprecated_boost, 0.2);
        assert_eq!(reranker.official_boost, 1.5);
        assert_eq!(reranker.recent_boost, 1.2);
        assert_eq!(reranker.recent_days, 90);
        assert!(reranker.authority_boost.is_empty());
    }

    // Design D7: a config factor overrides only when > 0; a non-empty
    // authority map replaces the default.
    #[test]
    fn config_overrides_only_positive_values() {
        // `official_boost` stays at the config default (0.0) so the
        // reranker's 1.5 default survives.
        let mut config = SearchConfig {
            deprecated_boost: 0.1,
            recent_boost: -1.0, // keep the 1.2 default
            recent_days: 30,
            ..SearchConfig::default()
        };

        let reranker = Reranker::new(Some(&config));
        assert_eq!(reranker.deprecated_boost, 0.1);
        assert_eq!(reranker.official_boost, 1.5);
        assert_eq!(reranker.recent_boost, 1.2);
        assert_eq!(reranker.recent_days, 30);
        assert!(reranker.authority_boost.is_empty());

        config.authority_boost.insert("policy".to_owned(), 2.0);
        let reranker = Reranker::new(Some(&config));
        assert_eq!(reranker.authority_boost.get("policy"), Some(&2.0));
    }

    // ── rerank (oracle TestReranker_Rerank) ──────────────────────────────

    #[test]
    fn rerank_empty_pool_is_noop() {
        let mut results: Vec<SearchResult> = Vec::new();
        Reranker::new(None).rerank(&mut results);
        assert!(results.is_empty());
    }

    // "deprecated document score reduced": 1.0×0.2 = 0.2 < 0.8 → the
    // deprecated chunk drops to last.
    #[test]
    fn rerank_deprecated_document_demoted() {
        let mut reranker = Reranker::new(None);
        reranker.official_boost = 1.0;
        reranker.recent_boost = 1.0;
        let mut results = vec![
            result(1, 0, 1.0, meta(json!({"is_deprecated": true}))),
            result(2, 0, 0.8, meta(json!({}))),
        ];
        reranker.rerank(&mut results);
        assert_ids(&results, &[2, 1]);
        assert_sorted_and_ranked(&results);
        assert_scores(&results, &[0.8, 0.2]);
    }

    // "official document score increased": 0.5×1.5 = 0.75 < 0.8 → order
    // unchanged, but closer.
    #[test]
    fn rerank_official_document_boosted() {
        let mut reranker = Reranker::new(None);
        reranker.deprecated_boost = 1.0;
        reranker.recent_boost = 1.0;
        let mut results = vec![
            result(1, 0, 0.5, meta(json!({"is_official": true}))),
            result(2, 0, 0.8, meta(json!({}))),
        ];
        reranker.rerank(&mut results);
        assert_ids(&results, &[2, 1]);
        assert_sorted_and_ranked(&results);
        assert_scores(&results, &[0.8, 0.75]);
    }

    // "recent document gets freshness boost": 0.5×1.2 = 0.6 < 0.8.
    #[test]
    fn rerank_recent_document_freshness_boost() {
        let mut reranker = Reranker::new(None);
        reranker.deprecated_boost = 1.0;
        reranker.official_boost = 1.0;
        let now = now_unix_seconds().unwrap();
        let mut results = vec![
            result(1, 0, 0.5, meta(json!({"updated_at": rfc3339_utc(now)}))),
            result(
                2,
                0,
                0.8,
                meta(json!({"updated_at": rfc3339_utc(now - 100 * SECONDS_PER_DAY)})),
            ),
        ];
        reranker.rerank(&mut results);
        assert_ids(&results, &[2, 1]);
        assert_sorted_and_ranked(&results);
        assert_scores(&results, &[0.8, 0.6]);
    }

    // "combined boosts reorder results": 0.3×1.5 = 0.45; 0.5; 0.4×0.2
    // = 0.08 → order 2, 1, 3.
    #[test]
    fn rerank_combined_boosts_reorder() {
        let mut reranker = Reranker::new(None);
        reranker.recent_boost = 1.0;
        let mut results = vec![
            result(1, 0, 0.3, meta(json!({"is_official": true}))),
            result(2, 0, 0.5, meta(json!({}))),
            result(3, 0, 0.4, meta(json!({"is_deprecated": true}))),
        ];
        reranker.rerank(&mut results);
        assert_ids(&results, &[2, 1, 3]);
        assert_sorted_and_ranked(&results);
        assert_scores(&results, &[0.5, 0.45, 0.08]);
    }

    // "ranks reassigned after re-sorting": no boost, but stale ranks are
    // rewritten 1..=n.
    #[test]
    fn rerank_reassigns_ranks() {
        let mut reranker = Reranker::new(None);
        reranker.deprecated_boost = 1.0;
        reranker.official_boost = 1.0;
        reranker.recent_boost = 1.0;
        let mut results = vec![
            result(1, 0, 0.9, meta(json!({}))),
            result(2, 0, 0.5, meta(json!({}))),
        ];
        results[0].rank = 7;
        results[1].rank = 3;
        reranker.rerank(&mut results);
        assert_ids(&results, &[1, 2]);
        assert_sorted_and_ranked(&results);
    }

    // ── business rules (oracle TestReranker_ApplyBusinessRules) ──────────

    #[test]
    fn business_rules_empty_pool() {
        let mut results: Vec<SearchResult> = Vec::new();
        Reranker::new(None).apply_business_rules(&mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn business_rules_deprecated_penalty() {
        let reranker = Reranker::new(None); // deprecated_boost 0.2
        let mut results = vec![result(1, 0, 1.0, meta(json!({"is_deprecated": true})))];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[0.2]);
    }

    #[test]
    fn business_rules_official_boost() {
        let reranker = Reranker::new(None); // official_boost 1.5
        let mut results = vec![result(1, 0, 1.0, meta(json!({"is_official": true})))];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[1.5]);
    }

    #[test]
    fn business_rules_expired_penalty() {
        let mut reranker = Reranker::new(None);
        reranker.deprecated_boost = 1.0;
        reranker.official_boost = 1.0;
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"valid_to": "2020-01-01T00:00:00Z"})),
        )];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[0.1]);
    }

    #[test]
    fn business_rules_normal_document_unchanged() {
        let reranker = Reranker::new(None);
        let mut results = vec![result(1, 0, 1.0, meta(json!({})))];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[1.0]);
    }

    // 1.0 × 0.2 × 1.5: the factors compose multiplicatively.
    #[test]
    fn business_rules_deprecated_and_official_compose() {
        let reranker = Reranker::new(None);
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"is_deprecated": true, "is_official": true})),
        )];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[0.3]);
    }

    #[test]
    fn business_rules_multiple_documents() {
        let reranker = Reranker::new(None);
        let mut results = vec![
            result(1, 0, 1.0, meta(json!({"is_deprecated": true}))),
            result(2, 0, 1.0, meta(json!({"is_official": true}))),
            result(3, 0, 1.0, meta(json!({}))),
        ];
        reranker.apply_business_rules(&mut results);
        assert_scores(&results, &[0.2, 1.5, 1.0]);
    }

    // ── freshness (oracle TestReranker_ApplyFreshnessBoost) ──────────────

    #[test]
    fn freshness_empty_pool() {
        let mut results: Vec<SearchResult> = Vec::new();
        Reranker::new(None).apply_freshness_boost(&mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn freshness_recent_document_boosted() {
        let reranker = Reranker::new(None); // recent_boost 1.2, 90 days
        let now = now_unix_seconds().unwrap();
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"updated_at": rfc3339_utc(now)})),
        )];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.2]);
    }

    #[test]
    fn freshness_old_document_unchanged() {
        let reranker = Reranker::new(None);
        let now = now_unix_seconds().unwrap();
        let stale = rfc3339_utc(now - 100 * SECONDS_PER_DAY);
        let mut results = vec![result(1, 0, 1.0, meta(json!({"updated_at": stale})))];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    #[test]
    fn freshness_mixed_recent_and_old() {
        let reranker = Reranker::new(None);
        let now = now_unix_seconds().unwrap();
        let mut results = vec![
            result(1, 0, 1.0, meta(json!({"updated_at": rfc3339_utc(now)}))),
            result(
                2,
                0,
                1.0,
                meta(json!({"updated_at": rfc3339_utc(now - 100 * SECONDS_PER_DAY)})),
            ),
            result(
                3,
                0,
                1.0,
                meta(json!({"updated_at": rfc3339_utc(now - 45 * SECONDS_PER_DAY)})),
            ),
        ];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.2, 1.0, 1.2]);
    }

    #[test]
    fn freshness_missing_metadata_unchanged() {
        let reranker = Reranker::new(None);
        let mut results = vec![result(1, 0, 1.0, meta(json!({})))];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    #[test]
    fn freshness_invalid_date_unchanged() {
        let reranker = Reranker::new(None);
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"updated_at": "invalid-date"})),
        )];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    // Freshness boundary: strictly after `now − recent_days` (oracle
    // `parsedTime.After(recentThreshold)`). A 60-second margin keeps the
    // wall clock from drifting across the boundary mid-test.
    #[test]
    fn freshness_boundary_around_recent_days() {
        let reranker = Reranker::new(None); // 90 days
        let threshold = now_unix_seconds().unwrap() - 90 * SECONDS_PER_DAY;
        let just_old = rfc3339_utc(threshold - 60);
        let just_recent = rfc3339_utc(threshold + 60);
        let mut results = vec![
            result(1, 0, 1.0, meta(json!({"updated_at": just_old}))),
            result(2, 0, 1.0, meta(json!({"updated_at": just_recent}))),
        ];
        reranker.apply_freshness_boost(&mut results);
        assert_scores(&results, &[1.0, 1.2]);
    }

    // ── authority (oracle TestReranker_ApplyAuthorityBoost) ──────────────

    fn authority_reranker(pairs: &[(&str, f64)]) -> Reranker {
        let mut reranker = Reranker::new(None);
        reranker.authority_boost = pairs
            .iter()
            .map(|(source_type, boost)| (source_type.to_string(), *boost))
            .collect();
        reranker
    }

    #[test]
    fn authority_empty_pool() {
        let mut results: Vec<SearchResult> = Vec::new();
        Reranker::new(None).apply_authority_boost(&mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn authority_known_type_boosted() {
        let reranker = authority_reranker(&[("policy", 1.5)]);
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"document_source_type": "policy"})),
        )];
        reranker.apply_authority_boost(&mut results);
        assert_scores(&results, &[1.5]);
    }

    #[test]
    fn authority_unknown_type_unchanged() {
        let reranker = authority_reranker(&[("policy", 1.5)]);
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"document_source_type": "unknown_type"})),
        )];
        reranker.apply_authority_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    #[test]
    fn authority_multiple_types() {
        let reranker = authority_reranker(&[("policy", 1.5), ("tutorial", 1.2), ("api", 1.0)]);
        let mut results = vec![
            result(1, 0, 1.0, meta(json!({"document_source_type": "policy"}))),
            result(2, 0, 1.0, meta(json!({"document_source_type": "tutorial"}))),
            result(3, 0, 1.0, meta(json!({"document_source_type": "api"}))),
        ];
        reranker.apply_authority_boost(&mut results);
        assert_scores(&results, &[1.5, 1.2, 1.0]);
    }

    #[test]
    fn authority_missing_metadata_unchanged() {
        let reranker = authority_reranker(&[("policy", 1.5)]);
        let mut results = vec![result(1, 0, 1.0, meta(json!({})))];
        reranker.apply_authority_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    #[test]
    fn authority_empty_map_unchanged() {
        let reranker = Reranker::new(None); // default map is empty
        let mut results = vec![result(
            1,
            0,
            1.0,
            meta(json!({"document_source_type": "policy"})),
        )];
        reranker.apply_authority_boost(&mut results);
        assert_scores(&results, &[1.0]);
    }

    // ── boost factor (oracle TestReranker_boostFactor) ───────────────────
    // The oracle's per-case setups set exactly the default boosts, so one
    // default reranker covers the whole table.

    #[test]
    fn boost_factor_table() {
        let reranker = Reranker::new(None);
        let cases = [
            ("no flags", json!({}), 1.0),
            ("deprecated", json!({"is_deprecated": true}), 0.2),
            ("official", json!({"is_official": true}), 1.5),
            ("expired", json!({"valid_to": "2020-01-01T00:00:00Z"}), 0.1),
            (
                "deprecated and official",
                json!({"is_deprecated": true, "is_official": true}),
                0.3,
            ),
            (
                "all three",
                json!({"is_deprecated": true, "is_official": true, "valid_to": "2020-01-01T00:00:00Z"}),
                0.03,
            ),
            (
                "future valid_to",
                json!({"valid_to": "2099-12-31T23:59:59Z"}),
                1.0,
            ),
            ("invalid valid_to", json!({"valid_to": "invalid"}), 1.0),
            (
                "non-bool is_deprecated ignored",
                json!({"is_deprecated": "true"}),
                1.0,
            ),
            (
                "non-bool is_official ignored",
                json!({"is_official": "true"}),
                1.0,
            ),
        ];
        for (name, metadata, expected) in cases {
            let factor = reranker.boost_factor(&meta(metadata));
            assert!(
                (factor - expected).abs() <= 1e-4,
                "{name}: factor = {factor}, want {expected}"
            );
        }
    }

    // ── enricher → reranker chain (oracle enricher_test.go, deferred from
    // task 4.3) ───────────────────────────────────────────────────────────

    fn seed_doc(db: &Db, source_type: &str, path: &str, metadata_json: Option<&str>) -> i64 {
        db.exec_tx(|tx| {
            let documents = DocumentDao::new(db::ConnectionOrTx::Transaction(&*tx));
            documents.create(source_type, path, metadata_json, None)
        })
        .expect("seed document commits")
    }

    fn seed_chunk(db: &Db, document_id: i64) -> i64 {
        db.exec_tx(|tx| {
            let chunks = ChunkDao::new(db::ConnectionOrTx::Transaction(&*tx));
            chunks.create(document_id, "chunk text", 0, None, None)
        })
        .expect("seed chunk commits")
    }

    fn set_updated_at(db: &Db, document_id: i64, value: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE documents SET updated_at = ?1 WHERE id = ?2",
                (value, document_id),
            )
        })
        .expect("connection checkout")
        .expect("updated_at set");
    }

    fn with_enricher<T>(db: &Db, f: impl FnOnce(&Enricher<'_>) -> T) -> T {
        db.with_conn(|conn| {
            let documents = DocumentDao::new(db::ConnectionOrTx::Connection(conn));
            let chunk_entities = ChunkEntityDao::new(db::ConnectionOrTx::Connection(conn));
            f(&Enricher::new(&documents, &chunk_entities))
        })
        .expect("connection checkout")
    }

    // Oracle TestEnricherReranker_OfficialBoost: a document flagged
    // is_official in metadata_json is boosted above a non-official one
    // through the full enrich → rerank chain.
    #[test]
    fn enrich_then_rerank_official_boost_chain() {
        let db = in_memory_db();
        let official_doc = seed_doc(
            &db,
            "policy",
            "/docs/official.md",
            Some(r#"{"is_official":true}"#),
        );
        let plain_doc = seed_doc(&db, "tutorial", "/docs/plain.md", Some("{}"));
        let official_chunk = seed_chunk(&db, official_doc);
        let plain_chunk = seed_chunk(&db, plain_doc);

        let mut results = vec![
            result(official_chunk, official_doc, 0.5, meta(json!({}))),
            result(plain_chunk, plain_doc, 0.7, meta(json!({}))),
        ];
        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });
        assert_eq!(results[0].metadata["is_official"], json!(true));

        let mut reranker = Reranker::new(None);
        reranker.recent_boost = 1.0; // disable freshness for this test
        reranker.rerank(&mut results);

        // official: 0.5 × 1.5 = 0.75 > 0.7 → official first, rank 1.
        assert_ids(&results, &[official_chunk, plain_chunk]);
        assert_eq!(results[0].rank, 1);
        assert_scores(&results, &[0.75, 0.7]);
    }

    // Oracle TestEnricher_NormalizesUpdatedAt: SQLite-layout updated_at is
    // normalized to RFC3339 by the enricher, so the reranker's freshness
    // boost actually fires.
    #[test]
    fn enrich_then_rerank_normalizes_updated_at() {
        let db = in_memory_db();
        let recent_doc = seed_doc(&db, "policy", "/docs/recent.md", None);
        let old_doc = seed_doc(&db, "policy", "/docs/old.md", None);
        let now = now_unix_seconds().unwrap();
        // One day ago and 100 days ago, in the SQLite CURRENT_TIMESTAMP
        // layout the enricher must normalize.
        set_updated_at(&db, recent_doc, &sqlite_layout(now - SECONDS_PER_DAY));
        set_updated_at(&db, old_doc, &sqlite_layout(now - 100 * SECONDS_PER_DAY));
        let recent_chunk = seed_chunk(&db, recent_doc);
        let old_chunk = seed_chunk(&db, old_doc);

        let mut results = vec![
            result(recent_chunk, recent_doc, 1.0, meta(json!({}))),
            result(old_chunk, old_doc, 1.0, meta(json!({}))),
        ];
        with_enricher(&db, |enricher| {
            enricher.enrich(&mut results).unwrap();
        });

        // Both updated_at values must be RFC3339 after normalization.
        for enriched in &results {
            let updated_at = enriched.metadata["updated_at"].as_str().unwrap();
            assert!(updated_at.contains('T'), "not RFC3339: {updated_at}");
            assert!(parse_epoch_seconds(updated_at).is_some());
        }

        let mut reranker = Reranker::new(None);
        reranker.deprecated_boost = 1.0;
        reranker.official_boost = 1.0;
        reranker.rerank(&mut results);

        // recent: 1.0 × 1.2 = 1.2 must outrank old: 1.0.
        assert_ids(&results, &[recent_chunk, old_chunk]);
        assert_scores(&results, &[1.2, 1.0]);
    }

    // ── time fixture helpers ─────────────────────────────────────────────
    // The fixtures render through the workspace date/time seam (no calendar
    // math in test code); the parser round-trips they used to pin now live
    // in crates/utils (change utils-crate, task 1.1).

    /// RFC3339 (UTC, `Z`) for a unix timestamp.
    fn rfc3339_utc(secs: i64) -> String {
        utils::temporal::format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs as u64)).unwrap()
    }

    /// SQLite `CURRENT_TIMESTAMP` layout for a unix timestamp (the enricher
    /// must normalize it to RFC3339).
    fn sqlite_layout(secs: i64) -> String {
        rfc3339_utc(secs)
            .replacen('T', " ", 1)
            .trim_end_matches('Z')
            .to_owned()
    }
}
