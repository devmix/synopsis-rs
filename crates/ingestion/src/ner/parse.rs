//! Pure LLM response processing for the NER provider (design D5, task 2.3).
//!
//! Oracle mapping: `parseLLMResponse` and its helpers in
//! `../synopsis/internal/ingestion/ner/llm_ner.go`. These are the parse/
//! validate rules of design D5; they are pure (no I/O) and the LlmNer
//! provider (task 2.5) composes them after each per-domain call.
//!
//! # Deliberate deviations
//!
//! - The oracle wraps its decode failure in a formatted `fmt.Errorf`; here
//!   [`parse_llm_response`] returns the [`serde_json::Error`] directly and
//!   the provider boundary (task 2.5) maps it into `IngestionError`
//!   (design D10: LLM failures are fatal at the provider).
//! - Parsed entities/facts carry an empty `domain`; the provider tags the
//!   domain after parsing (oracle parity: the oracle sets `Domain` in
//!   `ExtractEntities`, not in `parseLLMResponse`).
//! - The oracle's `validateEntityMetadata` returns `nil` for an empty result;
//!   here the result is an empty [`Map`] — the same value, owned by
//!   [`NerEntity::metadata`]/[`NerFact::metadata`].

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{NerEntity, NerFact, NerResult};

/// Confidence assigned when the model's `confidence` is absent or outside
/// `[0.0, 1.0]` (oracle `defaultLLMConfidence`).
const DEFAULT_LLM_CONFIDENCE: f64 = 0.5;
/// Description length cap in runes (oracle `maxDescLen`).
const MAX_DESCRIPTION_LEN: usize = 500;
/// The version-shape pattern (oracle `attributeVersionRe`): an optional
/// `v`/`V` prefix, digits, then dot/underscore/dash-separated alphanumerics.
static VERSION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // A static pattern that failed to compile would be a build bug.
    #[allow(clippy::expect_used)]
    Regex::new(r"^[vV]?[0-9]+([._-][a-zA-Z0-9]+)*$").expect("static version pattern compiles")
});
/// Obvious non-version patterns screened before the shape check (oracle
/// `rejectPatterns`): year ranges and hedging words.
const VERSION_REJECT_PATTERNS: &[&str] = &[
    " years",
    " year ",
    "-to-",
    "0-2",
    "1-3",
    "2-5",
    "approximately",
    "about",
    "around",
    "estimated",
];

/// The model's raw NER output (oracle `nerOutput`).
#[derive(Debug, Deserialize)]
struct NerOutput {
    /// Entity objects (JSON field `entities`).
    #[serde(default)]
    entities: Vec<RawEntity>,
    /// Relation objects (JSON field `relations`).
    #[serde(default)]
    relations: Vec<RawRelation>,
}

/// One raw entity (oracle `nerEntity`).
#[derive(Debug, Deserialize)]
struct RawEntity {
    /// Entity name; empty names are skipped.
    #[serde(default)]
    name: String,
    /// Entity type (JSON field `type`).
    #[serde(default, rename = "type")]
    entity_type: String,
    /// Confidence; absent or out of `[0.0, 1.0]` → the default.
    #[serde(default)]
    confidence: Option<f64>,
    /// Description; truncated to the cap.
    #[serde(default)]
    description: String,
    /// Attributes bag; validated before use.
    #[serde(default)]
    attributes: Map<String, Value>,
}

/// One raw relation (oracle `nerRelation`).
#[derive(Debug, Deserialize)]
struct RawRelation {
    /// Subject entity type.
    #[serde(default)]
    subject_type: String,
    /// Subject entity name.
    #[serde(default)]
    subject_name: String,
    /// Relation predicate.
    #[serde(default)]
    predicate: String,
    /// Object entity type.
    #[serde(default)]
    object_type: String,
    /// Object entity name.
    #[serde(default)]
    object_name: String,
    /// Attributes bag; validated before use.
    #[serde(default)]
    attributes: Map<String, Value>,
}

/// Parses and validates one LLM NER response (oracle `parseLLMResponse`).
///
/// Rules (design D5):
/// - entities with an empty name are skipped;
/// - confidence defaults to 0.5 when absent or outside `[0.0, 1.0]`;
/// - descriptions are truncated to at most 500 runes at the last sentence
///   boundary (`.!?;`) within the cap, else hard-capped;
/// - facts with any of the five required fields empty are skipped;
/// - metadata is validated (private `validate_metadata` rules).
///
/// Parsed entities/facts carry an empty `domain` (see the module docs).
///
/// # Errors
///
/// [`serde_json::Error`] when `raw` is not the expected JSON shape.
pub fn parse_llm_response(raw: &str) -> Result<NerResult, serde_json::Error> {
    let output: NerOutput = serde_json::from_str(raw)?;

    let entities = output
        .entities
        .into_iter()
        .filter(|entity| !entity.name.is_empty())
        .map(|entity| NerEntity {
            name: entity.name,
            entity_type: entity.entity_type,
            description: truncate_description(&entity.description),
            confidence: normalized_confidence(entity.confidence),
            domain: String::new(),
            metadata: validate_metadata(&entity.attributes),
        })
        .collect();

    let facts = output
        .relations
        .into_iter()
        .filter(|relation| {
            !relation.subject_type.is_empty()
                && !relation.subject_name.is_empty()
                && !relation.predicate.is_empty()
                && !relation.object_type.is_empty()
                && !relation.object_name.is_empty()
        })
        .map(|relation| NerFact {
            subject_type: relation.subject_type,
            subject_name: relation.subject_name,
            predicate: relation.predicate,
            object_type: relation.object_type,
            object_name: relation.object_name,
            domain: String::new(),
            metadata: validate_metadata(&relation.attributes),
        })
        .collect();

    Ok(NerResult { entities, facts })
}

/// The entity's confidence: the model value when it lies in `[0.0, 1.0]`
/// (NaN fails both bounds, like the oracle's comparisons), else the default.
fn normalized_confidence(confidence: Option<f64>) -> f64 {
    match confidence {
        Some(value) if (0.0..=1.0).contains(&value) => value,
        _ => DEFAULT_LLM_CONFIDENCE,
    }
}

/// Limits `desc` to at most [`MAX_DESCRIPTION_LEN`] runes (oracle
/// `truncateDescription`): the text is trimmed first; when it exceeds the
/// cap, it is cut after the last sentence-ending punctuation (`.!?;`) within
/// the cap (punctuation included, the cut prefix re-trimmed), else
/// hard-capped at the cap.
fn truncate_description(desc: &str) -> String {
    let trimmed = desc.trim();
    let runes: Vec<char> = trimmed.chars().collect();
    if runes.len() <= MAX_DESCRIPTION_LEN {
        return trimmed.to_owned();
    }

    let boundary = (0..MAX_DESCRIPTION_LEN)
        .rev()
        .find(|&i| matches!(runes[i], '.' | '!' | '?' | ';'));

    match boundary {
        Some(i) => runes[..i + 1].iter().collect::<String>().trim().to_owned(),
        None => runes[..MAX_DESCRIPTION_LEN].iter().collect(),
    }
}

/// Validates an LLM attributes bag (oracle `validateEntityMetadata`): string
/// values containing the LLM-uncertainty comments "implied by context" or
/// "not explicitly stated" (case-insensitive) are dropped, and a `version`
/// key (case-insensitive) must look like a real version
/// ([`is_valid_version`]) or it is dropped. Non-string values pass through.
///
/// Returns a cleaned copy (empty when nothing survives — the oracle's nil).
fn validate_metadata(attributes: &Map<String, Value>) -> Map<String, Value> {
    let mut cleaned = Map::new();
    for (key, value) in attributes {
        if let Some(text) = value.as_str() {
            let lower = text.to_lowercase();
            if lower.contains("implied by context") || lower.contains("not explicitly stated") {
                continue;
            }
            if key.eq_ignore_ascii_case("version") && !is_valid_version(text) {
                continue;
            }
        }
        cleaned.insert(key.clone(), value.clone());
    }
    cleaned
}

/// Checks whether `value` looks like a version identifier (oracle
/// `isValidVersion`): reject-list screening first, then the version-shape
/// pattern. Accepts `1`, `v2.3.1`, `1.0-beta`, `2_5`, …; rejects year
/// ranges, dates and hedged strings.
fn is_valid_version(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    if VERSION_REJECT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return false;
    }
    VERSION_PATTERN.is_match(trimmed)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (the fixtures are valid).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    // ── truncate_description (oracle TestTruncateDescription*) ────────────

    #[test]
    fn short_description_is_unchanged() {
        let input = "Alice is a software engineer.";
        assert_eq!(truncate_description(input), input);
    }

    #[test]
    fn empty_and_whitespace_only_descriptions_stay_empty() {
        assert_eq!(truncate_description(""), "");
        assert_eq!(truncate_description("   "), "");
    }

    /// Oracle `long_description_truncated_at_sentence`: a 3-sentence
    /// description over the cap is cut at the last boundary within it.
    #[test]
    fn long_description_is_truncated_at_sentence_boundary() {
        let input = "This is the first sentence. This is the second sentence that goes on and on. \
                     This is the third sentence with more content to make it longer than five \
                     hundred characters so we can test truncation properly at a sentence boundary.";
        let result = truncate_description(input);

        assert!(
            result.chars().count() <= 500,
            "got {} runes",
            result.chars().count()
        );
        assert!(result.ends_with('.'), "{result}");
    }

    /// Oracle `TestTruncateDescription_Exactly500`: exactly 500 runes stay,
    /// 501 without a boundary is hard-capped.
    #[test]
    fn exactly_500_stays_and_501_is_hard_capped() {
        let exactly_500 = "a".repeat(499) + "b";
        assert_eq!(truncate_description(&exactly_500), exactly_500);
        let result = truncate_description(&("a".repeat(500) + "b"));
        assert_eq!(result.chars().count(), 500);
    }

    /// Oracle `TestTruncateDescription_SentenceBoundary`: the cut happens at
    /// the first boundary (51 runes), not at the cap.
    #[test]
    fn sentence_boundary_before_limit_wins() {
        let input = format!("{} . {}.", "word ".repeat(10), "word ".repeat(100));
        let result = truncate_description(&input);

        assert!(
            result.chars().count() < 500,
            "got {} runes",
            result.chars().count()
        );
        assert!(result.ends_with('.'), "{result}");
    }

    /// Oracle `TestTruncateDescription_HardCap_NoSentenceBoundary`.
    #[test]
    fn no_boundary_is_hard_capped() {
        let result = truncate_description(&"x".repeat(600));
        assert_eq!(result, "x".repeat(500));
    }

    /// The cap is rune-aware, not byte-aware (Cyrillic is 2 bytes per rune).
    #[test]
    fn cap_counts_runes_not_bytes() {
        let result = truncate_description(&"я".repeat(600));
        assert_eq!(result.chars().count(), 500);
    }

    /// The oracle's trailing-whitespace check: no result carries trailing
    /// whitespace.
    #[test]
    fn results_have_no_trailing_whitespace() {
        for input in [
            "  padded text  ".to_owned(),
            format!("word . {}", "x".repeat(600)),
            format!("word. {}", "x".repeat(600)),
        ] {
            let result = truncate_description(&input);
            assert!(
                result == result.trim_end(),
                "trailing whitespace in: {result:?}"
            );
        }
    }

    // ── confidence (design D5) ─────────────────────────────────────────────

    #[test]
    fn confidence_defaults_outside_and_absent_values() {
        assert_eq!(normalized_confidence(None), 0.5);
        assert_eq!(normalized_confidence(Some(0.9)), 0.9);
        assert_eq!(normalized_confidence(Some(0.0)), 0.0);
        assert_eq!(normalized_confidence(Some(1.0)), 1.0);
        assert_eq!(normalized_confidence(Some(1.5)), 0.5);
        assert_eq!(normalized_confidence(Some(-0.2)), 0.5);
        assert_eq!(normalized_confidence(Some(f64::NAN)), 0.5);
    }

    // ── validate_metadata (oracle TestValidateEntityMetadata*) ─────────────

    #[test]
    fn empty_metadata_stays_empty() {
        assert!(validate_metadata(&Map::new()).is_empty());
    }

    #[test]
    fn clean_metadata_passes_through() {
        let metadata = validate_metadata(
            json!({
                "provider": "internal",
                "score": 0.95
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata.get("provider"), Some(&json!("internal")));
        assert_eq!(metadata.get("score"), Some(&json!(0.95)));
    }

    /// Oracle `drops_uncertainty_comment_implied` / `not_explicitly`, plus
    /// the case-insensitive matching.
    #[test]
    fn uncertainty_comments_are_dropped_case_insensitively() {
        let metadata = validate_metadata(
            json!({
                "status": "Implied By Context",
                "note": "value NOT explicitly stated in the text",
                "provider": "internal"
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata.get("provider"), Some(&json!("internal")));
    }

    /// Oracle `keeps_non_string_values`.
    #[test]
    fn non_string_values_pass_through() {
        let metadata = validate_metadata(
            json!({
                "count": 42,
                "flag": true,
                "nested": {"version": "whatever"}
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(metadata.len(), 3);
    }

    /// The uncertainty check does not apply to non-string values.
    #[test]
    fn uncertainty_check_ignores_non_strings() {
        let metadata = validate_metadata(json!({"count": 20, "ratio": 0.5}).as_object().unwrap());
        assert_eq!(metadata.len(), 2);
    }

    /// Oracle `TestValidateEntityMetadata_VersionField`: valid versions
    /// survive, junk is dropped, other keys always survive.
    #[test]
    fn version_field_is_validated() {
        for version in [
            "1", "2.3", "v1", "V2", "10", "1.0", "v2.3.1", "1-beta", "2_5",
        ] {
            let metadata = validate_metadata(
                json!({"version": version, "provider": "internal"})
                    .as_object()
                    .unwrap(),
            );
            assert_eq!(
                metadata.get("version"),
                Some(&json!(version)),
                "version {version:?} should be kept"
            );
            assert_eq!(metadata.get("provider"), Some(&json!("internal")));
        }

        for version in [
            "0-2 years",
            "approximately 3",
            "about 2.0",
            "estimated version",
            "latest",
            "unknown",
            "",
        ] {
            let metadata = validate_metadata(
                json!({"version": version, "provider": "internal"})
                    .as_object()
                    .unwrap(),
            );
            assert!(
                metadata.get("version").is_none(),
                "version {version:?} should be dropped"
            );
            assert_eq!(metadata.get("provider"), Some(&json!("internal")));
        }
    }

    /// The version check is case-insensitive on the key.
    #[test]
    fn version_key_is_case_insensitive() {
        let metadata = validate_metadata(json!({"Version": "1.0"}).as_object().unwrap());
        assert_eq!(metadata.get("Version"), Some(&json!("1.0")));

        let dropped = validate_metadata(json!({"VERSION": "garbage"}).as_object().unwrap());
        assert!(dropped.is_empty());
    }

    /// Oracle `TestIsValidVersion` (direct cases).
    #[test]
    fn is_valid_version_table() {
        for valid in [
            "1", "2.3", "v1", "V2", "10", "1.0", "v2.3.1", "1-beta", "2_5",
        ] {
            assert!(is_valid_version(valid), "{valid:?} should be valid");
        }
        for invalid in [
            "",
            "   ",
            "0-2 years",
            "approximately 3",
            "about 2.0",
            "estimated version",
            "abc",
            "1..0",
            "-1",
            "v",
        ] {
            assert!(!is_valid_version(invalid), "{invalid:?} should be invalid");
        }
    }

    // ── parse_llm_response (oracle TestParseLLMResponse*) ──────────────────

    /// Oracle `valid_response`: counts, the default confidence (the
    /// `confidence` key in attributes is metadata, not the entity field),
    /// the metadata pass-through, and the empty domain.
    #[test]
    fn valid_response_parses_entities_and_facts() {
        let result = parse_llm_response(
            r#"{
                "entities": [
                    {"name": "Alice", "type": "PERSON", "description": "Software engineer at Acme Corp.", "attributes": {"confidence": 0.9}}
                ],
                "relations": [
                    {"subject_name": "Alice", "subject_type": "PERSON", "predicate": "works_at", "object_name": "Acme Corp", "object_type": "ORGANIZATION", "attributes": {}}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.facts.len(), 1);

        let entity = &result.entities[0];
        assert_eq!(entity.name, "Alice");
        assert_eq!(entity.entity_type, "PERSON");
        assert_eq!(entity.description, "Software engineer at Acme Corp.");
        assert_eq!(entity.confidence, 0.5, "absent confidence → default");
        assert_eq!(
            entity.metadata.get("confidence"),
            Some(&json!(0.9)),
            "attributes pass through as metadata"
        );
        assert!(entity.domain.is_empty(), "the provider tags the domain");

        let fact = &result.facts[0];
        assert_eq!(fact.subject_name, "Alice");
        assert_eq!(fact.predicate, "works_at");
        assert_eq!(fact.object_name, "Acme Corp");
        assert!(fact.domain.is_empty());
    }

    /// Oracle `skips_empty_entity_names`.
    #[test]
    fn empty_entity_names_are_skipped() {
        let result = parse_llm_response(
            r#"{"entities": [
                    {"name": "", "type": "PERSON"},
                    {"name": "Bob", "type": "PERSON"}
                ], "relations": []}"#,
        )
        .unwrap();
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "Bob");
    }

    /// One fact JSON object with `field` emptied (`""` for a complete fact).
    fn fact_json(field: &str) -> String {
        let value = |name: &str, complete: &str| -> String {
            if field == name {
                String::new()
            } else {
                complete.to_owned()
            }
        };
        format!(
            r#"{{"subject_name": "{}", "subject_type": "{}", "predicate": "{}", "object_type": "{}", "object_name": "{}"}}"#,
            value("subject_name", "Alice"),
            value("subject_type", "PERSON"),
            value("predicate", "works_at"),
            value("object_type", "ORG"),
            value("object_name", "Acme"),
        )
    }

    /// Oracle `skips_invalid_facts`, extended to every required field.
    #[test]
    fn facts_with_any_empty_required_field_are_skipped() {
        let raw = format!(
            r#"{{"entities": [], "relations": [{}, {}, {}, {}, {}]}}"#,
            fact_json("subject_name"),
            fact_json("subject_type"),
            fact_json("predicate"),
            fact_json("object_type"),
            fact_json("object_name"),
        );

        let result = parse_llm_response(&raw).unwrap();
        assert!(
            result.facts.is_empty(),
            "every fact has one empty required field: {raw}"
        );

        // A complete fact survives.
        let raw = format!(r#"{{"entities": [], "relations": [{}]}}"#, fact_json(""));
        assert_eq!(parse_llm_response(&raw).unwrap().facts.len(), 1);
    }

    /// Oracle `truncates_long_description`.
    #[test]
    fn long_descriptions_are_truncated_in_parse() {
        let raw = format!(
            r#"{{"entities": [
                {{"name": "Alice", "type": "PERSON", "description": "{}. End.", "attributes": {{}}}}
            ], "relations": []}}"#,
            "word ".repeat(100)
        );
        let result = parse_llm_response(&raw).unwrap();
        assert!(
            result.entities[0].description.chars().count() <= 500,
            "got {} runes",
            result.entities[0].description.chars().count()
        );
    }

    /// Oracle `cleans_uncertainty_metadata`.
    #[test]
    fn uncertainty_metadata_is_cleaned_in_parse() {
        let result = parse_llm_response(
            r#"{"entities": [
                {"name": "Alice", "type": "PERSON", "description": "", "attributes": {"status": "implied by context", "provider": "internal"}}
            ], "relations": []}"#,
        )
        .unwrap();

        let metadata = &result.entities[0].metadata;
        assert!(metadata.get("status").is_none());
        assert_eq!(metadata.get("provider"), Some(&json!("internal")));
    }

    /// A top-level confidence in range is kept verbatim.
    #[test]
    fn in_range_confidence_is_kept() {
        let result = parse_llm_response(
            r#"{"entities": [{"name": "Bob", "type": "PERSON", "confidence": 0.75}], "relations": []}"#,
        )
        .unwrap();
        assert_eq!(result.entities[0].confidence, 0.75);
    }

    /// A top-level confidence outside the range falls back to the default.
    #[test]
    fn out_of_range_confidence_falls_back_to_default() {
        for confidence in [1.5, -0.4] {
            let raw = format!(
                r#"{{"entities": [{{"name": "Bob", "type": "PERSON", "confidence": {confidence}}}], "relations": []}}"#
            );
            let result = parse_llm_response(&raw).unwrap();
            assert_eq!(result.entities[0].confidence, 0.5);
        }
    }

    /// Missing top-level arrays parse to an empty result (serde defaults).
    #[test]
    fn missing_arrays_parse_to_empty_result() {
        let result = parse_llm_response("{}").unwrap();
        assert!(result.entities.is_empty());
        assert!(result.facts.is_empty());
    }

    /// Oracle `TestParseLLMResponse_InvalidJSON`.
    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_llm_response("{invalid json}").is_err());
    }
}
