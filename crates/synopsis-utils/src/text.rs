//! Name normalization, article stripping, script-based word stemming, and the
//! entity-resolution tier keys (change `multilingual-entity-resolution`,
//! design D1).
//!
//! Shared by `ingestion` (entity-resolution tiers) and `graph` (cross-script
//! candidate generation): those crates are siblings in the dependency graph,
//! so the shared code lives in this leaf crate.
//!
//! # Normalization and tier keys
//!
//! [`normalize`] is the project's single name-normalization rule (trim +
//! lowercase + whitespace collapse), shared by the NER layer and the
//! resolution tiers. [`strip_articles`] removes a leading English article
//! (`the`/`a`/`an`) when it is a whole word. The resolution tiers key on
//! [`match_key`] (article-stripped normalized name, tier 1) and [`stem_key`]
//! (article-stripped normalized name, stemmed per word, tier 2).
//!
//! # Stemming
//!
//! Stemming comes from `rust-stemmers` (Snowball, pure Rust): Latin words use
//! the English (Porter) algorithm, Cyrillic words the Russian Snowball
//! algorithm, and any other script falls back to the lowercased identity.
//! The stemmers require lowercase input, so every entry point lowercases
//! first. A `Stemmer` is created per call (the algorithms are stateless
//! function pointers; per-call creation avoids a global, design D1).

use rust_stemmers::{Algorithm, Stemmer};

/// Normalizes a name for matching: trims surrounding whitespace, lowercases,
/// and collapses internal whitespace runs to single spaces.
///
/// This is the project's single name-normalization rule, shared by the NER
/// layer (domain tagging) and the entity-resolution tiers (design D1 of
/// `multilingual-entity-resolution`). It is idempotent: normalizing an
/// already-normalized name is a no-op.
pub fn normalize(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.to_lowercase()
}

/// Removes a leading English article (`the`, `a`, `an`) from `name` when it is
/// a whole word — followed by a space or the end of the string.
///
/// The input is expected to be normalized (lowercase); the tier-key entry
/// points ([`match_key`], [`stem_key`]) normalize first. A bare article
/// (`"a"`, `"an"`, `"the"`) is stripped to the empty string. The whole-word
/// match means `"theater"` is left untouched (its `the` is not a separate
/// word). Only one leading article is stripped.
pub fn strip_articles(name: &str) -> String {
    let trimmed = name.trim();
    for article in ["the", "an", "a"] {
        let Some(rest) = trimmed.strip_prefix(article) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with(' ') {
            return rest.trim_start().to_owned();
        }
    }
    trimmed.to_owned()
}

/// Tier-1 resolution key: the article-stripped normalized name.
///
/// `match_key(name) == strip_articles(normalize(name))`. Two names with equal
/// match keys are the same entity up to a leading article and case/whitespace
/// (e.g. `"The City of Ash"` and `"city of ash"`).
pub fn match_key(name: &str) -> String {
    strip_articles(&normalize(name))
}

/// Tier-2 resolution key: the article-stripped normalized name, stemmed per
/// word.
///
/// `stem_key(name) == stem_name(strip_articles(normalize(name)))`. Two names
/// with equal stem keys are the same entity up to case/number inflection
/// (e.g. `"gates"` and `"gate"`; Russian case variants). Non-Latin/Cyrillic
/// scripts are not stemmed, so their stem key is the normalized identity.
pub fn stem_key(name: &str) -> String {
    stem_name(&strip_articles(&normalize(name)))
}

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
