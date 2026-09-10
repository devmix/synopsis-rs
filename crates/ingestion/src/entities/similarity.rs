//! Name-similarity primitives for entity resolution (design D8).
//!
//! All functions are pure and rune-aware, so Cyrillic (multi-byte UTF-8)
//! names score identically to ASCII.
//!
//! # Design decisions
//!
//! - [`normalize_name`] delegates to the shared normalization rule
//!   (`utils::text::normalize`) instead of re-implementing it: the NER
//!   providers and the resolution tiers all use the same rule (DRY, design D1
//!   of `multilingual-entity-resolution`).
//! - The tier keys ([`match_key`], [`stem_key`]) live in `utils::text`
//!   (shared by `ingestion` and `graph`, which are siblings) and are
//!   re-exported here; [`jaro_winkler`] and [`bigrams`] stay in this module
//!   (the JW home).
//! - [`bigrams`] represents names shorter than two runes by their
//!   *normalized* form rather than the raw (untrimmed) input, which could
//!   leak stray whitespace into block keys.
//! - [`bigrams`] returns an ordered, deduplicated `Vec` (first-seen order)
//!   rather than an unordered collection: `HashMap` iteration order is
//!   random, the ordered slice makes blocking deterministic.

/// The resolution tier keys, shared with the cross-script linker via
/// `utils::text` (design D1): re-exported here so the resolution code imports
/// them alongside [`jaro_winkler`]/[`bigrams`].
pub use utils::text::{match_key, stem_key};

/// Normalizes an entity name for matching: trims surrounding whitespace,
/// lowercases, and collapses internal whitespace runs to single spaces.
///
/// Delegates to the shared rule `utils::text::normalize`.
pub fn normalize_name(name: &str) -> String {
    utils::text::normalize(name)
}

/// Rune-aware character bigrams of a normalized name, deduplicated and in
/// first-seen order.
///
/// Names shorter than two runes are represented by their normalized form.
pub fn bigrams(name: &str) -> Vec<String> {
    let normalized = normalize_name(name);
    let runes: Vec<char> = normalized.chars().collect();
    if runes.len() < 2 {
        return vec![normalized];
    }
    let mut bigrams = Vec::with_capacity(runes.len() - 1);
    for window in runes.windows(2) {
        let bigram: String = window.iter().collect();
        if !bigrams.contains(&bigram) {
            bigrams.push(bigram);
        }
    }
    bigrams
}

/// Jaro-Winkler similarity of two names in `[0.0, 1.0]`.
///
/// Both names are normalized first, the match window is `max(len1, len2) / 2 -
/// 1` (clamped at 0), and a common prefix (up to 4 runes) earns a bonus of
/// `prefix * 0.1 * (1 - jaro)`. The whole algorithm runs over `char`s, so
/// Cyrillic names score correctly.
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a = normalize_name(a);
    let b = normalize_name(b);
    if a == b {
        return 1.0;
    }
    let r1: Vec<char> = a.chars().collect();
    let r2: Vec<char> = b.chars().collect();
    let (len1, len2) = (r1.len(), r2.len());
    if len1 == 0 || len2 == 0 {
        return 0.0;
    }

    let match_distance = (len1.max(len2) / 2).saturating_sub(1);
    let mut matched1 = vec![false; len1];
    let mut matched2 = vec![false; len2];
    let mut matches = 0usize;
    for i in 0..len1 {
        let start = i.saturating_sub(match_distance);
        let end = (i + match_distance).min(len2 - 1);
        for j in start..=end {
            if matched2[j] || r1[i] != r2[j] {
                continue;
            }
            matched1[i] = true;
            matched2[j] = true;
            matches += 1;
            break;
        }
    }
    if matches == 0 {
        return 0.0;
    }

    // Transpositions: matched runes out of relative order. The k-cursor
    // advances only over matched positions; it cannot run past `len2`
    // because every matched rune of `a` has exactly one matched partner in
    // `b`.
    let mut transpositions = 0usize;
    let mut k = 0usize;
    for i in 0..len1 {
        if !matched1[i] {
            continue;
        }
        while !matched2[k] {
            k += 1;
        }
        if r1[i] != r2[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f64;
    let jaro = (m / len1 as f64 + m / len2 as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0;

    let prefix = r1
        .iter()
        .zip(&r2)
        .take(len1.min(len2).min(4))
        .take_while(|(c1, c2)| c1 == c2)
        .count();
    jaro + prefix as f64 * 0.1 * (1.0 - jaro)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are static).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Normalization cases: padding, Cyrillic, case folding, blank input.
    #[test]
    fn normalize_name_matches_recorded_cases() {
        let cases = [
            ("  Apple Inc.  ", "apple inc."),
            ("Стив    Джобс", "стив джобс"),
            ("GOOGLE", "google"),
            ("   ", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_name(input), want, "normalize_name({input:?})");
        }
    }

    /// Bigram cases: single rune, ASCII, Cyrillic, mixed case/space.
    #[test]
    fn bigrams_match_recorded_cases() {
        let cases: [(&str, &[&str]); 4] = [
            ("ab", &["ab"]),
            ("a", &["a"]),
            ("привет", &["пр", "ри", "ив", "ве", "ет"]),
            ("AB cd", &["ab", "b ", " c", "cd"]),
        ];
        for (input, want) in cases {
            let got = bigrams(input);
            assert_eq!(got.len(), want.len(), "bigrams({input:?}) = {got:?}");
            for w in want {
                assert!(
                    got.iter().any(|g| g == w),
                    "bigrams({input:?}) missing {w:?}: {got:?}"
                );
            }
        }
    }

    /// Jaro-Winkler similarity cases (Cyrillic included).
    #[test]
    fn jaro_winkler_matches_recorded_cases() {
        const EPS: f64 = 1e-3;
        let cases: [(&str, &str, f64); 8] = [
            ("Apple", "Apple", 1.0),
            ("  APPLE  ", "apple", 1.0),
            ("", "", 1.0),
            ("apple", "", 0.0),
            ("apple", "apple inc.", 0.9),
            ("Стив Джобс", "С. Джобс", 0.873),
            ("martha", "marhta", 0.961),
            ("apple", "xyz", 0.0),
        ];
        for (a, b, want) in cases {
            let got = jaro_winkler(a, b);
            assert!(
                (got - want).abs() <= EPS,
                "jaro_winkler({a:?}, {b:?}) = {got}, want {want}"
            );
        }
    }

    /// Jaro-Winkler threshold cases.
    #[test]
    fn jaro_winkler_threshold_cases_match() {
        const THRESHOLD: f64 = 0.8;
        let cases: [(&str, &str, bool); 4] = [
            ("Apple", "Apple Inc.", true),
            ("Стив Джобс", "С. Джобс", true),
            ("Иван Иванов", "Петр Петров", false),
            ("Варя", "Вера", false),
        ];
        for (a, b, want) in cases {
            let got = jaro_winkler(a, b) >= THRESHOLD;
            assert_eq!(got, want, "jaro_winkler({a:?}, {b:?}) >= {THRESHOLD}");
        }
    }
}
