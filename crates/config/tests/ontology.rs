//! Integration tests for [`crate::ontology`] against the D15-adapted fixture and edge-case
//! documents written to temp dirs (tasks 3.1 + 3.1b).
//!
//! The fixture `tests/data/global.xml` is the D15-adapted ontology fixture: per design D15
//! revision 4 (2026-08-19) every group of repeated elements sits inside a plural wrapper, so
//! the adaptation adds wrapper lines (`<attributes>`, `<synonyms>`, `<methods>` x2,
//! `<regex-rules>`) and changes nothing else — see `tests/data/README.md` for provenance,
//! SHA-256 and the adaptation recipe. These tests assert that the whole document **loads** with
//! its exact values after defaulting: absent threshold elements carry the defaults (0.7 / 5),
//! every source has a domain, and the regex rule is compiled in place (design D5) — criteria
//! (a) and (e). Edge-case documents cover the task 3.1b validation matrix — criteria (d), (e),
//! (g), (h) — with the exact expected error messages.
//!
//! Also hosts the tests relocated from the inline `#[cfg(test)]` module in `src/ontology.rs`
//! (change `test-hygiene-phase-2`, task 2.7) — criterion (b): they exercise only the public API
//! (plus the crate's own `quick-xml` dependency for the parse-only helper), so names and
//! assertions are carried over verbatim and the move changes no behavior.

// Test target: unwrap/expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::ConfigError;
use config::ontology::{
    AttributeType, GlobalConfig, LinkMethod, NerMethod, SourceType, load_global_config,
};

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
/// expected message.
fn assert_validation_error(name: &str, document: &str, expected_message: &str) {
    let dir = TempOntology::new(name, document);
    match dir.load() {
        Err(ConfigError::Validation { message }) => assert_eq!(message, expected_message),
        other => panic!("expected Validation error with {expected_message:?}, got: {other:?}"),
    }
}

#[test]
fn fixture_parses_complete_ontology() {
    // Criterion (a): every block of the fixture loads with its exact values.
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
    // quick-xml surfaces a lone empty-valued attribute name as the element's string content, so
    // it parses to `Equals` — the spelling is kept verbatim in the fixture precisely because of
    // this quirk (see also the unit test of the same name in src/ontology.rs).
    assert_eq!(
        cdl.methods,
        vec![LinkMethod::Expression, LinkMethod::Equals, LinkMethod::Llm]
    );
    // Literal value from the file; it already equals the default (2).
    assert_eq!(cdl.equals.expect("fixture defines <equals>").min_words, 2);
    // The fixture omits both threshold elements: the loader applies the defaults on top of
    // the raw zero values (task 3.1b criterion (a)).
    assert!((cdl.llm_confidence_threshold - 0.7).abs() < f64::EPSILON);
    assert_eq!(cdl.batch_size, 5);

    let expression = cdl.expressions.first().expect("fixture has one expression");
    assert_eq!(expression.name, "same-product");
    assert_eq!(expression.priority, 80);
    // Literal value from the file (the default happens to coincide here).
    assert_eq!(expression.relation_type, "same_entity");
    // `&amp;` entities decode to literal `&&`; full text matches the fixture exactly.
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
    // Criterion (d): the expected message, 1-based numbering.
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
    // Criterion (e): an uncompilable pattern is a typed error naming both the ontology file and
    // the offending rule id.
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
    // Criterion (g): duplicate id fails with the expected message.
    let doc = "<global><entities>\
               <entity id=\"a\" name=\"A\"/><entity id=\"a\" name=\"B\"/>\
               </entities></global>";
    assert_validation_error("dup-entity", doc, "duplicate global entity id: a");
}

#[test]
fn empty_domain_element_yields_default_domain() {
    // Criterion (h): an element without text contributes no domain — quick-xml yields "" and
    // the loader filters it, so the "no domains → default" fallback still fires.
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
    // the fallback in both cases (two branches, one check).
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
    // The expected message; membership itself fails earlier at parse (strict enums).
    let doc = "<global><cross-domain-links></cross-domain-links></global>";
    assert_validation_error(
        "cdl-empty",
        doc,
        "cross-domain-links.methods must have at least one method",
    );
}

#[test]
fn entity_without_id_uses_fixed_message() {
    // The fixed message names the id, the field actually checked.
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
fn confidence_out_of_range_fails_with_expected_message() {
    // The message formats the confidence with 6 decimal places.
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

// ── Relocated from src/ontology.rs (test-hygiene-phase-2 task 2.7) ───────────

/// Parses an in-memory document through the exact deserializer of `load_global_config` minus
/// file I/O.
fn parse(xml: &str) -> Result<GlobalConfig, ConfigError> {
    quick_xml::de::from_str::<GlobalConfig>(xml).map_err(|source| ConfigError::Xml {
        path: "test.xml".to_string(),
        source,
    })
}

#[test]
fn load_with_empty_dir_yields_none() {
    assert!(load_global_config("").is_ok_and(|cfg| cfg.is_none()));
}

#[test]
fn load_with_missing_file_yields_none() {
    let dir =
        std::env::temp_dir().join(format!("synopsis-ontology-missing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(load_global_config(&dir).is_ok_and(|cfg| cfg.is_none()));
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn load_anchors_relative_source_paths_to_the_ontology_dir() {
    // Relative source paths resolve against the directory holding global.xml, not the
    // process working directory (a `..` segment is preserved verbatim in the join).
    let dir = std::env::temp_dir().join(format!("synopsis-ontology-anchor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("global.xml"),
        r#"<global><sources>
<source path="../content/documents/hr" type="markdown"/>
<source path="docs" type="markdown"/>
</sources></global>"#,
    )
    .unwrap();

    let cfg = load_global_config(&dir)
        .unwrap()
        .expect("global.xml present");
    assert_eq!(
        cfg.sources[0].path,
        dir.join("../content/documents/hr").to_string_lossy()
    );
    assert_eq!(cfg.sources[1].path, dir.join("docs").to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_keeps_absolute_source_paths_verbatim() {
    let dir =
        std::env::temp_dir().join(format!("synopsis-ontology-absolute-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let absolute = std::env::temp_dir().join("synopsis-ontology-abs-source");
    std::fs::write(
        dir.join("global.xml"),
        format!(
            r#"<global><sources><source path="{absolute}" type="markdown"/></sources></global>"#,
            absolute = absolute.display()
        ),
    )
    .unwrap();

    let cfg = load_global_config(&dir)
        .unwrap()
        .expect("global.xml present");
    assert_eq!(cfg.sources[0].path, absolute.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn minimal_document_parses_to_empty_structure() {
    // Parse-only: absent blocks stay empty — task 3.1b applies the defaults on top.
    let cfg = parse("<global></global>").unwrap();
    assert!(cfg.sources.is_empty());
    assert!(cfg.cross_domain_links.is_none(), "absent block stays None");
    assert!(cfg.ner.methods.is_empty());
    assert!(cfg.entities.is_empty());
    assert!(cfg.relations.is_empty());
    assert!(cfg.extraction.regex_rules.is_empty());

    // D15 revision 4: <expression> sits inside an <expressions> wrapper, which in turn is a
    // direct child of <cross-domain-links>.
    let cdl = parse(
        "<global><cross-domain-links>\
         <methods><method>expression</method></methods>\
         <expressions><expression><name>x</name></expression></expressions>\
         </cross-domain-links></global>",
    )
    .unwrap()
    .cross_domain_links
    .expect("block present");
    // Both threshold elements are absent: the raw zero values survive, defaults come in 3.1b.
    assert_eq!(cdl.llm_confidence_threshold, 0.0);
    assert_eq!(cdl.batch_size, 0);
    assert!(cdl.equals.is_none());
    assert_eq!(cdl.expressions[0].relation_type, "");
}

#[test]
fn source_fields_parse_to_raw_values() {
    // Parse-only: absent fields keep raw zero values; 3.1b defaults empty domains to
    // ["default"] and rejects missing path/type with the expected messages. D15 revision 4:
    // <domain> items sit inside a <domains> wrapper.
    let cfg = parse("<global><sources><source type=\"markdown\"/></sources></global>").unwrap();
    assert!(cfg.sources[0].path.is_empty());
    assert_eq!(cfg.sources[0].domains, Vec::<String>::new());

    // Absent `type` attribute → the enum's Default (Unknown("")), not a parse error.
    let cfg = parse("<global><sources><source path=\"a\"></source></sources></global>").unwrap();
    assert_eq!(cfg.sources[0].path, "a");
    assert_eq!(cfg.sources[0].source_type, SourceType::default());

    // Unknown (non-empty) types are tolerated at parse time — only presence is checked.
    let cfg = parse(
        "<global><sources><source path=\"a\" type=\"confluence\"></source></sources></global>",
    )
    .unwrap();
    assert_eq!(
        cfg.sources[0].source_type,
        SourceType::Unknown("confluence".to_string())
    );

    // Multiple <domain> items in file order.
    let cfg = parse(
        "<global><sources><source path=\"a\" type=\"markdown\">\
         <domains><domain>x</domain><domain>y</domain></domains>\
         </source></sources></global>",
    )
    .unwrap();
    assert_eq!(
        cfg.sources[0].domains,
        vec!["x".to_string(), "y".to_string()]
    );
}

#[test]
fn unknown_method_values_are_rejected_at_parse() {
    // Strict enums (design D15 revision 4): the accepted method sets are validated exactly, so
    // an unknown word is a parse error here instead of a 3.1b validation one.
    let err = parse(
        "<global><cross-domain-links>\
         <methods><method>bogus</method></methods>\
         </cross-domain-links></global>",
    )
    .expect_err("bogus link method must fail parsing");
    match &err {
        ConfigError::Xml { path, .. } => assert_eq!(path, "test.xml"),
        other => panic!("expected Xml error, got: {other:?}"),
    }
    let msg = err.to_string();
    assert!(msg.contains("\"bogus\""), "message names the word: {msg}");

    // Matching is case-sensitive: the accepted words are compared exactly, as are the
    // derived rename_all-lowercase identifiers.
    let err = parse(
        "<global><cross-domain-links>\
         <methods><method>Expression</method></methods>\
         </cross-domain-links></global>",
    )
    .expect_err("wrong-case link method must fail parsing");
    assert!(err.to_string().contains("\"Expression\""));

    // Same strictness for the NER method list.
    let err = parse("<global><ner><methods><method>spacy</method></methods></ner></global>")
        .expect_err("bogus ner method must fail parsing");
    assert!(err.to_string().contains("\"spacy\""));
}

#[test]
fn empty_method_elements_contribute_nothing() {
    // A text-less <method> element contributes nothing; the helpers drop empty items before
    // strict mapping.
    let cfg = parse(
        "<global><ner>\
         <methods><method></method><method>regex</method></methods>\
         </ner></global>",
    )
    .unwrap();
    assert_eq!(cfg.ner.methods, vec![NerMethod::Regex]);
}

#[test]
fn attribute_only_method_element_parses_as_its_attribute_name() {
    // The fixture writes the equals method as `<method>equals</method>` (an attribute, not
    // text). quick-xml surfaces that element's single empty attribute name as its string
    // value, so it parses to `Equals` — the spelling is preserved verbatim in the fixture
    // precisely because of this quirk.
    let cfg = parse(
        "<global><cross-domain-links>\
         <methods><method>expression</method><method>equals</method><method>llm</method></methods>\
         </cross-domain-links></global>",
    )
    .unwrap();
    assert_eq!(
        cfg.cross_domain_links.expect("block present").methods,
        vec![LinkMethod::Expression, LinkMethod::Equals, LinkMethod::Llm]
    );
}

#[test]
fn absent_attributes_parse_to_empty_defaults() {
    // D15 revision 4: <entity> sits inside an <entities> wrapper; its attributes and
    // synonyms sit in their own wrappers.
    let cfg = parse(
        "<global><entities>\
         <entity name=\"A\"><attributes>\
         <attribute name=\"x\" required=\"true\"></attribute>\
         </attributes></entity>\
         </entities></global>",
    )
    .unwrap();
    let entity = &cfg.entities[0];
    // Absent @id stays "" (3.1b: "global entity id is required").
    assert!(entity.id.is_empty());
    assert_eq!(entity.name, "A");
    assert!(entity.description.is_empty());
    let attribute = &entity.attributes[0];
    // Absent @type → the tolerant enum's Default (Unknown); absent @target stays "".
    assert_eq!(attribute.attr_type, AttributeType::default());
    assert!(attribute.required);
    assert!(attribute.target.is_empty());
}

#[test]
fn regex_rules_parse_without_compiling() {
    // Parse-only (3.1b compiles per D5): the pattern stays text; even an invalid pattern parses.
    let cfg = parse(
        "<global><extraction>\
         <regex-rules><regex id=\"email\" entity=\"email\" pattern=\"[unclosed\"\
         confidence=\"0.9\"></regex></regex-rules>\
         </extraction></global>",
    )
    .unwrap();
    assert_eq!(cfg.extraction.regex_rules.len(), 1);
    let rule = &cfg.extraction.regex_rules[0];
    assert_eq!(rule.id, "email");
    assert_eq!(rule.entity, "email");
    assert_eq!(rule.pattern, "[unclosed");
    assert!((rule.confidence - 0.9).abs() < f64::EPSILON);
}

#[test]
fn wrapped_attribute_and_synonym_blocks_parse_independently() {
    // D15 revision 4: <attribute> and <synonym> items live in separate wrappers, so their
    // relative order inside the entity no longer matters — each list is read from its own
    // container. Both orders below parse to the same values.
    let doc_a = "<global><entities>\
         <entity id=\"a\" name=\"A\">\
           <attributes><attribute name=\"x\" type=\"string\"></attribute></attributes>\
           <synonyms><synonym>s1</synonym><synonym>s2</synonym></synonyms>\
         </entity></entities></global>";
    let doc_b = "<global><entities>\
         <entity id=\"a\" name=\"A\">\
           <synonyms><synonym>s1</synonym><synonym>s2</synonym></synonyms>\
           <attributes><attribute name=\"x\" type=\"string\"></attribute></attributes>\
         </entity></entities></global>";

    for doc in [doc_a, doc_b] {
        let entity = &parse(doc).unwrap().entities[0];
        assert_eq!(entity.attributes.len(), 1);
        assert_eq!(entity.attributes[0].attr_type, AttributeType::String);
        assert_eq!(entity.synonyms, vec!["s1".to_string(), "s2".to_string()]);
    }
}

#[test]
fn relation_children_parse_with_attributes() {
    // D15 revision 4: <relation> sits inside a <relations> wrapper; its <attribute> items
    // keep the attribute-only shape inside an <attributes> wrapper.
    let cfg = parse(
        "<global><relations>\
         <relation source=\"a\" predicate=\"p\" target=\"b\" description=\"d\">\
           <attributes><attribute name=\"since\" type=\"date\"/></attributes>\
         </relation></relations></global>",
    )
    .unwrap();
    let relation = &cfg.relations[0];
    assert_eq!(relation.source, "a");
    assert_eq!(relation.predicate, "p");
    assert_eq!(relation.target, "b");
    assert_eq!(relation.description, "d");
    assert_eq!(relation.attributes.len(), 1);
    assert_eq!(relation.attributes[0].name, "since");
    assert_eq!(relation.attributes[0].attr_type, "date");
}

#[test]
fn attribute_types_match_known_words_and_tolerate_others() {
    // Tolerant enum via pure derive + #[serde(other)]: known words (matched exactly, in the
    // fixture's lowercase spelling) map to variants; anything else lands in Unknown.
    let cfg = parse(
        "<global><entities>\
         <entity id=\"a\" name=\"A\"><attributes>\
           <attribute name=\"x\" type=\"date\"></attribute>\
           <attribute name=\"y\" type=\"ref\" target=\"b\"></attribute>\
           <attribute name=\"z\" type=\"weird\"></attribute>\
         </attributes></entity>\
         </entities></global>",
    )
    .unwrap();
    let attributes = &cfg.entities[0].attributes;
    assert_eq!(attributes[0].attr_type, AttributeType::Date);
    assert_eq!(attributes[1].attr_type, AttributeType::Ref);
    assert!(!attributes[1].target.is_empty());
    assert_eq!(attributes[2].attr_type, AttributeType::Unknown);
}
