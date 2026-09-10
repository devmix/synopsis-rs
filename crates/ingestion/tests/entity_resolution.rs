//! Entity-resolution tier-key scenario tests (change
//! `multilingual-entity-resolution`, task 3.1).
//!
//! These tests live in the ingestion crate (not `utils`) because they pin the
//! Jaro-Winkler value of a pair alongside its `utils::text` stem key — the JW
//! home is `ingestion`, and `utils` (a leaf) cannot depend on `ingestion`.
//!
//! Generic words only (NDA): no subject-matter entity names.

use ingestion::jaro_winkler;
use utils::text::stem_key;

/// A Russian case-variant pair the JW tier alone would miss (JW below the
/// 0.85 merge threshold) but the stem tier settles: both forms stem to the
/// same stem.
///
/// "люди" (nominative plural) and "людей" (genitive plural) are two case
/// forms of the same generic noun ("people"). Their Jaro-Winkler similarity
/// is pinned to the actual value so the scenario is stable; it sits just
/// below the 0.85 threshold — exactly the class of pair the stem tier exists
/// to catch (design D1).
#[test]
fn ru_case_variant_pair_shares_stem_key_below_jw_threshold() {
    let (a, b) = ("люди", "людей");

    // The stem tier settles the pair: equal stem keys.
    assert_eq!(stem_key(a), stem_key(b));
    assert_eq!(stem_key(a), "люд");

    // The JW tier alone would NOT merge them: the similarity is below the
    // 0.85 threshold. Pin the actual value to lock the scenario.
    let jw = jaro_winkler(a, b);
    assert!(
        jw < 0.85,
        "the pair must fall below the merge threshold for the stem tier to matter: {jw}"
    );
    assert!(
        (jw - 0.8483).abs() <= 1e-4,
        "JW of the pinned pair drifted from 0.8483: {jw}"
    );
}
