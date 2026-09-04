//! Domain ontologies (`domains/*.xml`) loading and per-file validation (task 3.2).
//!
//! Each file describes one domain: its entity types, relation types, extraction rules and the
//! confidence policy used to auto-publish / review / reject extracted items. The document format
//! follows design D15 revision 4 — the same wrapper scheme as `global.xml`: `<domain name=
//! version= description=>` with `<entities><entity/>` (each entity carrying `<attributes>` and
//! `<synonyms>`), `<relations><relation/>` (each relation carrying `<attributes>`),
//! `<extraction><regex-rules><regex id= entity= pattern= confidence=>`, and one
//! `<confidence auto_publish_threshold= review_threshold= reject_threshold=>`.
//!
//! [`load_domain_config`] is the single entry point: read → parse → per-file validation + regex
//! compilation (design D5) in one pass — the load + validate startup sequence as one call. A
//! missing file, malformed XML, a violated invariant or an uncompilable pattern are all start
//! errors; unlike the global pool there is no "absent → empty" state — every domain file that
//! exists must load cleanly.
//!
//! Entity/relation/extraction types are shared with the global pool ([`crate::ontology`], design
//! D6: one parser model for both layers); this module adds only [`DomainConfig`] and its
//! confidence policy. Cross-layer resolution (a domain definition shadowing a pooled one) is a
//! `graph` concern, not this crate's.
//!
//! Design decisions:
//! - The copy-paste messages "entity Predicate is required" / "duplicate entity Predicate: %s"
//!   name a field `EntityDef` does not have; fixed to "entity id …", exactly like task 3.1b did
//!   for the pool's twin checks. All other validation messages keep their established wording.
//! - An uncompilable pattern is a typed [`ConfigError::Regex`] (file + rule id) — fail-fast at
//!   load time, no process crash (the fix established in task 3.1b; the shared compile step
//!   lives on [`crate::ontology::RegexRuleDef::validate_and_compile`]).
//! - Confidence defaults are not applied at load time: parsed attributes keep their raw values
//!   and zero → 0.85/0.60/0.40 is resolved on demand by
//!   [`DomainConfig::effective_confidence`] instead of mutating parsed fields.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::ontology::{AttributeType, EntityDef, ExtractionDef, RelationDef};

/// Default for `auto_publish_threshold` when the attribute is absent (zero value). Applied by
/// [`DomainConfig::effective_confidence`].
const DEFAULT_AUTO_PUBLISH_THRESHOLD: f64 = 0.85;
/// Default for `review_threshold` when the attribute is absent (zero value). See above.
const DEFAULT_REVIEW_THRESHOLD: f64 = 0.60;
/// Default for `reject_threshold` when the attribute is absent (zero value). See above.
const DEFAULT_REJECT_THRESHOLD: f64 = 0.40;

/// Builds a [`ConfigError::Validation`] from a message (same per-module helper as
/// [`crate::ontology`] and [`crate::preset`]).
fn validation(message: impl Into<String>) -> ConfigError {
    ConfigError::Validation {
        message: message.into(),
    }
}

/// One loaded domain ontology — every block of one `domains/*.xml` file (design D15 revision 4,
/// same wrapper scheme as [`crate::ontology::GlobalConfig`]). Values returned by
/// [`load_domain_config`] are fully normalized: every section validated and every regex rule
/// compiled in place. Entity/relation/extraction types come from [`crate::ontology`] — the pool
/// and domains share one model (design D6).
#[derive(Debug, Clone, Deserialize)]
pub struct DomainConfig {
    /// Domain name (`name` attribute); required — enforced at load time.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Schema version (`version` attribute); required — enforced at load time.
    #[serde(default, rename = "@version")]
    pub version: String,
    /// Human-readable description (`description` attribute).
    #[serde(default, rename = "@description")]
    pub description: String,
    /// Domain entity definitions (`<entities><entity/></entities>`); a loaded config has unique
    /// non-empty ids and validated attributes. Empty when absent.
    #[serde(default, deserialize_with = "crate::ontology::de_entities")]
    pub entities: Vec<EntityDef>,
    /// Domain relation types (`<relations><relation/></relations>`); a loaded config has unique
    /// non-empty predicates whose source/target reference this domain's own entities (checked at
    /// load time; resolution against the global pool is a `graph` concern). Empty when absent.
    #[serde(default, deserialize_with = "crate::ontology::de_relations")]
    pub relations: Vec<RelationDef>,
    /// Extraction section (`<extraction><regex-rules><regex/></regex-rules>`); in a loaded config
    /// every rule has been validated and its pattern compiled (design D5).
    #[serde(default)]
    pub extraction: ExtractionDef,
    /// Confidence thresholds as written in the file; absent attributes stay `0.0` — the
    /// documented defaults are resolved on demand by [`DomainConfig::effective_confidence`], not
    /// here.
    #[serde(default)]
    pub confidence: ConfidencePolicy,
}

/// Raw `<confidence>` element of a domain file (`auto_publish_threshold`, `review_threshold`,
/// `reject_threshold` attributes). Values are kept exactly as parsed — an absent attribute is
/// the XML zero value `0.0`; [`DomainConfig::effective_confidence`] maps zeros to the
/// documented defaults (same split as the raw-field + on-demand-resolution pair).
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct ConfidencePolicy {
    /// Auto-publish threshold (`auto_publish_threshold` attribute); must lie in `[0, 1]` —
    /// checked at load time.
    #[serde(default, rename = "@auto_publish_threshold")]
    pub auto_publish_threshold: f64,
    /// Review threshold (`review_threshold` attribute); must lie in `[0, 1]` — checked at load
    /// time.
    #[serde(default, rename = "@review_threshold")]
    pub review_threshold: f64,
    /// Reject threshold (`reject_threshold` attribute); must lie in `[0, 1]` — checked at load
    /// time.
    #[serde(default, rename = "@reject_threshold")]
    pub reject_threshold: f64,
}

/// Confidence thresholds with the documented defaults applied to zero values. All three lie in
/// `[0, 1]`: either a validated file value or one of the defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveConfidence {
    /// Threshold above which extracted items are published without review (default 0.85).
    pub auto_publish: f64,
    /// Threshold below `auto_publish` at which items go to manual review (default 0.60).
    pub review: f64,
    /// Threshold below which extracted items are rejected outright (default 0.40).
    pub reject: f64,
}

impl DomainConfig {
    /// Effective confidence thresholds with the documented defaults applied to zero values: an
    /// absent attribute parses to `0.0`, so "absent" and explicit `"0"` both resolve to the
    /// default (zero → default). The parsed [`confidence`](Self::confidence) fields are never
    /// mutated — the defaults are applied at use time (the NER pipeline), not at load time.
    pub fn effective_confidence(&self) -> EffectiveConfidence {
        EffectiveConfidence {
            auto_publish: if self.confidence.auto_publish_threshold == 0.0 {
                DEFAULT_AUTO_PUBLISH_THRESHOLD
            } else {
                self.confidence.auto_publish_threshold
            },
            review: if self.confidence.review_threshold == 0.0 {
                DEFAULT_REVIEW_THRESHOLD
            } else {
                self.confidence.review_threshold
            },
            reject: if self.confidence.reject_threshold == 0.0 {
                DEFAULT_REJECT_THRESHOLD
            } else {
                self.confidence.reject_threshold
            },
        }
    }

    /// Validates every section in a fixed error precedence and compiles the regex rules in place
    /// (design D5): name/version presence, entity pool (ids, attribute names, ref targets),
    /// relations (predicates, endpoints existing among **this domain's** entities, relation
    /// attributes), extraction rules via
    /// [`RegexRuleDef::validate_and_compile`](crate::ontology::RegexRuleDef::validate_and_compile)
    /// with the shared messages, then confidence threshold ranges. `file` names the document in
    /// [`ConfigError::Regex`].
    fn validate(&mut self, file: &str) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(validation("domain name is required"));
        }
        if self.version.is_empty() {
            return Err(validation("domain version is required"));
        }

        let mut entity_ids = HashSet::new();
        for entity in &self.entities {
            // Message bug fixed (task-mandated): the wording said "entity Predicate is
            // required" — a copy-paste from the relation check; `EntityDef` has no such field.
            if entity.id.is_empty() {
                return Err(validation("entity id is required"));
            }
            // Same copy-paste bug class in the duplicate message, fixed consistently.
            if !entity_ids.insert(entity.id.as_str()) {
                return Err(validation(format!("duplicate entity id: {}", entity.id)));
            }

            let mut attribute_names = HashSet::new();
            for attribute in &entity.attributes {
                if attribute.name.is_empty() {
                    return Err(validation(format!(
                        "attribute name is required for entity {}",
                        entity.id
                    )));
                }
                if !attribute_names.insert(attribute.name.as_str()) {
                    return Err(validation(format!(
                        "duplicate attribute name {} in entity {}",
                        attribute.name, entity.id
                    )));
                }
                // The check compares the raw word `ref`; the tolerant enum's case-sensitive
                // derived match yields `Ref` for exactly that spelling.
                if attribute.attr_type == AttributeType::Ref && attribute.target.is_empty() {
                    return Err(validation(format!(
                        "attribute {} in entity {} has type 'ref' but no target specified",
                        attribute.name, entity.id
                    )));
                }
            }
        }

        let mut predicates = HashSet::new();
        for relation in &self.relations {
            // Message kept verbatim: the check is on the predicate while the wording says
            // "name" — the predicate is the relation's identifier, so this is wording, not a bug.
            if relation.predicate.is_empty() {
                return Err(validation("relation name is required"));
            }
            if !predicates.insert(relation.predicate.as_str()) {
                return Err(validation(format!(
                    "duplicate relation Predicate: {}",
                    relation.predicate
                )));
            }
            if relation.source.is_empty() {
                return Err(validation(format!(
                    "relation {} has no source entity specified",
                    relation.predicate
                )));
            }
            if relation.target.is_empty() {
                return Err(validation(format!(
                    "relation {} has no target entity specified",
                    relation.predicate
                )));
            }
            // Endpoints must exist among this domain's entities (per-file validation; the global
            // pool is a fallback layer resolved in `graph`, design D6).
            if !entity_ids.contains(relation.source.as_str()) {
                return Err(validation(format!(
                    "relation {} references non-existent source entity {}",
                    relation.predicate, relation.source
                )));
            }
            if !entity_ids.contains(relation.target.as_str()) {
                return Err(validation(format!(
                    "relation {} references non-existent target entity {}",
                    relation.predicate, relation.target
                )));
            }

            let mut attribute_names = HashSet::new();
            for attribute in &relation.attributes {
                if attribute.name.is_empty() {
                    return Err(validation(format!(
                        "attribute name is required for relation {}",
                        relation.predicate
                    )));
                }
                if !attribute_names.insert(attribute.name.as_str()) {
                    return Err(validation(format!(
                        "duplicate attribute name {} in relation {}",
                        attribute.name, relation.predicate
                    )));
                }
            }
        }

        for rule in &mut self.extraction.regex_rules {
            rule.validate_and_compile(file)?;
        }

        if !(0.0..=1.0).contains(&self.confidence.auto_publish_threshold) {
            return Err(validation("auto_publish_threshold must be between 0 and 1"));
        }
        if !(0.0..=1.0).contains(&self.confidence.review_threshold) {
            return Err(validation("review_threshold must be between 0 and 1"));
        }
        if !(0.0..=1.0).contains(&self.confidence.reject_threshold) {
            return Err(validation("reject_threshold must be between 0 and 1"));
        }

        Ok(())
    }
}

// ── Loader ─────────────────────────────────────────────────────────────────

/// Loads a domain ontology from `path`: parse → per-file validation + regex compilation (design
/// D5) in one pass — the load + validate startup sequence as a single entry point. A missing
/// file is [`ConfigError::Io`] carrying the path; malformed XML or an unexpected document shape
/// is [`Xml`](ConfigError::Xml); a violated invariant is [`Validation`](ConfigError::Validation);
/// an uncompilable pattern is [`Regex`](ConfigError::Regex) naming both the file and the rule id.
/// The returned config is fully normalized: validated, every rule compiled; confidence
/// thresholds still raw (resolve them through [`DomainConfig::effective_confidence`]).
pub fn load_domain_config(path: impl AsRef<Path>) -> Result<DomainConfig, ConfigError> {
    let path = path.as_ref();
    let mut config = crate::io_util::read_xml_file::<DomainConfig>(path)?;
    config.validate(&crate::io_util::display_path(path))?;
    Ok(config)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (documents always parse as written).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Parses an in-memory document through the exact deserializer of `load_domain_config` minus
    /// file I/O and validation.
    fn parse(xml: &str) -> Result<DomainConfig, ConfigError> {
        quick_xml::de::from_str::<DomainConfig>(xml).map_err(|source| ConfigError::Xml {
            path: "test.xml".to_string(),
            source,
        })
    }

    #[test]
    fn minimal_document_parses_to_empty_domain() {
        let cfg = parse(r#"<domain name="x" version="1.0"></domain>"#).unwrap();
        assert_eq!(cfg.name, "x");
        assert_eq!(cfg.version, "1.0");
        assert!(cfg.description.is_empty());
        assert!(cfg.entities.is_empty());
        assert!(cfg.relations.is_empty());
        assert!(cfg.extraction.regex_rules.is_empty());
        // Absent <confidence> → all-zero raw values; defaults are resolved on demand, not here.
        assert_eq!(cfg.confidence, ConfidencePolicy::default());
    }

    #[test]
    fn confidence_policy_parses_from_attributes_and_tolerates_empty_element() {
        let cfg = parse(
            r#"<domain name="x" version="1.0">
               <confidence auto_publish_threshold="0.9" review_threshold="0.7"/>
               </domain>"#,
        )
        .unwrap();
        assert!((cfg.confidence.auto_publish_threshold - 0.9).abs() < f64::EPSILON);
        assert!((cfg.confidence.review_threshold - 0.7).abs() < f64::EPSILON);
        // Absent attribute stays the XML zero value.
        assert_eq!(cfg.confidence.reject_threshold, 0.0);

        let cfg = parse(r#"<domain name="x" version="1.0"><confidence/></domain>"#).unwrap();
        assert_eq!(cfg.confidence, ConfidencePolicy::default());
    }

    #[test]
    fn extraction_ignores_unknown_children() {
        // A document with extra <method>, <dictionary> and <llm> children in <extraction> plus
        // an extra `attribute` attribute on the rule: the deserializer ignores unknown children,
        // so the wrapper-format spelling of that same document loads with just the rule.
        let cfg = parse(
            r#"<domain name="product" version="1.0">
               <extraction>
                   <method>regex</method>
                   <dictionary name="keywords"><keyword>laptop</keyword></dictionary>
                   <llm enabled="true" model="gpt-4"/>
                   <regex-rules>
                       <regex id="price_rule" entity="product" attribute="price"
                              pattern="\$\d+\.\d{2}" confidence="0.9"/>
                   </regex-rules>
               </extraction>
               </domain>"#,
        )
        .unwrap();
        assert_eq!(cfg.extraction.regex_rules.len(), 1);
        let rule = &cfg.extraction.regex_rules[0];
        assert_eq!(rule.id, "price_rule");
        assert_eq!(rule.entity, "product");
    }

    #[test]
    fn effective_confidence_maps_zero_values_to_defaults() {
        // All-zero policy → 0.85 / 0.60 / 0.40.
        let cfg = parse(r#"<domain name="x" version="1.0"><confidence/></domain>"#).unwrap();
        assert_eq!(
            cfg.effective_confidence(),
            EffectiveConfidence {
                auto_publish: 0.85,
                review: 0.60,
                reject: 0.40,
            }
        );

        // Custom values are used verbatim; a partial policy fills only the zeros.
        let cfg = parse(
            r#"<domain name="x" version="1.0">
               <confidence auto_publish_threshold="0.95"/>
               </domain>"#,
        )
        .unwrap();
        assert_eq!(
            cfg.effective_confidence(),
            EffectiveConfidence {
                auto_publish: 0.95,
                review: 0.60,
                reject: 0.40,
            }
        );

        // Non-zero file values survive untouched; the parsed fields are never mutated.
        let cfg = parse(
            r#"<domain name="x" version="1.0">
               <confidence auto_publish_threshold="0.9" review_threshold="0.7" reject_threshold="0.5"/>
               </domain>"#,
        )
        .unwrap();
        assert_eq!(cfg.confidence.auto_publish_threshold, 0.9);
        assert_eq!(
            cfg.effective_confidence(),
            EffectiveConfidence {
                auto_publish: 0.9,
                review: 0.7,
                reject: 0.5,
            }
        );
    }

    #[test]
    fn loader_rejects_missing_file_with_the_path() {
        // Criterion (b) via the real loader: no existence pre-check — the read failure carries
        // the full path in ConfigError::Io. The pid keeps the probe name unique per process.
        let missing = std::env::temp_dir().join(format!(
            "synopsis-domain-missing-{}.xml",
            std::process::id()
        ));
        match load_domain_config(&missing) {
            Err(ConfigError::Io { path, .. }) => {
                assert!(
                    path.contains(&format!(
                        "synopsis-domain-missing-{}.xml",
                        std::process::id()
                    )),
                    "error names the file: {path}"
                )
            }
            other => panic!("expected Io error for a missing file, got: {other:?}"),
        }
    }

    #[test]
    fn loader_validates_and_compiles_in_place() {
        // The loader is read → parse → validate → compile; a bad pattern never escapes as an
        // uncompiled placeholder.
        let dir =
            std::env::temp_dir().join(format!("synopsis-domain-compile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("domain.xml");
        std::fs::write(
            &file,
            r#"<domain name="d" version="1.0">
               <entities><entity id="e" name="E"/></entities>
               <extraction><regex-rules>
                   <regex id="r" entity="e" pattern="[a-z]+" confidence="0.8"/>
               </regex-rules></extraction>
               </domain>"#,
        )
        .unwrap();

        let cfg = load_domain_config(&file).expect("valid domain must load");
        assert!(cfg.extraction.regex_rules[0].compiled.is_match("abc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
