//! Relocated unit tests for the cross-domain entity linker (task 1.8).
//!
//! Extracted from the inline `#[cfg(test)]` module in `src/linker.rs` to
//! shrink the source file. They exercise the public `build_entity_links`
//! pipeline (the `equals` and `expression` methods) against an in-memory
//! SQLite database (`db::test_util::in_memory_db`). Oracle mapping: Go
//! `internal/relations/{entity_links.go,expression_linker.go}` (a
//! functional reference for behavior and contracts only, not a code
//! blueprint).
//!
//! The nine tests that reach private production items (the
//! `cross_domain_pairs` fn and the `MockLlm` mock server) remain inline in
//! `src/linker.rs` (design D7). The test fixtures shared with those
//! stay-inline tests are copied here (design D4), as are the oracle-parity
//! confidence/relation constants (private in `src/linker.rs`; an
//! integration test cannot reach private items).

#![allow(clippy::unwrap_used)]

use config::ontology::{CrossDomainLinksConfig, EqualsConfig, LinkExpression, LinkMethod};
use config::preset::LinkerConfig;
use db::{ConnectionOrTx, Db, EntityDao, EntityLink, EntityLinkDao};
use graph::linker::build_entity_links;

// ── fixtures ────────────────────────────────────────────────────────────────

/// A nonexistent prompts path: the `llm` method falls back to the embedded
/// templates (the normal case, design D3). Design D4: a local copy — shared
/// with the stay-inline tests in `src/linker.rs`.
const TEST_PROMPTS_PATH: &str = "/nonexistent/prompts";

/// The oracle's `config.DefaultRelationType`. Design D4: a local copy of the
/// private constant in `src/linker.rs`; keep in sync.
const DEFAULT_RELATION_TYPE: &str = "same_entity";

/// The oracle's `equalsConfidence`. Design D4: a local copy of the private
/// constant in `src/linker.rs`; keep in sync.
const EQUALS_CONFIDENCE: f64 = 0.9;

/// The oracle's `ruleConfidence`. Design D4: a local copy of the private
/// constant in `src/linker.rs`; keep in sync.
const RULE_CONFIDENCE: f64 = 1.0;

/// A links config with the given methods and (optionally) expressions.
/// Design D4: a local copy — shared with the stay-inline tests in
/// `src/linker.rs`.
fn links_config(
    methods: Vec<LinkMethod>,
    expressions: Vec<LinkExpression>,
) -> CrossDomainLinksConfig {
    CrossDomainLinksConfig {
        methods,
        equals: None,
        llm_confidence_threshold: 0.7,
        batch_size: 5,
        expressions,
    }
}

/// Insert entities (type, name, domain) and return their ids in order.
/// Design D4: a local copy — shared with the stay-inline tests in
/// `src/linker.rs`.
fn insert_entities(db: &Db, rows: &[(&str, &str, &str)]) -> Vec<i64> {
    db.with_conn(|conn| {
        let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
        rows.iter()
            .map(|(entity_type, name, domain)| {
                entities.create(entity_type, name, domain, None, None, None)
            })
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap()
    .unwrap()
}

/// The entity_links rows, in insertion (rowid) order.
/// Design D4: a local copy — shared with the stay-inline tests in
/// `src/linker.rs`.
fn all_links(db: &Db) -> Vec<EntityLink> {
    db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap()
}

#[test]
fn equals_links_matching_names_and_skips_short_or_different_names() {
    let db = db::test_util::in_memory_db();
    insert_entities(
        &db,
        &[
            ("PERSON", "Alice Smith", "hr"),
            ("PERSON", "Alice Smith", "it"),
            // One word < default min_words (2): pair exists, equals skips.
            ("PERSON", "Alice", "hr"),
            ("PERSON", "Alice", "it"),
            // Different name: no pair at all.
            ("PERSON", "Alice Jones", "it"),
        ],
    );

    let result = build_entity_links(
        &db,
        None,
        &links_config(vec![LinkMethod::Equals], Vec::new()),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(result.links_created, 1);
    assert_eq!(result.links_skipped, 0);
    assert!(result.errors.is_empty());

    let links = all_links(&db);
    assert_eq!(links.len(), 2, "one bidirectional pair");
    for link in &links {
        assert_eq!(link.method, "equals");
        assert_eq!(link.relation_type, "same_entity");
        assert_eq!(link.confidence, EQUALS_CONFIDENCE);
        assert_eq!(
            link.evidence.as_deref(),
            Some("equals: alice smith in hr and it")
        );
    }
}

#[test]
fn equals_respects_configured_min_words() {
    let db = db::test_util::in_memory_db();
    insert_entities(&db, &[("PERSON", "Alice", "hr"), ("PERSON", "Alice", "it")]);

    let config = CrossDomainLinksConfig {
        equals: Some(EqualsConfig { min_words: 1 }),
        ..links_config(vec![LinkMethod::Equals], Vec::new())
    };
    let result = build_entity_links(
        &db,
        None,
        &config,
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(result.links_created, 1);
    assert_eq!(all_links(&db).len(), 2);
}

#[test]
fn expression_rule_creates_link_with_rule_attributes() {
    let db = db::test_util::in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("PERSON", "John Doe", "hr"),
            ("PERSON", "John Doe", "it"),
            ("ORGANIZATION", "Acme", "hr"),
            // Rule-false pair: no facts.
            ("PERSON", "Jane Roe", "hr"),
            ("PERSON", "Jane Roe", "it"),
        ],
    );
    db.with_conn(|conn| {
        let facts = db::FactDao::new(ConnectionOrTx::Connection(conn));
        facts.create(
            Some(ids[0]),
            "works_at",
            Some(ids[2]),
            "hr",
            None,
            None,
            None,
        )
    })
    .unwrap()
    .unwrap();

    let rule = LinkExpression {
        name: "acme_workers".to_owned(),
        description: String::new(),
        priority: 5,
        where_: "has_fact(A.id, 'works_at', 'Acme')".to_owned(),
        relation_type: "works_at_same_org".to_owned(),
    };
    let result = build_entity_links(
        &db,
        None,
        &links_config(vec![LinkMethod::Expression], vec![rule]),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(result.links_created, 1);
    // The rule-false pair counts neither as created nor as skipped.
    assert_eq!(result.links_skipped, 0);
    assert!(result.errors.is_empty());

    let links = all_links(&db);
    assert_eq!(links.len(), 2, "one bidirectional pair");
    for link in &links {
        assert_eq!(link.method, "expression");
        assert_eq!(link.relation_type, "works_at_same_org");
        assert_eq!(link.confidence, RULE_CONFIDENCE);
        assert_eq!(link.evidence.as_deref(), Some("expression: acme_workers"));
        assert!(
            (link.subject_entity_id, link.target_entity_id) == (ids[0], ids[1])
                || (link.subject_entity_id, link.target_entity_id) == (ids[1], ids[0])
        );
    }
}

#[test]
fn expression_priority_order_and_first_true_wins() {
    // Ontology order: low (priority 1) FIRST, high (priority 10) second.
    let rules = vec![
        LinkExpression {
            name: "low".to_owned(),
            description: String::new(),
            priority: 1,
            where_: "A.domain == 'hr' || A.domain == 'it'".to_owned(),
            relation_type: "rel_low".to_owned(),
        },
        LinkExpression {
            name: "high".to_owned(),
            description: String::new(),
            priority: 10,
            where_: "A.domain == 'hr'".to_owned(),
            relation_type: "rel_high".to_owned(),
        },
    ];

    // Pair 1: A from "hr" — BOTH rules true; the higher priority must win.
    let db1 = db::test_util::in_memory_db();
    insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
    let r1 = build_entity_links(
        &db1,
        None,
        &links_config(vec![LinkMethod::Expression], rules.clone()),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(r1.links_created, 1);
    assert!(
        all_links(&db1)
            .iter()
            .all(|link| link.relation_type == "rel_high")
    );

    // Pair 2: A from "it" — high false, low true: falls back in priority
    // order.
    let db2 = db::test_util::in_memory_db();
    insert_entities(&db2, &[("PERSON", "U V", "it"), ("PERSON", "U V", "zz")]);
    let r2 = build_entity_links(
        &db2,
        None,
        &links_config(vec![LinkMethod::Expression], rules),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(r2.links_created, 1);
    assert!(
        all_links(&db2)
            .iter()
            .all(|link| link.relation_type == "rel_low")
    );
}

#[test]
fn expression_errors_are_recorded_not_fatal() {
    // A parse error fails the whole method (oracle parity).
    let db1 = db::test_util::in_memory_db();
    insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
    let bad = LinkExpression {
        name: "broken".to_owned(),
        description: String::new(),
        priority: 0,
        where_: "A.name +".to_owned(),
        relation_type: "same_entity".to_owned(),
    };
    let r1 = build_entity_links(
        &db1,
        None,
        &links_config(vec![LinkMethod::Expression], vec![bad]),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(r1.links_created, 0);
    assert!(
        r1.errors
            .iter()
            .any(|err| err.starts_with("init expression linker:"))
    );
    assert!(all_links(&db1).is_empty());

    // A non-boolean result is a per-pair error (the `cel` crate cannot
    // type-check at compile time).
    let db2 = db::test_util::in_memory_db();
    insert_entities(&db2, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
    let non_bool = LinkExpression {
        name: "string".to_owned(),
        description: String::new(),
        priority: 0,
        where_: "A.name".to_owned(),
        relation_type: "same_entity".to_owned(),
    };
    let r2 = build_entity_links(
        &db2,
        None,
        &links_config(vec![LinkMethod::Expression], vec![non_bool]),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(r2.links_created, 0);
    assert_eq!(r2.links_skipped, 1);
    assert!(
        r2.errors
            .iter()
            .any(|err| err.starts_with("expression pair ("))
    );
    assert!(all_links(&db2).is_empty());
}

#[test]
fn method_order_from_config_is_respected() {
    // The rule's relation type collides with equals' default: whichever
    // method runs first owns the row, the second one is skipped.
    let rule = LinkExpression {
        name: "dup".to_owned(),
        description: String::new(),
        priority: 0,
        where_: "A.domain == 'hr'".to_owned(),
        relation_type: DEFAULT_RELATION_TYPE.to_owned(),
    };

    let db1 = db::test_util::in_memory_db();
    insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
    let first = build_entity_links(
        &db1,
        None,
        &links_config(
            vec![LinkMethod::Equals, LinkMethod::Expression],
            vec![rule.clone()],
        ),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(first.links_created, 1);
    assert_eq!(first.links_skipped, 1);
    assert!(all_links(&db1).iter().all(|link| link.method == "equals"));

    let db2 = db::test_util::in_memory_db();
    insert_entities(&db2, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
    let second = build_entity_links(
        &db2,
        None,
        &links_config(vec![LinkMethod::Expression, LinkMethod::Equals], vec![rule]),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(second.links_created, 1);
    assert_eq!(second.links_skipped, 1);
    assert!(
        all_links(&db2)
            .iter()
            .all(|link| link.method == "expression")
    );
}

#[test]
fn self_link_never_created() {
    // Same normalized domain (case/whitespace variants): no pair at all.
    let db = db::test_util::in_memory_db();
    insert_entities(
        &db,
        &[
            ("PERSON", "Alice Smith", "HR"),
            ("PERSON", "Alice Smith", " hr "),
            // Same domain, different type: still no pair.
            ("ORGANIZATION", "Alice Smith", "hr"),
        ],
    );
    let result = build_entity_links(
        &db,
        None,
        &links_config(vec![LinkMethod::Equals], Vec::new()),
        &LinkerConfig::default(),
        TEST_PROMPTS_PATH,
    )
    .unwrap();
    assert_eq!(result.links_created, 0);
    assert_eq!(result.links_skipped, 0);
    assert!(all_links(&db).is_empty());

    // The DAO rejects an explicit self-link as well (schema CHECK +
    // DAO pre-check).
    let id = db
        .with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            entities.create("PERSON", "Solo", "hr", None, None, None)
        })
        .unwrap()
        .unwrap();
    let created = db
        .with_conn(|conn| {
            let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
            links.create(&EntityLink {
                subject_entity_id: id,
                target_entity_id: id,
                relation_type: "same_entity".to_owned(),
                method: "equals".to_owned(),
                confidence: 0.9,
                evidence: None,
            })
        })
        .unwrap()
        .unwrap();
    assert!(!created);
}
