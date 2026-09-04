//! Integration tests for [`crate::domain`] against the D15-adapted fixtures and edge-case
//! documents written to temp dirs (task 3.2).
//!
//! The fixtures `tests/data/domains/domain_{hr,it,product}.xml` are the D15-adapted domain
//! fixtures: per design D15 revision 4 every group of repeated elements sits inside a plural
//! wrapper, so the adaptation adds wrapper lines
//! (`<entities>`, `<relations>`, `<attributes>`, `<synonyms>`, `<regex-rules>`) and changes
//! nothing else — see `tests/data/README.md` for provenance, SHA-256 and the adaptation recipe.
//! These tests assert that every document **loads** with its exact values (entities, relations,
//! extraction, confidence — criteria a) and that the validation matrix rejects the same
//! documents with the expected messages (criteria b–f; the two copy-paste "entity Predicate"
//! messages are fixed to "entity id").

// Test target: unwrap/expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::{ConfigError, load_domain_config};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/domains")
}

/// Owns a temp dir holding the given `domain.xml`; removed on drop even when a test panics. One
/// instance per edge-case document keeps parallel tests from clobbering each other.
struct TempDomain(PathBuf);

impl TempDomain {
    fn new(name: &str, document: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("synopsis-domain-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("domain.xml"), document).unwrap();
        Self(dir)
    }

    fn load(&self) -> Result<config::DomainConfig, ConfigError> {
        load_domain_config(self.0.join("domain.xml"))
    }
}

impl Drop for TempDomain {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Loads `document` and asserts it fails with a [`ConfigError::Validation`] carrying exactly the
/// expected message.
fn assert_validation_error(name: &str, document: &str, expected_message: &str) {
    let dir = TempDomain::new(name, document);
    match dir.load() {
        Err(ConfigError::Validation { message }) => assert_eq!(message, expected_message),
        other => panic!("expected Validation error with {expected_message:?}, got: {other:?}"),
    }
}

#[test]
fn fixture_hr_parses_complete_domain() {
    // Criterion (a): every block of the fixture loads with its exact values.
    let cfg =
        load_domain_config(fixture_dir().join("domain_hr.xml")).expect("domain_hr.xml must load");

    assert_eq!(cfg.name, "hr");
    assert_eq!(cfg.version, "1.0.0");
    assert_eq!(
        cfg.description,
        "HR policies, employee data, organizational structure"
    );

    // Two entities in file order; attribute names and synonyms verbatim.
    assert_eq!(cfg.entities.len(), 2);
    let salary = &cfg.entities[0];
    assert_eq!(salary.id, "salary");
    assert_eq!(salary.name, "Salary");
    assert_eq!(
        salary
            .attributes
            .iter()
            .map(|attribute| attribute.name.as_str())
            .collect::<Vec<_>>(),
        vec!["title", "amount", "currency"]
    );
    assert!(salary.attributes[0].required);
    assert_eq!(
        salary.synonyms,
        vec!["зп", "оклад", "заработная плата", "оплата труда"]
    );
    let grade = &cfg.entities[1];
    assert_eq!(grade.id, "grade");
    assert_eq!(grade.attributes.len(), 2);
    assert_eq!(grade.synonyms.len(), 3);

    // One relation with its attributes.
    assert_eq!(cfg.relations.len(), 1);
    let relation = &cfg.relations[0];
    assert_eq!(relation.predicate, "salary_of");
    assert_eq!(relation.source, "grade");
    assert_eq!(relation.target, "salary");
    assert_eq!(relation.attributes.len(), 3);

    // Empty extraction; the file's confidence values are non-zero, so the effective policy
    // returns them verbatim.
    assert!(cfg.extraction.regex_rules.is_empty());
    let effective = cfg.effective_confidence();
    assert_eq!(
        effective.auto_publish,
        cfg.confidence.auto_publish_threshold
    );
    assert!((effective.auto_publish - 0.7).abs() < f64::EPSILON);
    assert!((effective.review - 0.60).abs() < f64::EPSILON);
    assert!((effective.reject - 0.40).abs() < f64::EPSILON);
}

#[test]
fn fixture_it_parses_with_compiled_regex() {
    // Criterion (a): the only fixture with a regex rule — it loads and the pattern is compiled
    // in place (design D5).
    let cfg =
        load_domain_config(fixture_dir().join("domain_it.xml")).expect("domain_it.xml must load");

    assert_eq!(cfg.name, "it");
    assert_eq!(cfg.version, "1.0.0");
    assert_eq!(
        cfg.entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "system",
            "service",
            "server",
            "database",
            "network_component"
        ]
    );
    assert_eq!(
        cfg.relations
            .iter()
            .map(|relation| relation.predicate.as_str())
            .collect::<Vec<_>>(),
        vec![
            "runs_on",
            "uses_database",
            "provides",
            "consumes",
            "hosted_on",
            "depends_on",
            "replicates_to"
        ]
    );

    assert_eq!(cfg.extraction.regex_rules.len(), 1);
    let rule = &cfg.extraction.regex_rules[0];
    assert_eq!(rule.id, "ip_address");
    assert_eq!(rule.entity, "ip_address");
    assert!((rule.confidence - 0.95).abs() < f64::EPSILON);
    // Compiled at load time: it matches an IP literal and nothing else.
    assert!(rule.compiled.is_match("192.168.1.1"));
    assert!(!rule.compiled.is_match("no ip here"));
    assert!(!rule.compiled.is_match("12345"));

    let effective = cfg.effective_confidence();
    assert!((effective.auto_publish - 0.7).abs() < f64::EPSILON);
    assert!((effective.review - 0.60).abs() < f64::EPSILON);
    assert!((effective.reject - 0.40).abs() < f64::EPSILON);
}

#[test]
fn fixture_product_parses_complete_domain() {
    // Criterion (a): the largest entity/relation set, empty extraction.
    let cfg = load_domain_config(fixture_dir().join("domain_product.xml"))
        .expect("domain_product.xml must load");

    assert_eq!(cfg.name, "product");
    assert_eq!(cfg.version, "1.0.0");
    assert_eq!(
        cfg.description,
        "Product management, features, sales, and customer data"
    );
    assert_eq!(
        cfg.entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "product",
            "feature",
            "customer",
            "competitor",
            "market_segment"
        ]
    );
    assert_eq!(
        cfg.relations
            .iter()
            .map(|relation| relation.predicate.as_str())
            .collect::<Vec<_>>(),
        vec![
            "has_feature",
            "purchased_by",
            "competes_with",
            "targets_segment",
            "belongs_to_competitor"
        ]
    );
    assert!(cfg.extraction.regex_rules.is_empty());

    let effective = cfg.effective_confidence();
    assert!((effective.auto_publish - 0.7).abs() < f64::EPSILON);
    assert!((effective.review - 0.60).abs() < f64::EPSILON);
    assert!((effective.reject - 0.40).abs() < f64::EPSILON);
}

#[test]
fn missing_file_is_an_io_error_carrying_the_path() {
    // Criterion (b): no existence pre-check — the read failure carries the full path.
    let missing = fixture_dir().join("domain_missing.xml");
    match load_domain_config(&missing) {
        Err(ConfigError::Io { path, .. }) => {
            assert!(
                path.contains("domain_missing.xml"),
                "error names the file: {path}"
            )
        }
        other => panic!("expected Io error for a missing file, got: {other:?}"),
    }
}

#[test]
fn malformed_xml_is_an_xml_error_carrying_the_path() {
    // Criterion (c): an "invalid XML" document (unclosed tag) is a parse error.
    let dir = TempDomain::new(
        "malformed",
        "<domain name=\"invalid\" version=\"1.0\">\n    <entity id=\"test\" <!-- missing closing bracket -->\n</domain>",
    );
    match dir.load() {
        Err(ConfigError::Xml { path, .. }) => {
            assert!(path.contains("domain.xml"), "error names the file: {path}")
        }
        other => panic!("expected Xml error for malformed input, got: {other:?}"),
    }
}

#[test]
fn invalid_regex_pattern_is_a_typed_error_with_file_and_rule() {
    // Criterion (d): an uncompilable pattern is a typed error naming both the file and the rule
    // id.
    let dir = TempDomain::new(
        "bad_regex",
        r#"<domain name="d" version="1.0">
           <entities><entity id="e" name="E"/></entities>
           <extraction><regex-rules>
               <regex id="bad_rule" entity="e" pattern="[unclosed" confidence="0.8"/>
           </regex-rules></extraction>
           </domain>"#,
    );
    match dir.load() {
        Err(ConfigError::Regex { file, rule, .. }) => {
            assert!(file.contains("domain.xml"), "error names the file: {file}");
            assert_eq!(rule, "bad_rule");
        }
        other => panic!("expected Regex error for an uncompilable pattern, got: {other:?}"),
    }
}

#[test]
fn duplicate_entity_id_fails_validation() {
    // Criterion (e). The message names the id, not the predicate — the same fix as
    // task 3.1b's pool twin.
    assert_validation_error(
        "dup_entity",
        r#"<domain name="d" version="1.0">
           <entities>
           <entity id="product" name="Product"/>
           <entity id="product" name="Another Product"/>
           </entities>
           </domain>"#,
        "duplicate entity id: product",
    );
}

#[test]
fn relation_to_missing_entity_fails_validation() {
    // Criterion (f): endpoints are resolved against the domain's own entities (per-file
    // validation; the global pool is a fallback layer resolved in `graph`).
    assert_validation_error(
        "missing_source",
        r#"<domain name="d" version="1.0">
           <entities><entity id="product" name="Product"/></entities>
           <relations>
           <relation source="nonexistent" predicate="test_rel" target="product"/>
           </relations>
           </domain>"#,
        "relation test_rel references non-existent source entity nonexistent",
    );
    assert_validation_error(
        "missing_target",
        r#"<domain name="d" version="1.0">
           <entities><entity id="product" name="Product"/></entities>
           <relations>
           <relation source="product" predicate="test_rel" target="nonexistent"/>
           </relations>
           </domain>"#,
        "relation test_rel references non-existent target entity nonexistent",
    );
}

#[test]
fn missing_name_and_version_use_expected_messages() {
    // Absent attributes parse to "" and fail in a fixed order (name before version).
    assert_validation_error(
        "missing_name",
        "<domain version=\"1.0\"></domain>",
        "domain name is required",
    );
    assert_validation_error(
        "missing_version",
        "<domain name=\"d\"></domain>",
        "domain version is required",
    );
}

#[test]
fn ref_attribute_without_target_fails_validation() {
    assert_validation_error(
        "ref_no_target",
        r#"<domain name="d" version="1.0">
           <entities>
           <entity id="product" name="Product">
           <attributes>
           <attribute name="category" type="ref" required="true"/>
           </attributes>
           </entity>
           </entities>
           </domain>"#,
        "attribute category in entity product has type 'ref' but no target specified",
    );
}

#[test]
fn duplicate_relation_predicate_fails_validation() {
    // The expected message wording names the Predicate field.
    assert_validation_error(
        "dup_predicate",
        r#"<domain name="d" version="1.0">
           <entities>
           <entity id="product" name="Product"/>
           <entity id="category" name="Category"/>
           </entities>
           <relations>
           <relation source="product" predicate="belongs_to" target="category"/>
           <relation source="product" predicate="belongs_to" target="category"/>
           </relations>
           </domain>"#,
        "duplicate relation Predicate: belongs_to",
    );
}

#[test]
fn out_of_range_confidence_thresholds_fail_with_expected_messages() {
    let document = |auto: &str, reject: &str| {
        format!(
            r#"<domain name="d" version="1.0">
               <confidence auto_publish_threshold="{auto}" review_threshold="0.6" reject_threshold="{reject}"/>
               </domain>"#
        )
    };
    assert_validation_error(
        "high_auto",
        &document("1.5", "0.4"),
        "auto_publish_threshold must be between 0 and 1",
    );
    assert_validation_error(
        "negative_reject",
        &document("0.7", "-0.1"),
        "reject_threshold must be between 0 and 1",
    );
}
