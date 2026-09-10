//! Integration tests for the `utils::text` module (change
//! `multilingual-entity-resolution`): normalization, article-stripping, and
//! tier-key functions (task 3.1), plus script detection and stemming
//! (task 1.1).
//!
//! Generic words only (NDA): no subject-matter entity names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use utils::text::{
    Script, detect_script, match_key, normalize, stem_key, stem_name, stem_word, strip_articles,
};

#[test]
fn normalize_trims_lowercases_and_collapses_whitespace() {
    let cases = [
        ("  Apple Inc.  ", "apple inc."),
        ("Стив    Джобс", "стив джобс"),
        ("GOOGLE", "google"),
        ("   ", ""),
        ("already normalized", "already normalized"),
    ];
    for (input, want) in cases {
        assert_eq!(normalize(input), want, "normalize({input:?})");
    }
}

#[test]
fn normalize_is_idempotent() {
    let once = normalize("  The   City  ");
    assert_eq!(normalize(&once), once);
}

#[test]
fn strip_articles_removes_a_leading_whole_word_article() {
    let cases = [
        ("the city of ash", "city of ash"),
        ("a city", "city"),
        ("an apple", "apple"),
        // A bare article strips to the empty string.
        ("a", ""),
        ("an", ""),
        ("the", ""),
        // No leading article: unchanged.
        ("city of ash", "city of ash"),
        ("apple", "apple"),
        // The article must be a whole word: these are untouched.
        ("theater", "theater"),
        ("android", "android"),
        // Only one leading article is stripped.
        ("the the city", "the city"),
    ];
    for (input, want) in cases {
        assert_eq!(strip_articles(input), want, "strip_articles({input:?})");
    }
}

#[test]
fn match_key_strips_a_leading_article_and_normalizes() {
    // The acceptance pair: a leading-article variant equals the bare name.
    assert_eq!(match_key("The City of Ash"), match_key("city of ash"));
    assert_eq!(match_key("The City of Ash"), "city of ash");
    // Case/whitespace normalization is part of the key.
    assert_eq!(match_key("  THE   CITY  "), "city");
}

#[test]
fn match_key_without_a_leading_article_is_unchanged() {
    // A name without a leading article is normalized but not otherwise
    // altered.
    assert_eq!(match_key("City of Ash"), "city of ash");
    assert_eq!(match_key("apple inc."), "apple inc.");
}

#[test]
fn stem_key_stems_per_word_after_article_stripping() {
    // English: case/number inflection collapses to the same stem.
    assert_eq!(stem_key("gates"), stem_key("gate"));
    assert_eq!(stem_key("gates"), "gate");
    // The leading article is stripped before stemming.
    assert_eq!(stem_key("The Gates"), stem_key("gate"));
}

#[test]
fn stem_key_is_normalized_identity_for_non_stemmed_scripts() {
    // CJK: not stemmed, so the stem key is the normalized identity.
    assert_eq!(stem_key("東京"), "東京");
    assert_eq!(stem_key("東京 塔"), "東京 塔");
    // The key still normalizes (trims) a mixed-script name.
    assert_eq!(stem_key(" 東京 "), "東京");
}

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
