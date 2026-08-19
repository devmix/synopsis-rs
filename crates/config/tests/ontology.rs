//! Integration tests for [`crate::ontology`] against the D15-adapted oracle fixture.
//!
//! The fixture `tests/data/global.xml` derives from `../synopsis/data/ontology/global.xml`: per
//! design D15 revision 4 (2026-08-19) every group of repeated elements sits inside a plural
//! wrapper, so the adaptation adds wrapper lines (`<attributes>`, `<synonyms>`, `<methods>` x2,
//! `<regex-rules>`) and changes nothing else — see `tests/data/README.md` for provenance,
//! SHA-256 and the adaptation recipe. These tests assert that the whole document parses with its
//! exact values — task 3.1 applies **no** defaults, so absent elements keep raw zero values until
//! task 3.1b layers oracle defaulting on top; criteria (a) and (e). Criterion (b) lives in
//! `src/ontology.rs` unit tests together with the rest of the parse-only behavior, driven via
//! in-memory documents.

// Test target: unwrap/expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::ConfigError;
use config::ontology::{AttributeType, LinkMethod, NerMethod, SourceType, load_global_config};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

#[test]
fn fixture_parses_complete_ontology() {
    // Criterion (a): every block of the oracle file with its exact values.
    let cfg = load_global_config(fixture_dir())
        .expect("global.xml must parse")
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
    assert_eq!(
        first.path,
        "./data/storage/edtech/documents/demo-all-in-one"
    );
    assert!(!first.disabled);
    assert!(first.space.is_empty());
    assert!(first.dataset.is_empty());
    assert_eq!(first.domains, vec!["hr".to_string()]);

    // The mediawiki source lists three domains in file order.
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

    // Methods in file order; equals.min_words read from the file (already 2). Note the fixture's
    // attribute-only spelling `<method>equals</method>`: both Go's encoding/xml and quick-xml
    // surface that single empty-valued attribute name as the element's string content, so it
    // parses to `Equals` in both parsers — kept verbatim precisely because this quirk preserves
    // parity (see also the unit test of the same name in src/ontology.rs).
    assert_eq!(
        cdl.methods,
        vec![LinkMethod::Expression, LinkMethod::Equals, LinkMethod::Llm]
    );
    assert_eq!(cdl.equals.expect("fixture defines <equals>").min_words, 2);
    // The fixture omits both threshold elements: parse-only keeps the zero values — task 3.1b
    // applies the oracle defaults (0.7 / 5) on top of this parser.
    assert_eq!(cdl.llm_confidence_threshold, 0.0);
    assert_eq!(cdl.batch_size, 0);

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

    // One regex rule; parse-only keeps the pattern as text — task 3.1b compiles it (design D5).
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
}

#[test]
fn malformed_xml_is_an_xml_error_carrying_the_file_path() {
    // Criterion (e): a syntactically invalid global.xml surfaces as ConfigError::Xml with
    // the offending path — never as Ok(None) and never as a validation error.
    let dir = std::env::temp_dir().join(format!(
        "synopsis-ontology-malformed-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Syntactically invalid (unclosed element); format-independent on purpose.
    let _ = std::fs::write(
        dir.join("global.xml"),
        b"<global><entity id=\"a\"></global>",
    );

    let err = load_global_config(&dir)
        .expect_err("malformed XML must fail, not parse to None or a config");
    match err {
        ConfigError::Xml { path, .. } => {
            assert!(path.ends_with("global.xml"), "error names the file: {path}")
        }
        other => panic!("expected Xml error, got: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
