//! Rule-based NER provider (design D3).
//!
//! [`RegexNer`] flattens the extraction rules of the domain configs into
//! prepared rules at construction. Patterns are already compiled by the
//! config loader (config design D5: `ValidateAndCompile` at load time), so
//! the constructor cannot fail — pattern compilation errors are reported at
//! config load time, not here.
//!
//! Extraction semantics:
//!
//! - capture group 1 wins over the full match when the pattern has groups;
//!   a non-participating group 1 yields no entity (an empty group 1);
//! - names are trimmed, empty names skipped;
//! - dedup by `(name, entity_type, domain)` — a tuple key, not a `"|"`-joined
//!   string (which would collide when a name contains `|`);
//! - every entity is stamped with the matching rule's id under the
//!   `"rule_id"` metadata key;
//! - empty content, zero rules or zero matches → `Ok(None)` (design D2).

use std::collections::HashSet;

use config::DomainConfig;
use regex::Regex;
use serde_json::{Map, Value};

use super::{NerEntity, NerProvider, NerResult};
use crate::error::IngestionError;

/// A prepared extraction rule: one `(domain, rule)` pair flattened at
/// construction (design D3). Cloning the compiled pattern is cheap — the
/// `regex` crate shares the compiled program behind an `Arc`.
struct PreparedRule {
    /// Lowercase entity type the matches are attributed to.
    entity_type: String,
    /// Normalized domain name the rule belongs to.
    domain: String,
    /// Rule id, stamped into every entity's metadata.
    rule_id: String,
    /// Confidence assigned to every match of this rule.
    confidence: f64,
    /// Compiled pattern (pre-compiled by the config loader).
    pattern: Regex,
}

/// Rule-based NER provider over the domain configs' regex extraction rules.
///
/// Construction flattens all rules of all domains in config order and reuses
/// the patterns as compiled by
/// [`config::load_domain_config`](config::load_domain_config) — no
/// recompilation here (design D3).
pub struct RegexNer {
    /// Prepared rules in config order.
    rules: Vec<PreparedRule>,
}

impl RegexNer {
    /// Builds the provider from the domain configs, flattening each config's
    /// extraction rules into prepared rules (design D3).
    ///
    /// Cannot fail: the config loader already validated and compiled every
    /// pattern (config design D5). Domain names are normalized (trim +
    /// lowercase + collapse whitespace, [`normalize`]) and entity types
    /// lowercased.
    pub fn new(domain_configs: &[DomainConfig]) -> Self {
        let mut rules = Vec::new();
        for config in domain_configs {
            let domain = normalize(&config.name);
            for rule in &config.extraction.regex_rules {
                rules.push(PreparedRule {
                    entity_type: rule.entity.to_lowercase(),
                    domain: domain.clone(),
                    rule_id: rule.id.clone(),
                    confidence: rule.confidence,
                    pattern: Regex::clone(&rule.compiled),
                });
            }
        }
        Self { rules }
    }
}

impl NerProvider for RegexNer {
    fn name(&self) -> &'static str {
        "regex"
    }

    fn extract_entities(
        &self,
        content: &str,
        _metadata: &Map<String, Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        // The rule engine is pure and synchronous (design D2).
        if content.trim().is_empty() || self.rules.is_empty() {
            return Ok(None);
        }

        let mut seen = HashSet::new();
        let mut entities = Vec::new();

        for rule in &self.rules {
            // Capture group 1 wins when the pattern has groups
            // (`captures_len() > 1` — index 0 is the full match).
            let prefer_capture = rule.pattern.captures_len() > 1;
            for captures in rule.pattern.captures_iter(content) {
                let group = if prefer_capture {
                    captures.get(1)
                } else {
                    captures.get(0)
                };
                let name = group
                    .map(|match_| match_.as_str().trim())
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                // Dedup by (name, type, domain); a tuple key, not a
                // "|"-joined string (see the module docs).
                if !seen.insert((name.clone(), rule.entity_type.clone(), rule.domain.clone())) {
                    continue;
                }

                let mut metadata = Map::new();
                metadata.insert("rule_id".to_string(), Value::String(rule.rule_id.clone()));

                entities.push(NerEntity {
                    name,
                    entity_type: rule.entity_type.clone(),
                    description: String::new(),
                    confidence: rule.confidence,
                    domain: rule.domain.clone(),
                    metadata,
                });
            }
        }

        if entities.is_empty() {
            Ok(None)
        } else {
            Ok(Some(NerResult {
                entities,
                facts: Vec::new(),
            }))
        }
    }
}

/// Trims, lowercases and collapses internal whitespace runs to single
/// spaces. Crate-private: `LlmNer` reuses it for domain tagging
/// (ingestion-ner task 2.5) instead of re-implementing the rule.
///
/// Delegates to the shared normalization rule `utils::text::normalize`
/// (design D1 of `multilingual-entity-resolution`): one canonical
/// implementation, reused by the NER providers and the resolution tiers.
pub(crate) fn normalize(text: &str) -> String {
    utils::text::normalize(text)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::load_domain_config;

    use super::*;

    /// Test pattern with a capture group (email local part).
    const EMAIL_CAPTURE: &str = r"([a-zA-Z0-9._%+\-]+)@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}";
    /// Test pattern without a capture group (full email).
    const EMAIL_FULL: &str = r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}";

    /// Writes a one-domain XML with the given rules and loads it through the
    /// real config loader — the only public way to obtain compiled patterns
    /// (config design D5 compiles at load time).
    fn load_domain(name: &str, rules: &[(&str, &str, &str, f64)]) -> DomainConfig {
        let dir =
            std::env::temp_dir().join(format!("synopsis-ner-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rules_xml = String::new();
        for (id, entity, pattern, confidence) in rules {
            rules_xml.push_str(&format!(
                r#"<regex id="{id}" entity="{entity}" pattern="{pattern}" confidence="{confidence}"/>"#
            ));
        }
        let xml = format!(
            r#"<domain name="{name}" version="1.0"><extraction><regex-rules>{rules_xml}</regex-rules></extraction></domain>"#
        );
        let path = dir.join("domain.xml");
        std::fs::write(&path, xml).unwrap();
        let config = load_domain_config(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        config
    }

    /// Capture group extracts the first group instead of the full match;
    /// rule id, confidence and domain are stamped on the entity.
    #[test]
    fn capture_group_wins_over_full_match() {
        let ner = RegexNer::new(&[load_domain(
            "capture",
            &[("entity_from_email", "employee", EMAIL_CAPTURE, 0.9)],
        )]);

        let result = ner
            .extract_entities(
                "Contact nikolay.morozov@example.com for details.",
                &Map::new(),
            )
            .unwrap()
            .expect("entities found");

        assert_eq!(result.entities.len(), 1);
        let entity = &result.entities[0];
        assert_eq!(entity.name, "nikolay.morozov");
        assert_eq!(entity.entity_type, "employee");
        assert_eq!(entity.domain, "capture");
        assert_eq!(entity.confidence, 0.9);
        assert!(entity.description.is_empty());
        assert_eq!(
            entity.metadata.get("rule_id"),
            Some(&Value::String("entity_from_email".to_string()))
        );
        assert!(result.facts.is_empty());
    }

    /// Capture group with a simple username.
    #[test]
    fn capture_group_simple_username() {
        let ner = RegexNer::new(&[load_domain(
            "capture2",
            &[("entity_from_email", "employee", EMAIL_CAPTURE, 0.9)],
        )]);

        let result = ner
            .extract_entities("Email john@company.org for support.", &Map::new())
            .unwrap()
            .expect("entities found");

        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "john");
    }

    /// Multiple matches keep their order of appearance.
    #[test]
    fn capture_group_multiple_matches_in_order() {
        let ner = RegexNer::new(&[load_domain(
            "capture3",
            &[("entity_from_email", "employee", EMAIL_CAPTURE, 0.9)],
        )]);

        let result = ner
            .extract_entities("Reach alice@corp.com or bob.smith@corp.com.", &Map::new())
            .unwrap()
            .expect("entities found");

        assert_eq!(
            result
                .entities
                .iter()
                .map(|entity| entity.name.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob.smith"]
        );
    }

    /// Without a capture group the full match is used.
    #[test]
    fn no_capture_group_uses_full_match() {
        let ner = RegexNer::new(&[load_domain(
            "full",
            &[("full_email_match", "email", EMAIL_FULL, 0.9)],
        )]);

        let result = ner
            .extract_entities("Send to test@example.com please.", &Map::new())
            .unwrap()
            .expect("entities found");

        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "test@example.com");
        assert_eq!(result.entities[0].entity_type, "email");
    }

    /// A capture-group pattern with no match yields nothing — `Ok(None)`, not
    /// an empty result.
    #[test]
    fn no_match_yields_none() {
        let ner = RegexNer::new(&[load_domain(
            "nomatch",
            &[(
                "specific_domain",
                "employee",
                r"([a-zA-Z]+)@example\.com",
                0.9,
            )],
        )]);

        let result = ner
            .extract_entities("No emails here, just text.", &Map::new())
            .unwrap();
        assert_eq!(result, None);
    }

    /// Design D3: empty or whitespace-only content → `Ok(None)`.
    #[test]
    fn empty_content_yields_none() {
        let ner = RegexNer::new(&[load_domain("empty", &[("r1", "concept", r"\btest\b", 0.8)])]);

        assert_eq!(ner.extract_entities("", &Map::new()).unwrap(), None);
        assert_eq!(ner.extract_entities("  \n\t ", &Map::new()).unwrap(), None);
    }

    /// Design D3: zero rules → `Ok(None)` even for non-empty content.
    #[test]
    fn zero_rules_yields_none() {
        let ner = RegexNer::new(&[load_domain("norules", &[])]);
        assert_eq!(
            ner.extract_entities("this is a test string", &Map::new())
                .unwrap(),
            None
        );
    }

    /// Design D3: dedup by (name, type, domain) — a repeated match yields one
    /// entity; the same name under a different type is kept.
    #[test]
    fn dedup_by_name_type_and_domain() {
        let ner = RegexNer::new(&[load_domain(
            "dedup",
            &[
                ("r1", "employee", EMAIL_CAPTURE, 0.9),
                ("r2", "contact", EMAIL_CAPTURE, 0.8),
            ],
        )]);

        let content = "Ping alice@corp.com and again alice@corp.com.";
        let result = ner
            .extract_entities(content, &Map::new())
            .unwrap()
            .expect("entities found");

        // Two rules × one unique name: (alice, employee) + (alice, contact).
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.entities[0].entity_type, "employee");
        assert_eq!(result.entities[0].confidence, 0.9);
        assert_eq!(result.entities[1].entity_type, "contact");
        assert_eq!(result.entities[1].confidence, 0.8);
    }

    /// Design D3: multi-domain — the same rule in two domains keeps both
    /// entities (different domain in the dedup key), and domain names are
    /// normalized (trim + lowercase + collapse whitespace).
    #[test]
    fn multi_domain_keeps_per_domain_entities() {
        let alpha = load_domain("alpha", &[("r1", "employee", EMAIL_CAPTURE, 0.9)]);
        let beta = load_domain("  Beta ", &[("r1", "employee", EMAIL_CAPTURE, 0.9)]);
        let ner = RegexNer::new(&[alpha, beta]);

        let result = ner
            .extract_entities("Ping alice@corp.com.", &Map::new())
            .unwrap()
            .expect("entities found");

        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.entities[0].domain, "alpha");
        assert_eq!(result.entities[1].domain, "beta");
    }

    /// Entity types are lowercased.
    #[test]
    fn entity_type_is_lowercased() {
        let ner = RegexNer::new(&[load_domain(
            "case",
            &[("r1", "EMPLOYEE", EMAIL_CAPTURE, 0.9)],
        )]);

        let result = ner
            .extract_entities("Ping alice@corp.com.", &Map::new())
            .unwrap()
            .expect("entities found");
        assert_eq!(result.entities[0].entity_type, "employee");
    }

    /// With capture groups, a non-participating group 1 yields an empty name
    /// and is skipped.
    #[test]
    fn non_participating_capture_group_is_skipped() {
        let ner = RegexNer::new(&[load_domain(
            "alt",
            &[("r1", "concept", r"(foo)|(bar)", 0.9)],
        )]);

        // Only group 2 participates; group 1 is empty → no entity.
        assert_eq!(
            ner.extract_entities("bar only here", &Map::new()).unwrap(),
            None
        );
        // Group 1 participates → the entity is captured.
        let result = ner
            .extract_entities("foo is here", &Map::new())
            .unwrap()
            .expect("entities found");
        assert_eq!(result.entities[0].name, "foo");
    }

    /// Design D2: the provider is object-safe (`Box<dyn NerProvider>`) and
    /// reports its stable name.
    #[test]
    fn provider_is_object_safe_and_named() {
        let provider: Box<dyn NerProvider> = Box::new(RegexNer::new(&[]));
        assert_eq!(provider.name(), "regex");
        assert_eq!(
            provider
                .extract_entities("anything goes", &Map::new())
                .unwrap(),
            None
        );
    }
}
