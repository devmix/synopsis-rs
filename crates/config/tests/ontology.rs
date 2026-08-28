//! Integration tests for [`crate::ontology`] against the D15-adapted oracle fixture and edge-case
//! documents written to temp dirs (tasks 3.1 + 3.1b).
//!
//! The fixture `tests/data/global.xml` derives from `../synopsis/data/ontology/global.xml`: per
//! design D15 revision 4 (2026-08-19) every group of repeated elements sits inside a plural
//! wrapper, so the adaptation adds wrapper lines (`<attributes>`, `<synonyms>`, `<methods>` x2,
//! `<regex-rules>`) and changes nothing else — see `tests/data/README.md` for provenance,
//! SHA-256 and the adaptation recipe. These tests assert that the whole document **loads** with
//! its exact values after oracle defaulting: absent threshold elements carry the defaults (0.7 /
//! 5), every source has a domain, and the regex rule is compiled in place (design D5) — criteria
//! (a) and (e). Edge-case documents cover the task 3.1b validation matrix — criteria (г), (д),
//! (ж), (з) — with error messages byte-parity to `../synopsis/internal/config/global_config.go`
//! and `internal/domain/global_pool.go`. Criterion (б) lives in `src/ontology.rs` unit tests.

// Test target: unwrap/expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::ConfigError;
use config::ontology::{AttributeType, LinkMethod, NerMethod, SourceType, load_global_config};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Owns a temp ontology dir holding the given `global.xml`; removed on drop even when a test
/// panics. One instance per edge-case document keeps parallel tests from clobbering each other.
struct TempOntology(PathBuf);

impl TempOntology {
    fn new(name: &str, document: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("synopsis-ontology-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("global.xml"), document).unwrap();
        Self(dir)
    }

    fn load(&self) -> Result<Option<config::GlobalConfig>, ConfigError> {
        load_global_config(&self.0)
    }
}

impl Drop for TempOntology {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Loads `document` and asserts it fails with a [`ConfigError::Validation`] carrying exactly the
/// oracle's message.
fn assert_validation_error(name: &str, document: &str, expected_message: &str) {
    let dir = TempOntology::new(name, document);
    match dir.load() {
        Err(ConfigError::Validation { message }) => assert_eq!(message, expected_message),
        other => panic!("expected Validation error with {expected_message:?}, got: {other:?}"),
    }
}

#[test]
fn fixture_parses_complete_ontology() {
    // Criterion (a): every block of the oracle file loads with its exact values.
    let cfg = load_global_config(fixture_dir())
        .expect("global.xml must load")
        .expect("fixture directory holds a global.xml");

    assert_fixture_sources(&cfg);
    assert_fixture_cross_domain_links(&cfg);
    assert_fixture_ner_entities_relations_extraction(&cfg);
}

fn assert_fixture_sources(cfg: &config::GlobalConfig) {
    // 8 sources: markdown x4, webpages x3, mediawiki x1. No source sets disabled/space/dataset.
    assert_eq!(cfg.sources.len(), 8);
    for source in &cfg.sources[0..4] {
        assert_eq!(source.source_type, SourceType::Markdown);
    }
    for source in &cfg.sources[4..7] {
        assert_eq!(source.source_type, SourceType::Webpages);
    }
    assert_eq!(cfg.sources[7].source_type, SourceType::Mediawiki);

    let first = &cfg.sources[0];
    // Relative source paths are anchored to the ontology directory at load time, so the stored
    // value is the fixture dir joined with the file's relative path.
    assert_eq!(
        first.path,
        fixture_dir()
            .join("./data/storage/edtech/documents/demo-all-in-one")
            .to_string_lossy()
            .into_owned()
    );
    assert!(!first.disabled);
    assert!(first.space.is_empty());
    assert!(first.dataset.is_empty());
    assert_eq!(first.domains, vec!["hr".to_string()]);

    // The mediawiki source lists three domains in file order. Every fixture source declares at
    // least one <domain>, so the ["default"] fallback never fires here.
    let wiki = &cfg.sources[7];
    assert_eq!(
        wiki.domains,
        vec!["product".to_string(), "hr".to_string(), "it".to_string()]
    );
}

fn assert_fixture_cross_domain_links(cfg: &config::GlobalConfig) {
    let cdl = cfg
        .cross_domain_links
        .as_ref()
        .expect("fixture has cross-domain-links");

    // Methods in file order. Note the fixture's attribute-only spelling `<method>equals</method>`:
    // both Go's encoding/xml and quick-xml surface that single empty-valued attribute name as the
    // element's string content, so it parses to `Equals` in both parsers — kept verbatim precisely
    // because this quirk preserves parity (see also the unit test of the same name in
    // src/ontology.rs).
    assert_eq!(
        cdl.methods,
        vec![LinkMethod::Expression, LinkMethod::Equals, LinkMethod::Llm]
    );
    // Literal value from the file; it already equals the oracle default (2).
    assert_eq!(cdl.equals.expect("fixture defines <equals>").min_words, 2);
    // The fixture omits both threshold elements: the loader applies the oracle defaults on top of
    // the raw zero values (task 3.1b criterion (a)).
    assert!((cdl.llm_confidence_threshold - 0.7).abs() < f64::EPSILON);
    assert_eq!(cdl.batch_size, 5);

    let expression = cdl.expressions.first().expect("fixture has one expression");
    assert_eq!(expression.name, "same-product");
    assert_eq!(expression.priority, 80);
    // Literal value from the file (the oracle default happens to coincide here).
    assert_eq!(expression.relation_type, "same_entity");
    // `&amp;` entities decode to literal `&&`; full text matches the oracle file exactly.
    assert_eq!(
        expression.where_,
        "A.type == 'product' && A.type == B.type && A.name == B.name"
    );
}

fn assert_fixture_ner_entities_relations_extraction(cfg: &config::GlobalConfig) {
    // The fixture's <ner> block is explicit; the fallback defaulting is covered by a dedicated test.
    assert_eq!(cfg.ner.methods, vec![NerMethod::Regex, NerMethod::Llm]);

    // 5 entities in file order; the email entity has no attributes at all.
    let ids = cfg
        .entities
        .iter()
        .map(|e| e.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["employee", "department", "policy", "role", "email"]
    );

    let employee = &cfg.entities[0];
    assert_eq!(employee.attributes.len(), 6);
    let full_name = &employee.attributes[0];
    assert_eq!(full_name.name, "full_name");
    assert_eq!(full_name.attr_type, AttributeType::String);
    assert!(full_name.required);
    assert!(full_name.target.is_empty());
    let hire_date = employee
        .attributes
        .iter()
        .find(|a| a.name == "hire_date")
        .expect("employee has a hire_date attribute");
    assert_eq!(hire_date.attr_type, AttributeType::Date);

    let email = &cfg.entities[4];
    assert!(email.attributes.is_empty());
    assert_eq!(
        email.synonyms,
        vec!["mail".to_string(), "почта".to_string()]
    );

    // 6 relations in file order.
    let predicates = cfg
        .relations
        .iter()
        .map(|r| r.predicate.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        predicates,
        vec![
            "works_in",
            "owns_policy",
            "has_role",
            "belongs_to",
            "manages",
            "has_email"
        ]
    );
    let works_in = &cfg.relations[0];
    assert_eq!(works_in.source, "employee");
    assert_eq!(works_in.target, "department");
    assert_eq!(works_in.attributes.len(), 2);

    // One regex rule; the loader validated it and compiled it in place (design D5, criterion (a)).
    let rules = &cfg.extraction.regex_rules;
    assert_eq!(rules.len(), 1);
    let rule = &rules[0];
    assert_eq!(rule.id, "email");
    assert_eq!(rule.entity, "email");
    assert_eq!(
        rule.pattern,
        r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}"
    );
    assert!((rule.confidence - 0.95).abs() < f64::EPSILON);
    // The compiled form behaves like the pattern text: matches a sample address, rejects two that
    // lack the required @...TLD shape.
    assert!(rule.compiled.is_match("john.doe@example.com"));
    assert!(!rule.compiled.is_match("not-an-address"));
    assert!(!rule.compiled.is_match("user@example"));
}

#[test]
fn source_without_path_fails_validation() {
    // Criterion (г): the oracle's message, 1-based numbering.
    let doc = "<global><sources>\
               <source type=\"markdown\"><domains><domain>x</domain></domains></source>\
               </sources></global>";
    assert_validation_error("nopath", doc, "source 1: path is required");
}

#[test]
fn source_without_type_fails_validation() {
    let doc = "<global><sources><source path=\"a\"/></sources></global>";
    assert_validation_error("notype", doc, "source 1: type is required");
}

#[test]
fn invalid_regex_pattern_is_a_typed_error_with_file_and_rule() {
    // Criterion (д): the oracle panics here (`regexp.MustCompile`); the typed error names both
    // the ontology file and the offending rule id.
    let dir = TempOntology::new(
        "bad-regex",
        "<global><extraction><regex-rules>\
         <regex id=\"bad\" entity=\"e\" pattern=\"[unclosed\" confidence=\"0.5\"/>\
         </regex-rules></extraction></global>",
    );
    match dir.load() {
        Err(ConfigError::Regex { file, rule, .. }) => {
            assert!(file.ends_with("global.xml"), "error names the file: {file}");
            assert_eq!(rule, "bad", "error names the rule id");
        }
        other => panic!("expected Regex error for an invalid pattern, got: {other:?}"),
    }
}

#[test]
fn duplicate_entity_id_fails_validation() {
    // Criterion (ж): oracle message with the copy-paste bug fixed ("entity Predicate" → "entity id").
    let doc = "<global><entities>\
               <entity id=\"a\" name=\"A\"/><entity id=\"a\" name=\"B\"/>\
               </entities></global>";
    assert_validation_error("dup-entity", doc, "duplicate global entity id: a");
}

#[test]
fn empty_domain_element_yields_default_domain() {
    // Criterion (з) + parity action: Go's encoding/xml contributes nothing to []string for an
    // element without text; quick-xml yields "" — the loader filters it, so the oracle's
    // "no domains → default" fallback still fires.
    let dir = TempOntology::new(
        "empty-domain",
        "<global><sources>\
         <source path=\"a\" type=\"markdown\"><domains><domain></domain></domains></source>\
         </sources></global>",
    );
    let cfg = dir.load().expect("must load").expect("file present");
    assert_eq!(cfg.sources[0].domains, vec!["default".to_string()]);

    // A real word survives alongside the empty element.
    let dir = TempOntology::new(
        "mixed-domain",
        "<global><sources>\
         <source path=\"a\" type=\"markdown\"><domains>\
         <domain>hr</domain><domain/></domains></source>\
         </sources></global>",
    );
    let cfg = dir.load().expect("must load").expect("file present");
    assert_eq!(cfg.sources[0].domains, vec!["hr".to_string()]);
}

#[test]
fn ner_defaults_apply_when_absent_or_empty() {
    // Absent <ner> block and an empty <methods> list both parse to an empty vec; the loader fills
    // the oracle fallback in both cases (the oracle's two branches, one check).
    let dir = TempOntology::new("ner-absent", "<global></global>");
    let cfg = dir.load().expect("must load").expect("file present");
    assert_eq!(cfg.ner.methods, vec![NerMethod::Regex, NerMethod::Llm]);

    let dir = TempOntology::new(
        "ner-empty",
        "<global><ner><methods></methods></ner></global>",
    );
    let cfg = dir.load().expect("must load").expect("file present");
    assert_eq!(cfg.ner.methods, vec![NerMethod::Regex, NerMethod::Llm]);

    // An explicit non-empty list is untouched by defaulting.
    let dir = TempOntology::new(
        "ner-explicit",
        "<global><ner><methods><method>prose</method></methods></ner></global>",
    );
    let cfg = dir.load().expect("must load").expect("file present");
    assert_eq!(cfg.ner.methods, vec![NerMethod::Prose]);
}

#[test]
fn cross_domain_links_without_methods_fail_validation() {
    // Oracle message kept verbatim; membership itself fails earlier at parse (strict enums).
    let doc = "<global><cross-domain-links></cross-domain-links></global>";
    assert_validation_error(
        "cdl-empty",
        doc,
        "cross-domain-links.methods must have at least one method",
    );
}

#[test]
fn entity_without_id_uses_fixed_message() {
    // Deviation (recorded in the change report): the oracle's copy-paste bug said "global entity
    // Predicate is required"; the fixed message names what it actually checks.
    let doc = "<global><entities><entity name=\"A\"/></entities></global>";
    assert_validation_error("no-entity-id", doc, "global entity id is required");
}

#[test]
fn ref_attribute_without_target_fails_validation() {
    let doc = "<global><entities>\
               <entity id=\"a\" name=\"A\"><attributes>\
               <attribute name=\"dept\" type=\"ref\"/>\
               </attributes></entity>\
               </entities></global>";
    assert_validation_error(
        "ref-no-target",
        doc,
        "attribute dept in global entity a has type 'ref' but no target specified",
    );
}

#[test]
fn relation_endpoints_must_exist_in_the_pool() {
    let doc = "<global>\
               <entities><entity id=\"a\" name=\"A\"/></entities>\
               <relations><relation source=\"a\" predicate=\"p\" target=\"missing\"/></relations>\
               </global>";
    assert_validation_error(
        "dangling-relation",
        doc,
        "global relation p references non-existent target entity missing",
    );
}

#[test]
fn confidence_out_of_range_fails_with_oracle_message() {
    // Byte-parity with the oracle's `%f` formatting (6 decimal places).
    let dir = TempOntology::new(
        "bad-confidence",
        "<global><extraction><regex-rules>\
         <regex id=\"c\" entity=\"e\" pattern=\"x\" confidence=\"2\"/>\
         </regex-rules></extraction></global>",
    );
    match dir.load() {
        Err(ConfigError::Validation { message }) => {
            assert_eq!(
                message,
                "regex rule \"c\" confidence 2.000000 must be in [0, 1]"
            );
        }
        other => panic!("expected Validation error for the range, got: {other:?}"),
    }
}

#[test]
fn malformed_xml_is_an_xml_error_carrying_the_file_path() {
    // Criterion (e): a syntactically invalid global.xml surfaces as ConfigError::Xml with the
    // offending path — never as Ok(None) and never as a validation error.
    let dir = TempOntology::new("malformed", "<global><entity id=\"a\"></global>");

    match dir.load() {
        Err(ConfigError::Xml { path, .. }) => {
            assert!(path.ends_with("global.xml"), "error names the file: {path}")
        }
        other => panic!("expected Xml error for malformed input, got: {other:?}"),
    }
}
