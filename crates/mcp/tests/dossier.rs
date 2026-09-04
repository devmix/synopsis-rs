//! Integration tests for the `get_entity_dossier` tool (extracted from
//! `src/tools/dossier.rs` by test-hygiene-phase-2 task 2.5). The 16 tests
//! previously lived in the inline `#[cfg(test)] mod tests` module; they now
//! run against the public API (`mcp::tools::dossier::handle_get_entity_dossier`,
//! `mcp::McpError`) plus the public seams of the `db`, `graph` and `config`
//! crates (in-memory fixture db, ready graph index).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use config::preset::GraphConfig;
use db::{
    ConnectionOrTx, DocumentDao, EntityDao, EntityLink, EntityLinkDao, EntitySourceDao, FactDao,
    FactSourceDao, test_util,
};
use graph::GraphIndex;
use mcp::error::McpError;
use mcp::tools::dossier::handle_get_entity_dossier;
use serde_json::Value;

/// Seed an in-memory KB in one transaction; returns the db and the value
/// the seeder produced (usually the created ids).
fn seed_db<T>(seed: impl FnOnce(ConnectionOrTx<'_>) -> Result<T, db::DbError>) -> (db::Db, T) {
    let db = test_util::in_memory_db();
    let value = db
        .exec_tx(|tx| seed(ConnectionOrTx::Transaction(&*tx)))
        .expect("seed transaction commits");
    (db, value)
}

/// A ready graph index over the db (config default: enabled + loaded).
fn ready_graph(db: &db::Db) -> GraphIndex {
    GraphIndex::from_db(db, &GraphConfig::default()).expect("graph builds")
}

fn call(db: &db::Db, graph: &GraphIndex, args: Option<Value>) -> Result<Value, McpError> {
    handle_get_entity_dossier(db, graph, args.as_ref())
}

/// The dossier fixture: Alice (hr) `works_at` Acme (hr) + a fact source +
/// a linked document.
fn seeded_dossier_db() -> (db::Db, (i64, i64, i64)) {
    seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let facts = FactDao::new(exec);
        let sources = FactSourceDao::new(exec);
        let documents = DocumentDao::new(exec);
        let entity_sources = EntitySourceDao::new(exec);
        let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let acme = entities.create("ORGANIZATION", "Acme Corp", "hr", None, None, None)?;
        // The document must exist before the fact source references it
        // (`fact_sources.document_id` is a foreign key).
        let doc_id = documents.create("markdown", "/docs/hr.md", None, None)?;
        let fact_id = facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
        let quote = "Alice works at Acme Corp";
        sources.create(fact_id, doc_id, Some(quote), None)?;
        entity_sources.create(alice, doc_id)?;
        Ok((alice, acme, doc_id))
    })
}

// ── resolution ───────────────────────────────────────────────────────────

/// An empty/missing entity id returns an error: the XOR is enforced.
#[test]
fn resolution_requires_exactly_one_of_id_or_name() {
    let (db, _) = seeded_dossier_db();
    let graph = GraphIndex::Unavailable;
    for args in [
        None,
        Some(serde_json::json!({})),
        Some(serde_json::json!({ "entity_id": "" })),
        Some(serde_json::json!({ "entity_id": "1", "entity_name": "Alice" })),
    ] {
        let err = call(&db, &graph, args).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
    }
}

/// A non-integer entity id returns an error.
#[test]
fn resolution_non_integer_id_is_an_error() {
    let (db, _) = seeded_dossier_db();
    let graph = GraphIndex::Unavailable;
    let err = call(&db, &graph, Some(serde_json::json!({ "entity_id": "abc" }))).unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("must be an integer"), "got: {err}");
}

/// A nonexistent entity id returns an error.
#[test]
fn resolution_nonexistent_id_is_not_found() {
    let (db, _) = seeded_dossier_db();
    let graph = GraphIndex::Unavailable;
    let err = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": "99999" })),
    )
    .unwrap_err();
    assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
    assert!(
        err.to_string().contains("entity with id 99999 not found"),
        "got: {err}"
    );
}

/// Two same-named entities in different domains.
#[test]
fn resolution_domain_disambiguates_and_lists_candidates() {
    let (db, ids) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let id_product = entities.create("PERSON", "Alice", "product", None, None, None)?;
        let unique = entities.create("PERSON", "UniqueBob", "hr", None, None, None)?;
        Ok((id_hr, id_product, unique))
    });
    let graph = GraphIndex::Unavailable;

    // A domain narrows to one entity (case-insensitive).
    let by_hr = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
    )
    .unwrap();
    assert_eq!(by_hr["entity"]["id"], ids.0);

    let by_product = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
    )
    .unwrap();
    assert_eq!(by_product["entity"]["id"], ids.1);

    // No domain + multiple matches lists the candidates.
    let ambiguous = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "Alice" })),
    )
    .unwrap_err();
    assert!(
        matches!(ambiguous, McpError::InvalidArguments { .. }),
        "got: {ambiguous:?}"
    );
    assert!(
        ambiguous.to_string().contains("multiple entities match"),
        "got: {ambiguous}"
    );

    // No domain + a single match succeeds.
    let single = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "UniqueBob" })),
    )
    .unwrap();
    assert_eq!(single["entity"]["id"], ids.2);
}

// ── response shape ───────────────────────────────────────────────────────

/// A valid entity returns a dossier with facts and sources: entity
/// fields, a fact, and a source.
#[test]
fn dossier_returns_entity_facts_and_sources() {
    let (db, (alice, _acme, doc_id)) = seeded_dossier_db();
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();

    assert_eq!(response["entity"]["id"], alice);
    assert_eq!(response["entity"]["name"], "Alice");
    assert_eq!(response["entity"]["type"], "PERSON");
    assert_eq!(response["entity"]["domain"], "hr");

    let facts = response["facts"].as_array().unwrap();
    assert_eq!(facts.len(), 1, "{response}");
    assert_eq!(facts[0]["predicate"], "works_at");
    assert_eq!(facts[0]["status"], "approved");
    assert_eq!(facts[0]["domain"], "hr");
    assert_eq!(facts[0]["sources"].as_array().unwrap().len(), 1);
    assert_eq!(facts[0]["sources"][0]["quote"], "Alice works at Acme Corp");

    let sources = response["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1, "{response}");
    assert_eq!(sources[0]["id"], doc_id);
    assert_eq!(sources[0]["source_type"], "markdown");
    assert_eq!(sources[0]["original_path"], "/docs/hr.md");
}

/// With description + metadata: both are present (the raw metadata
/// string, not parsed).
#[test]
fn dossier_entity_carries_description_and_raw_metadata() {
    let (db, alice) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let alice = entities.create(
            "PERSON",
            "Alice",
            "hr",
            Some("Senior engineer"),
            Some(0.9),
            Some(r#"{"role":"senior_engineer"}"#),
        )?;
        Ok(alice)
    });
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();
    assert_eq!(response["entity"]["description"], "Senior engineer");
    assert_eq!(response["entity"]["confidence"], 0.9);
    assert_eq!(
        response["entity"]["metadata"], r#"{"role":"senior_engineer"}"#,
        "metadata must be the raw string, not parsed"
    );
}

/// Both flags false → the sections are omitted (omitempty).
#[test]
fn dossier_excludes_facts_and_sources_when_disabled() {
    let (db, (alice, _, _)) = seeded_dossier_db();
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({
            "entity_id": alice.to_string(),
            "include_facts": false,
            "include_sources": false,
        })),
    )
    .unwrap();
    assert!(response.get("facts").is_none(), "{response}");
    assert!(response.get("sources").is_none(), "{response}");
    assert_eq!(response["entity"]["id"], alice);
}

/// Out-of-range depths are clamped, not rejected.
#[test]
fn dossier_depth_is_clamped_not_rejected() {
    let (db, alice) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("PERSON", "Alice", "hr", None, None, None)
    });
    let graph = ready_graph(&db);
    for depth in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!(10),
        serde_json::json!("3"),
    ] {
        let response = call(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": depth })),
        )
        .unwrap();
        assert_eq!(response["entity"]["id"], alice, "depth {depth}");
    }
}

/// An entity with no facts/sources/links yields a dossier with only the
/// `entity` section (all others omitted).
#[test]
fn dossier_empty_sections_are_omitted() {
    let (db, alice) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("PERSON", "Lonely", "hr", None, None, None)
    });
    let graph = ready_graph(&db);

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();
    assert_eq!(response["entity"]["id"], alice);
    for section in ["facts", "sources", "related_entities", "cross_domain_links"] {
        assert!(
            response.get(section).is_none(),
            "{section} must be omitted: {response}"
        );
    }
}

// ── cross-domain links ───────────────────────────────────────────────────

/// Same-domain filtering (with graph): same-domain targets stay out of
/// `cross_domain_links` but appear in `related_entities`; cross-domain
/// targets appear in both.
#[test]
fn cross_links_filter_same_domain() {
    let (db, ids) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let facts = FactDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let same = entities.create("ORGANIZATION", "Acme HR", "hr", None, None, None)?;
        facts.create(Some(id_hr), "works_at", Some(same), "hr", None, None, None)?;
        let id_product = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        let id_it = entities.create("POLICY", "IT Policy", "it", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: id_product,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: None,
        })?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: id_it,
            relation_type: "related_to".into(),
            method: "equals".into(),
            confidence: 0.85,
            evidence: None,
        })?;
        Ok((id_hr, id_product, id_it))
    });
    let graph = ready_graph(&db);

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": ids.0.to_string() })),
    )
    .unwrap();

    let cross = response["cross_domain_links"].as_array().unwrap();
    let domains: std::collections::HashSet<&str> = cross
        .iter()
        .map(|link| link["target_domain"].as_str().unwrap())
        .collect();
    assert!(!domains.contains("hr"), "no same-domain entry: {response}");
    assert!(domains.contains("product"), "{response}");
    assert!(domains.contains("it"), "{response}");

    // The same-domain fact neighbor is a related entity, not a cross link.
    let related = response["related_entities"].as_array().unwrap();
    assert!(
        related
            .iter()
            .any(|node| node["domain"] == "hr" && node["id"] != ids.0),
        "a same-domain related entity is expected: {response}"
    );
}

/// Deduplication (no graph): two links to the same target collapse to
/// one, keeping the higher confidence and both relation types.
#[test]
fn cross_links_dedup_by_target_and_keep_best_provenance() {
    let (db, (id_hr, id_product)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let id_product = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: id_product,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.7,
            evidence: None,
        })?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: id_product,
            relation_type: "equals".into(),
            method: "llm".into(),
            confidence: 0.95,
            evidence: None,
        })?;
        Ok((id_hr, id_product))
    });
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();

    let cross = response["cross_domain_links"].as_array().unwrap();
    assert_eq!(cross.len(), 1, "one entry per target: {response}");
    let link = &cross[0];
    assert_eq!(link["target_entity_id"], id_product);
    assert_eq!(link["confidence"], 0.95, "the higher confidence is kept");
    let types: Vec<&str> = link["relation_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(types.contains(&"related_to"), "{types:?}");
    assert!(types.contains(&"equals"), "{types:?}");
}

/// Incoming links (no graph): a link where the entity is the target
/// resolves the subject as the cross-domain target.
#[test]
fn cross_links_resolve_incoming_links() {
    let (db, id_hr) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let policy = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: policy,
            target_entity_id: id_hr,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: None,
        })?;
        Ok(id_hr)
    });
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();
    let cross = response["cross_domain_links"].as_array().unwrap();
    assert!(
        cross.iter().any(
            |link| link["target_name"] == "Hiring Policy" && link["target_domain"] == "product"
        ),
        "the incoming link must resolve: {response}"
    );
}

/// Relation-type merging (no graph): per target the relation types are
/// the union, and the best-provenance entry's confidence is kept.
#[test]
fn cross_links_merge_relation_types_per_target() {
    let (db, id_hr) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let product = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        let server = entities.create("SERVER", "Server A", "it", None, None, None)?;
        for (target, rel, method, conf) in [
            (product, "related_to", "rule", 0.95),
            (product, "equals", "llm", 0.85),
            (product, "related_to", "equals", 0.6),
            (server, "manages", "rule", 0.9),
        ] {
            links.create(&EntityLink {
                subject_entity_id: id_hr,
                target_entity_id: target,
                relation_type: rel.into(),
                method: method.into(),
                confidence: conf,
                evidence: None,
            })?;
        }
        Ok(id_hr)
    });
    let graph = GraphIndex::Unavailable;

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();
    let cross = response["cross_domain_links"].as_array().unwrap();
    assert_eq!(cross.len(), 2, "one entry per target: {response}");

    let product = cross
        .iter()
        .find(|link| link["target_domain"] == "product")
        .unwrap();
    let product_types: Vec<&str> = product["relation_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(product_types.contains(&"related_to"), "{product_types:?}");
    assert!(product_types.contains(&"equals"), "{product_types:?}");
    assert_eq!(product["confidence"], 0.95);

    let server = cross
        .iter()
        .find(|link| link["target_domain"] == "it")
        .unwrap();
    assert_eq!(server["relation_types"], serde_json::json!(["manages"]));
}

/// Only incident edges (with graph): only edges incident to the center
/// produce cross links; a depth-2 hop is not.
#[test]
fn cross_links_bfs_only_incident_edges() {
    let (db, id_hr) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let policy_a = entities.create("POLICY", "Policy A", "product", None, None, None)?;
        let policy_b = entities.create("POLICY", "Policy B", "product", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: policy_a,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: None,
        })?;
        links.create(&EntityLink {
            subject_entity_id: policy_a,
            target_entity_id: policy_b,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.85,
            evidence: None,
        })?;
        Ok(id_hr)
    });
    let graph = ready_graph(&db);

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();
    let cross = response["cross_domain_links"].as_array().unwrap();
    assert!(
        cross.iter().any(|link| link["target_name"] == "Policy A"),
        "the incident link is expected: {response}"
    );
    assert!(
        !cross.iter().any(|link| link["target_name"] == "Policy B"),
        "a non-incident hop must not be a cross link: {response}"
    );
}

/// BFS + direct types (with graph): the BFS edge's relation type and the
/// direct link's type both land in the same target's union.
#[test]
fn cross_links_merge_bfs_and_direct_types() {
    let (db, id_hr) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let product = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: product,
            relation_type: "related_to".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: None,
        })?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: product,
            relation_type: "equals".into(),
            method: "llm".into(),
            confidence: 0.85,
            evidence: None,
        })?;
        Ok(id_hr)
    });
    let graph = ready_graph(&db);

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();
    let cross = response["cross_domain_links"].as_array().unwrap();
    assert_eq!(cross.len(), 1, "one entry per target: {response}");
    let types: Vec<&str> = cross[0]["relation_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(types.contains(&"related_to"), "{types:?}");
    assert!(types.contains(&"equals"), "{types:?}");
}

/// Provenance (with graph): a cross-domain link carries its method and
/// confidence.
#[test]
fn cross_links_carry_provenance() {
    let (db, id_hr) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let id_hr = entities.create("PERSON", "Alice", "hr", None, None, None)?;
        let product = entities.create("POLICY", "Hiring Policy", "product", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: id_hr,
            target_entity_id: product,
            relation_type: "equals".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: Some("name match with confidence 0.95".into()),
        })?;
        Ok(id_hr)
    });
    let graph = ready_graph(&db);

    let response = call(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": id_hr.to_string() })),
    )
    .unwrap();
    let cross = response["cross_domain_links"].as_array().unwrap();
    let link = cross
        .iter()
        .find(|link| link["target_domain"] == "product")
        .unwrap_or_else(|| panic!("a product cross link is expected: {response}"));
    assert_eq!(link["method"], "rule");
    assert_eq!(link["confidence"], 0.95);
    assert_eq!(link["evidence"], "name match with confidence 0.95");
}
