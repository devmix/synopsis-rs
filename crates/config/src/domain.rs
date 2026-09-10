//! Domain ontologies (`domains/*.xml`) loading and per-file validation (task 3.2).
//!
//! Each file describes one domain: its entity types, relation types, extraction rules and the
//! confidence policy used to auto-publish / review / reject extracted items. The document format
//! follows design D15 revision 4 — the same wrapper scheme as `global.xml`: `<domain name=
//! version= description=>` with `<entities><entity/>` (each entity carrying `<attributes>` and
//! `<synonyms>`), `<relations><relation/>` (each relation carrying `<attributes>`),
//! `<extraction><regex-rules><regex id= entity= pattern= confidence=>`, one
//! `<confidence auto_publish_threshold= review_threshold= reject_threshold=>` and an optional
//! `<aliases><alias name= canonical=/></aliases>` block (dataset alias map, task 4.3).
//!
//! [`load_domain_config`] is the single entry point: read → parse → per-file validation + regex
//! compilation (design D5) in one pass — the load + validate startup sequence as one call. A
//! missing file, malformed XML, a violated invariant or an uncompilable pattern are all start
//! errors; unlike the global pool there is no "absent → empty" state — every domain file that
//! exists must load cleanly.
//!
//! Entity/relation/extraction types are shared with the global pool ([`crate::ontology`], design
//! D6: one parser model for both layers); this module adds [`DomainConfig`], its confidence
//! policy and the two-layer merge [`effective_domain`] — the domain's definitions shadow the
//! pooled ones by entity id / relation predicate / rule id, the domain wins silently.
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
use crate::ontology::{
    AliasDef, AttributeType, EntityDef, ExtractionDef, GlobalConfig, RelationDef,
};

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
    /// load time; the global pool is merged in by [`effective_domain`]). Empty when absent.
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
    /// Dataset alias map (`<aliases><alias name= canonical=/></aliases>`); a loaded config has
    /// non-empty `name`/`canonical` values and unique `name`s (checked at load time). Empty when
    /// the block is absent. Instance-name aliases (see [`AliasDef`]) — not the type-level
    /// `<synonyms>` of an entity.
    #[serde(default, deserialize_with = "crate::ontology::de_aliases")]
    pub aliases: Vec<AliasDef>,
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
            // pool is a fallback layer merged in by [`effective_domain`], design D6).
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

        // The <aliases> block (task 4.3): the same per-file invariants as the global pool.
        AliasDef::validate_file(&self.aliases)?;

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

// ── Two-layer effective schema ─────────────────────────────────────────────

/// Appends the unshadowed pool items to a copy of the domain's own list: the
/// shadowed keys (those the domain already defines) are dropped from the pool
/// silently, and the order is domain items first, pool additions after.
fn merge_shadowed<T>(domain: Vec<T>, pool: &[T], key: impl Fn(&T) -> &str) -> Vec<T>
where
    T: Clone,
{
    // Owned keys: the set must outlive the move of `domain` into the result.
    let shadowed: HashSet<String> = domain.iter().map(|item| key(item).to_owned()).collect();
    let mut merged = domain;
    for item in pool {
        if !shadowed.contains(key(item)) {
            merged.push(item.clone());
        }
    }
    merged
}

/// The effective domain schema: the two-layer merge of one domain config with the
/// global pool (config-format spec "XML ontologies"). Every domain definition
/// shadows the pooled definition with the same key, silently — no warning:
/// entities by id, relations by predicate, extraction regex rules by rule id.
///
/// The result is a new [`DomainConfig`] whose lists are the domain's own items
/// first, then the unshadowed pool additions (pool order preserved); `name`,
/// `version`, `description` and `confidence` are taken from `domain`
/// unchanged. A pool with nothing to add yields a copy of `domain`.
#[must_use]
pub fn effective_domain(domain: &DomainConfig, pool: &GlobalConfig) -> DomainConfig {
    DomainConfig {
        name: domain.name.clone(),
        version: domain.version.clone(),
        description: domain.description.clone(),
        entities: merge_shadowed(domain.entities.clone(), &pool.entities, |entity| {
            entity.id.as_str()
        }),
        relations: merge_shadowed(domain.relations.clone(), &pool.relations, |relation| {
            relation.predicate.as_str()
        }),
        extraction: ExtractionDef {
            regex_rules: merge_shadowed(
                domain.extraction.regex_rules.clone(),
                &pool.extraction.regex_rules,
                |rule| rule.id.as_str(),
            ),
        },
        confidence: domain.confidence,
        // The alias block is dataset-wide data, not a shadowable definition: the effective
        // domain keeps its own block only. The pool's block is unioned by
        // `aliases::dataset_alias_map` — copying it in here would double-count it in the union
        // (task 4.3).
        aliases: domain.aliases.clone(),
    }
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

    // ── effective_domain (two-layer merge) ─────────────────────────────────

    /// Parses an in-memory document through the global pool's deserializer
    /// (no loader normalization: raw values, uncompiled regex placeholder).
    fn parse_global(xml: &str) -> GlobalConfig {
        quick_xml::de::from_str(xml).unwrap()
    }

    /// A domain that defines `person`, `owns` and `phone_rule` — the same keys
    /// the pool below uses for shadowing.
    fn shadowed_domain() -> DomainConfig {
        parse(
            r#"<domain name="alpha" version="1.0" description="domain alpha">
               <entities>
                 <entity id="person" name="Person" description="DOMAIN person"/>
                 <entity id="company" name="Company" description="A company"/>
               </entities>
               <relations>
                 <relation source="person" predicate="works_for" target="company" description="works for"/>
                 <relation source="person" predicate="owns" target="company" description="DOMAIN owns"/>
               </relations>
               <extraction><regex-rules>
                 <regex id="phone_rule" entity="person" pattern="\d{10}" confidence="0.8"/>
               </regex-rules></extraction>
               </domain>"#,
        )
        .unwrap()
    }

    /// A pool that shadows the domain's `person` / `owns` / `phone_rule` keys
    /// and adds `email_address`, `owns`-target and `email_rule` of its own.
    fn shadowed_pool() -> GlobalConfig {
        parse_global(
            r#"<global version="1.0">
               <entities>
                 <entity id="person" name="Person" description="POOL person"/>
                 <entity id="email_address" name="Email" description="An email address"/>
               </entities>
               <relations>
                 <relation source="person" predicate="owns" target="email_address" description="POOL owns"/>
               </relations>
               <extraction><regex-rules>
                 <regex id="phone_rule" entity="person" pattern="POOL phone" confidence="0.5"/>
                 <regex id="email_rule" entity="email_address" pattern="[a-z]+@[a-z]+\.com" confidence="0.9"/>
               </regex-rules></extraction>
               </global>"#,
        )
    }

    /// The entity ids of a domain config in list order.
    fn entity_ids(cfg: &DomainConfig) -> Vec<&str> {
        cfg.entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect()
    }

    #[test]
    fn effective_domain_entity_shadowing() {
        let merged = effective_domain(&shadowed_domain(), &shadowed_pool());
        // The shadowed pool entity is dropped; the domain's version remains.
        let persons: Vec<_> = merged
            .entities
            .iter()
            .filter(|entity| entity.id == "person")
            .collect();
        assert_eq!(persons.len(), 1, "exactly one person: {merged:?}");
        assert_eq!(
            persons[0].description, "DOMAIN person",
            "domain wins silently"
        );
        // The pool-only entity is still added.
        assert!(
            merged
                .entities
                .iter()
                .any(|entity| entity.id == "email_address"),
            "pool-only entity is added"
        );
    }

    #[test]
    fn effective_domain_relation_shadowing() {
        let merged = effective_domain(&shadowed_domain(), &shadowed_pool());
        // The shadowed pool relation is dropped; the domain's version remains.
        let owns: Vec<_> = merged
            .relations
            .iter()
            .filter(|relation| relation.predicate == "owns")
            .collect();
        assert_eq!(owns.len(), 1, "exactly one owns: {merged:?}");
        assert_eq!(owns[0].description, "DOMAIN owns", "domain wins silently");
        assert_eq!(
            owns[0].target, "company",
            "the domain's target, not the pool's"
        );
    }

    #[test]
    fn effective_domain_rule_shadowing() {
        let merged = effective_domain(&shadowed_domain(), &shadowed_pool());
        // The shadowed pool rule is dropped; the domain's pattern remains.
        let phone: Vec<_> = merged
            .extraction
            .regex_rules
            .iter()
            .filter(|rule| rule.id == "phone_rule")
            .collect();
        assert_eq!(phone.len(), 1, "exactly one phone_rule: {merged:?}");
        assert_eq!(phone[0].pattern, r"\d{10}", "the domain's pattern wins");
        // The pool-only rule is still added.
        assert!(
            merged
                .extraction
                .regex_rules
                .iter()
                .any(|rule| rule.id == "email_rule"),
            "pool-only rule is added"
        );
    }

    #[test]
    fn effective_domain_empty_pool_is_identity() {
        let domain = shadowed_domain();
        let pool = parse_global(r#"<global version="1.0"></global>"#);
        let merged = effective_domain(&domain, &pool);
        assert_eq!(merged.name, domain.name);
        assert_eq!(merged.version, domain.version);
        assert_eq!(merged.description, domain.description);
        assert_eq!(merged.confidence, domain.confidence);
        assert_eq!(entity_ids(&merged), entity_ids(&domain));
        assert_eq!(
            merged
                .relations
                .iter()
                .map(|relation| relation.predicate.as_str())
                .collect::<Vec<_>>(),
            domain
                .relations
                .iter()
                .map(|relation| relation.predicate.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            merged
                .extraction
                .regex_rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            domain
                .extraction
                .regex_rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn effective_domain_pool_only_additions() {
        let domain = parse(r#"<domain name="empty" version="1.0"></domain>"#).unwrap();
        let merged = effective_domain(&domain, &shadowed_pool());
        assert_eq!(entity_ids(&merged), vec!["person", "email_address"]);
        assert_eq!(
            merged
                .relations
                .iter()
                .map(|relation| relation.predicate.as_str())
                .collect::<Vec<_>>(),
            vec!["owns"]
        );
        assert_eq!(
            merged
                .extraction
                .regex_rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone_rule", "email_rule"]
        );
        // The scalar fields still come from the (empty) domain.
        assert_eq!(merged.name, "empty");
        assert_eq!(merged.version, "1.0");
    }

    #[test]
    fn effective_domain_order_domain_first_pool_after() {
        let merged = effective_domain(&shadowed_domain(), &shadowed_pool());
        // Domain items keep their order, pool additions follow in pool order.
        assert_eq!(
            entity_ids(&merged),
            vec!["person", "company", "email_address"]
        );
        assert_eq!(
            merged
                .relations
                .iter()
                .map(|relation| relation.predicate.as_str())
                .collect::<Vec<_>>(),
            vec!["works_for", "owns"]
        );
        // The single `owns` is the domain's (order + shadowing together).
        assert_eq!(merged.relations[1].target, "company");
        assert_eq!(
            merged
                .extraction
                .regex_rules
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            vec!["phone_rule", "email_rule"]
        );
    }
}
