//! Script-based word stemming (change `multilingual-entity-resolution`,
//! design D1).
//!
//! Shared by `ingestion` (entity-resolution tiers) and `graph` (cross-script
//! candidate generation): those crates are siblings in the dependency graph,
//! so the shared code lives in this leaf crate.
//!
//! Stemming comes from `rust-stemmers` (Snowball, pure Rust): Latin words use
//! the English (Porter) algorithm, Cyrillic words the Russian Snowball
//! algorithm, and any other script falls back to the lowercased identity.
//! The stemmers require lowercase input, so every entry point lowercases
//! first. A `Stemmer` is created per call (the algorithms are stateless
//! function pointers; per-call creation avoids a global, design D1).

use rust_stemmers::{Algorithm, Stemmer};

/// The writing script of a word, classified by its first alphabetic rune.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    /// Latin letters (the English Porter stemmer applies).
    Latin,
    /// Cyrillic letters (the Russian Snowball stemmer applies).
    Cyrillic,
    /// Any other script — or no alphabetic rune at all (identity fallback).
    Other,
}

/// Classifies `word` by its first alphabetic rune: [`Script::Latin`],
/// [`Script::Cyrillic`] or [`Script::Other`] (no alphabetic rune).
///
/// Latin covers ASCII letters, the Latin-1 Supplement letters, and the Latin
/// Extended-A/B blocks; Cyrillic covers the Cyrillic block and the Cyrillic
/// Supplement. Everything else (CJK, Greek, Arabic, …) is [`Script::Other`].
pub fn detect_script(word: &str) -> Script {
    word.chars()
        .find(|c| c.is_alphabetic())
        .map(script_of)
        .unwrap_or(Script::Other)
}

/// The script of a single alphabetic rune.
fn script_of(c: char) -> Script {
    match c {
        // Cyrillic block + Cyrillic Supplement.
        '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}' => Script::Cyrillic,
        // ASCII letters, Latin-1 Supplement letters (excluding the
        // multiplication signs U+00D7/U+00F7), Latin Extended-A and -B.
        'A'..='Z'
        | 'a'..='z'
        | '\u{00C0}'..='\u{00D6}'
        | '\u{00D8}'..='\u{00F6}'
        | '\u{00F8}'..='\u{00FF}'
        | '\u{0100}'..='\u{024F}'
        | '\u{0250}'..='\u{02AF}' => Script::Latin,
        _ => Script::Other,
    }
}

/// Stems a single word: Latin → English Porter, Cyrillic → Russian Snowball,
/// any other script → lowercased identity.
///
/// The input is lowercased before stemming (the Snowball algorithms require
/// lowercase input).
pub fn stem_word(word: &str) -> String {
    stem_lowered(&word.to_lowercase())
}

/// Stems every word of `name` and rejoins the stems with single spaces; the
/// input is lowercased before stemming. Words are split on any whitespace
/// run, so the output has no leading/trailing whitespace and exactly one
/// space between stems (empty input yields an empty string).
pub fn stem_name(name: &str) -> String {
    let lower = name.to_lowercase();
    lower
        .split_whitespace()
        .map(stem_lowered)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stems an already-lowercased word (the shared core of [`stem_word`] and
/// [`stem_name`]).
fn stem_lowered(word: &str) -> String {
    match detect_script(word) {
        Script::Latin => Stemmer::create(Algorithm::English).stem(word).into_owned(),
        Script::Cyrillic => Stemmer::create(Algorithm::Russian).stem(word).into_owned(),
        Script::Other => word.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn detect_script_by_first_alphabetic_rune() {
        assert_eq!(detect_script("gates"), Script::Latin);
        assert_eq!(detect_script("CITY"), Script::Latin);
        // é (U+00E9) is a Latin-1 Supplement letter.
        assert_eq!(detect_script("café"), Script::Latin);
        assert_eq!(detect_script("город"), Script::Cyrillic);
        assert_eq!(detect_script("ГОРОД"), Script::Cyrillic);
        // CJK and Greek are not Latin or Cyrillic.
        assert_eq!(detect_script("東京"), Script::Other);
        assert_eq!(detect_script("παράδειγμα"), Script::Other);
        // The first ALPHABETIC rune decides; leading digits are skipped.
        assert_eq!(detect_script("123abc"), Script::Latin);
        assert_eq!(detect_script("123город"), Script::Cyrillic);
        // No alphabetic rune at all.
        assert_eq!(detect_script("123"), Script::Other);
        assert_eq!(detect_script(""), Script::Other);
    }

    #[test]
    fn stem_word_english_porter_vectors() {
        // Vectors from rust-stemmers' own English test vocabulary
        // (test_data/voc_en.txt + res_en.txt, lines 11061, 10821 and 4491).
        assert_eq!(stem_word("gates"), "gate");
        assert_eq!(stem_word("fruitlessly"), "fruitless");
        // Porter step 1c: a final `y` becomes `i` when the stem has a vowel.
        assert_eq!(stem_word("city"), "citi");
        // Input is lowercased before stemming.
        assert_eq!(stem_word("Gates"), "gate");
        assert_eq!(stem_word("Fruitlessly"), "fruitless");
    }

    #[test]
    fn stem_word_russian_snowball_vectors() {
        // Vectors from rust-stemmers' own Russian test vocabulary
        // (test_data/voc_ru.txt + res_ru.txt, lines 7034–7038): a
        // prepositional/plural pair of a generic city noun — every
        // case/number form stems to the same stem.
        assert_eq!(stem_word("город"), "город");
        assert_eq!(stem_word("города"), "город");
        assert_eq!(stem_word("городе"), "город");
    }

    #[test]
    fn stem_word_other_script_is_lowercased_identity() {
        // CJK: identity (no case).
        assert_eq!(stem_word("東京"), "東京");
        // Greek: lowercased identity (uppercase is lowered, not stemmed).
        assert_eq!(stem_word("Παράδειγμα"), "παράδειγμα");
        // No alphabetic rune at all.
        assert_eq!(stem_word("123"), "123");
        assert_eq!(stem_word(""), "");
    }

    #[test]
    fn stem_name_stems_per_word_and_rejoins_with_single_spaces() {
        assert_eq!(stem_name("Fruitless Gates"), "fruitless gate");
        // Whitespace runs collapse to single spaces; leading/trailing
        // whitespace is dropped.
        assert_eq!(stem_name("  Fruitless   Gates  "), "fruitless gate");
        assert_eq!(stem_name(""), "");
        // Mixed scripts: each word is stemmed by its own script.
        assert_eq!(stem_name("город gates"), "город gate");
        // Input is lowercased before stemming.
        assert_eq!(stem_name("GATES"), "gate");
    }
}
