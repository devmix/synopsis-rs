//! Integration tests for the `get_entity_relations` and `get_entity_links`
//! tools (extracted from `src/tools/graph_tools.rs` by test-hygiene-phase-1
//! task 1.5). The 17 tests previously lived in the inline `#[cfg(test)] mod
//! tests` module; they now run against the public API
//! (`mcp::tools::graph_tools`), with the `crate::`/`super::*` imports
//! rewritten to the crate name.
//!
//! Oracle mapping: Go `internal/mcp/handlers/{get_entity_relations.go,
//! get_entity_links.go}` + the shared `entity_resolve.go`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use config::preset::GraphConfig;
use db::{ConnectionOrTx, EntityDao, EntityLink, EntityLinkDao, test_util};
use graph::GraphIndex;
use mcp::error::McpError;
use mcp::tools::graph_tools::{handle_get_entity_links, handle_get_entity_relations};
use serde_json::Value;

/// Seed an in-memory KB in one transaction; returns the db and the value
/// the seeder produced (usually the created ids). Same helper as
/// `dossier.rs`.
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

fn relations(db: &db::Db, graph: &GraphIndex, args: Option<Value>) -> Result<Value, McpError> {
    handle_get_entity_relations(db, graph, args.as_ref())
}

fn links(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
    handle_get_entity_links(db, args.as_ref())
}

/// The oracle `setupTestGraph` fixture: Alice --works_in--> Engineering,
/// Bob --works_in--> Engineering, Alice --owns--> NDA,
/// Policy --requires--> Bob, Alice --reports_to--> Policy.
fn seeded_relations_db() -> (db::Db, (i64, i64, i64, i64, i64)) {
    seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let facts = db::FactDao::new(exec);
        let alice = entities.create("employee", "Alice", "", None, None, None)?;
        let engineering = entities.create("department", "Engineering", "", None, None, None)?;
        let nda = entities.create("policy", "NDA", "", None, None, None)?;
        let bob = entities.create("employee", "Bob", "", None, None, None)?;
        let policy = entities.create("policy", "Security Policy", "", None, None, None)?;
        facts.create(
            Some(alice),
            "works_in",
            Some(engineering),
            "",
            None,
            None,
            None,
        )?;
        facts.create(
            Some(bob),
            "works_in",
            Some(engineering),
            "",
            None,
            None,
            None,
        )?;
        facts.create(Some(alice), "owns", Some(nda), "", None, None, None)?;
        facts.create(Some(policy), "requires", Some(bob), "", None, None, None)?;
        facts.create(
            Some(alice),
            "reports_to",
            Some(policy),
            "",
            None,
            None,
            None,
        )?;
        Ok((alice, engineering, nda, bob, policy))
    })
}

// ── get_entity_relations ─────────────────────────────────────────────────

/// Oracle `TestHandleGetEntityRelations_NilGraph` (the task's "no-graph
/// degradation"): an unavailable graph is a tool error BEFORE argument
/// parsing — `None` args must still yield the graph error, not an
/// argument error.
#[test]
fn relations_unavailable_graph_errors_before_arg_parsing() {
    let db = test_util::in_memory_db();
    let graph = GraphIndex::Unavailable;

    let err = relations(&db, &graph, None).unwrap_err();
    assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
    assert!(
        err.to_string().contains("knowledge graph is not available"),
        "got: {err}"
    );
}

/// Oracle "empty entity_id and entity_name returns error" +
/// `TestHandleGetEntityRelations_BothIDAndName`: the XOR is enforced.
#[test]
fn relations_requires_exactly_one_of_id_or_name() {
    let (db, _) = seeded_relations_db();
    let graph = ready_graph(&db);
    for args in [
        None,
        Some(serde_json::json!({})),
        Some(serde_json::json!({ "entity_id": "" })),
        Some(serde_json::json!({ "entity_id": "1", "entity_name": "Alice" })),
    ] {
        let err = relations(&db, &graph, args).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
    }
}

/// Oracle "non-integer entity_id returns error".
#[test]
fn relations_non_integer_id_is_an_error() {
    let (db, _) = seeded_relations_db();
    let graph = ready_graph(&db);
    let err = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": "not_a_number" })),
    )
    .unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("must be an integer"), "got: {err}");
}

/// Oracle "nonexistent entity returns error" + `EntityNameNotFound`.
#[test]
fn relations_missing_entity_is_not_found() {
    let (db, _) = seeded_relations_db();
    let graph = ready_graph(&db);
    for args in [
        serde_json::json!({ "entity_id": "99999" }),
        serde_json::json!({ "entity_name": "NonExistentEntity" }),
    ] {
        let err = relations(&db, &graph, Some(args)).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(err.to_string().contains("not found"), "got: {err}");
    }
}

/// Oracle happy path + `_ResponseFields`: depth-1 traversal from Alice,
/// response shape, and non-zero/non-empty edge endpoints.
#[test]
fn relations_depth_one_returns_direct_neighbors() {
    let (db, (alice, ..)) = seeded_relations_db();
    let graph = ready_graph(&db);

    let response = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": 1 })),
    )
    .unwrap();

    assert_eq!(response["center_entity"]["id"], alice);
    assert_eq!(response["center_entity"]["name"], "Alice");
    assert_eq!(response["center_entity"]["type"], "employee");
    assert_eq!(response["total_nodes"], 3, "{response}");
    assert_eq!(response["total_edges"], 3, "{response}");
    assert_eq!(response["traversal_depth"], 1);
    assert!(response["traversal_time_ms"].is_u64(), "{response}");

    let names: Vec<&str> = response["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Engineering", "NDA", "Security Policy"],
        "{response}"
    );

    // Oracle `_ResponseFields`: every edge carries non-zero ids and
    // non-empty endpoint names.
    for edge in response["edges"].as_array().unwrap() {
        assert_eq!(edge["source_id"].as_i64().unwrap(), alice);
        assert!(edge["target_id"].as_i64().unwrap() != 0, "{edge}");
        assert!(!edge["source_name"].as_str().unwrap().is_empty(), "{edge}");
        assert!(!edge["target_name"].as_str().unwrap().is_empty(), "{edge}");
    }
}

/// Depth-2 traversal: Bob is a second hop (reached through Engineering's
/// incoming `works_in`).
#[test]
fn relations_depth_two_reaches_second_hop() {
    let (db, (alice, ..)) = seeded_relations_db();
    let graph = ready_graph(&db);

    let response = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": 2 })),
    )
    .unwrap();
    assert_eq!(response["total_nodes"], 4, "{response}");
    assert_eq!(response["total_edges"], 4, "{response}");
    let names: Vec<&str> = response["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Bob"), "Bob is a second hop: {response}");
}

/// Oracle "depth zero defaults to 1" + "depth over 10 capped at 10", plus
/// the house lenient numeric string and the default for unparseable input.
#[test]
fn relations_depth_is_clamped_to_the_frozen_range() {
    let (db, alice) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("employee", "Alice", "", None, None, None)
    });
    let graph = ready_graph(&db);
    for (depth, expected) in [
        (serde_json::json!(0), 1u32),
        (serde_json::json!(-3), 1u32),
        (serde_json::json!(50), 10u32),
        (serde_json::json!("7"), 7u32),
        (serde_json::json!("bogus"), 2u32),
    ] {
        let response = relations(
            &db,
            &graph,
            Some(serde_json::json!({ "entity_id": alice.to_string(), "depth": depth })),
        )
        .unwrap();
        assert_eq!(
            response["traversal_depth"].as_u64().unwrap(),
            expected as u64,
            "depth {depth}"
        );
    }
}

/// Oracle `TestHandleGetEntityRelations_DomainDisambiguation`: two
/// same-named entities in different domains; a domain narrows the lookup
/// (case-insensitive), no domain + multiple matches lists the candidates,
/// and a single match succeeds.
#[test]
fn relations_domain_disambiguates_and_lists_candidates() {
    let (db, (id_hr, id_product, _acme, unique)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let facts = db::FactDao::new(exec);
        let id_hr = entities.create("employee", "Alice", "hr", None, None, None)?;
        let id_product = entities.create("employee", "Alice", "product", None, None, None)?;
        let acme = entities.create("organization", "Acme Corp", "hr", None, None, None)?;
        facts.create(Some(id_hr), "works_at", Some(acme), "hr", None, None, None)?;
        let unique = entities.create("employee", "UniqueBob", "hr", None, None, None)?;
        Ok((id_hr, id_product, acme, unique))
    });
    let graph = ready_graph(&db);

    let by_hr = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
    )
    .unwrap();
    assert_eq!(by_hr["center_entity"]["id"], id_hr);

    let by_product = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
    )
    .unwrap();
    assert_eq!(by_product["center_entity"]["id"], id_product);

    let ambiguous = relations(
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

    let single = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_name": "UniqueBob" })),
    )
    .unwrap();
    assert_eq!(single["center_entity"]["id"], unique);
}

/// The index can lag the database (built at startup, rebuilt on demand):
/// an entity present in the db but absent from the index is a not-found
/// tool error (oracle `g.GetNode` miss; recorded deviation 1 wording).
#[test]
fn relations_entity_missing_from_index_is_not_found() {
    let (db, _) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("employee", "Alice", "", None, None, None)
    });
    let graph = ready_graph(&db);

    // Insert AFTER the index is built: the graph cannot know this entity.
    let late = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                "employee",
                "LateArriver",
                "",
                None,
                None,
                None,
            )
        })
        .and_then(|result| result)
        .expect("late insert");

    let err = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": late.to_string() })),
    )
    .unwrap_err();
    assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
    assert!(err.to_string().contains("not found in graph"), "got: {err}");
}

/// `include_cross_domain`: with the flag, entity-link edges cross domains
/// and carry provenance metadata; the cross-domain FACT edge stays blocked
/// (D4); without the flag, links are not followed and no metadata is
/// emitted.
#[test]
fn relations_cross_domain_flag_controls_links_and_metadata() {
    let (db, (alice, ..)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let facts = db::FactDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
        let bob = entities.create("employee", "Bob", "hr", None, None, None)?;
        let acme = entities.create("organization", "Acme", "it", None, None, None)?;
        let globex = entities.create("organization", "Globex", "it", None, None, None)?;
        facts.create(Some(alice), "knows", Some(bob), "hr", None, None, None)?;
        // A cross-domain FACT edge: blocked by D4 even with the flag.
        facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: alice,
            target_entity_id: globex,
            relation_type: "same_entity".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: Some("rule: hr/Alice -> it/Globex".into()),
        })?;
        Ok((alice, bob, acme, globex))
    });
    let graph = ready_graph(&db);

    // include_cross_domain = true: Globex crosses via the LINK (metadata
    // present), Bob via the fact (no metadata), Acme stays blocked.
    let with = relations(
        &db,
        &graph,
        Some(serde_json::json!({
            "entity_id": alice.to_string(),
            "include_cross_domain": true,
        })),
    )
    .unwrap();
    let names: Vec<&str> = with["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Globex"), "{with}");
    assert!(names.contains(&"Bob"), "{with}");
    assert!(
        !names.contains(&"Acme"),
        "cross-domain fact must stay blocked: {with}"
    );

    let globex_edge = with["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["target_name"] == "Globex")
        .unwrap();
    assert_eq!(globex_edge["metadata"]["method"], "rule");
    assert_eq!(globex_edge["metadata"]["confidence"], 0.95);
    assert_eq!(
        globex_edge["metadata"]["evidence"],
        "rule: hr/Alice -> it/Globex"
    );
    let bob_edge = with["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["target_name"] == "Bob")
        .unwrap();
    assert!(bob_edge.get("metadata").is_none(), "{with}");

    // include_cross_domain = false: no link followed, no metadata.
    let without = relations(
        &db,
        &graph,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();
    let names: Vec<&str> = without["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Bob"], "{without}");
    for edge in without["edges"].as_array().unwrap() {
        assert!(edge.get("metadata").is_none(), "{without}");
    }
}

// ── get_entity_links ─────────────────────────────────────────────────────

/// Oracle `TestHandleGetEntityLinks` error cases: missing args, empty id,
/// non-integer id, and a not-found id.
#[test]
fn links_argument_errors() {
    let (db, _) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("employee", "Alice", "", None, None, None)
    });

    let err = links(&db, None).unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );

    let err = links(&db, Some(serde_json::json!({ "entity_id": "" }))).unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );

    let err = links(&db, Some(serde_json::json!({ "entity_id": "abc" }))).unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("must be an integer"), "got: {err}");

    let err = links(&db, Some(serde_json::json!({ "entity_id": "99999" }))).unwrap_err();
    assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
}

/// Oracle `TestHandleGetEntityLinks_EntityWithNoLinks`: `links` is an
/// empty array, still present in the response.
#[test]
fn links_empty_entity_returns_empty_links_array() {
    let (db, id) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("employee", "SoloEmployee", "", None, None, None)
    });
    let response = links(
        &db,
        Some(serde_json::json!({ "entity_id": id.to_string() })),
    )
    .unwrap();
    assert_eq!(response["entity"]["id"], id);
    assert_eq!(response["entity"]["name"], "SoloEmployee");
    assert_eq!(response["links"], serde_json::json!([]), "{response}");
}

/// Oracle `TestHandleGetEntityLinks_EntityWithLinks`: the entity's links
/// with the DAO's deterministic `(target, subject)` order and the full
/// provenance payload.
#[test]
fn links_returns_entity_links_with_provenance() {
    let (db, (alice, engineering, nda)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
        let engineering = entities.create("department", "Engineering", "hr", None, None, None)?;
        let nda = entities.create("policy", "NDA", "hr", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: alice,
            target_entity_id: engineering,
            relation_type: "located_in".into(),
            method: "rule".into(),
            confidence: 0.95,
            evidence: Some("Rule-based matching on keyword 'Alice'".into()),
        })?;
        links.create(&EntityLink {
            subject_entity_id: alice,
            target_entity_id: nda,
            relation_type: "owns".into(),
            method: "llm".into(),
            confidence: 0.78,
            evidence: Some("LLM inference from context".into()),
        })?;
        Ok((alice, engineering, nda))
    });

    let response = links(
        &db,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();
    assert_eq!(response["entity"]["id"], alice);

    let out = response["links"].as_array().unwrap();
    assert_eq!(out.len(), 2, "{response}");
    // DAO order: (target, subject) ascending → Engineering, then NDA.
    assert_eq!(out[0]["target_entity_id"], engineering);
    assert_eq!(out[0]["target_name"], "Engineering");
    assert_eq!(out[0]["target_domain"], "hr");
    assert_eq!(out[0]["relation_type"], "located_in");
    assert_eq!(out[0]["method"], "rule");
    assert_eq!(out[0]["confidence"], 0.95);
    assert_eq!(out[0]["evidence"], "Rule-based matching on keyword 'Alice'");
    assert_eq!(out[1]["target_entity_id"], nda);
    assert_eq!(out[1]["relation_type"], "owns");
    assert_eq!(out[1]["method"], "llm");
    assert_eq!(out[1]["confidence"], 0.78);
}

/// Oracle `TestHandleGetEntityLinks_LinksProvenance`: method / confidence
/// / evidence round-trip; an absent evidence value is omitted from the
/// wire (oracle `omitempty`).
#[test]
fn links_provenance_round_trips_and_omits_absent_evidence() {
    let (db, bob) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let bob = entities.create("employee", "Bob", "", None, None, None)?;
        let it = entities.create("department", "IT", "", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: bob,
            target_entity_id: it,
            relation_type: "resides_in".into(),
            method: "llm".into(),
            confidence: 0.85,
            evidence: Some("Found in chapter 3, paragraph 2".into()),
        })?;
        Ok(bob)
    });

    let response = links(
        &db,
        Some(serde_json::json!({ "entity_id": bob.to_string() })),
    )
    .unwrap();
    let out = response["links"].as_array().unwrap();
    assert_eq!(out.len(), 1, "{response}");
    assert_eq!(out[0]["method"], "llm");
    assert_eq!(out[0]["confidence"], 0.85);
    assert!(
        out[0]["evidence"].as_str().unwrap().contains("chapter 3"),
        "{response}"
    );

    // A link without evidence omits the key.
    let (db2, carol) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let carol = entities.create("employee", "Carol", "", None, None, None)?;
        let ops = entities.create("department", "Ops", "", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: carol,
            target_entity_id: ops,
            relation_type: "resides_in".into(),
            method: "rule".into(),
            confidence: 0.6,
            evidence: None,
        })?;
        Ok(carol)
    });
    let response2 = links(
        &db2,
        Some(serde_json::json!({ "entity_id": carol.to_string() })),
    )
    .unwrap();
    assert!(
        response2["links"][0].get("evidence").is_none(),
        "{response2}"
    );
}

/// Oracle `TestHandleGetEntityLinks_DedupBidirectional`: an A→B / B→A
/// pair with the same relation type yields ONE entry; the first
/// occurrence (DAO order) wins.
#[test]
fn links_dedup_bidirectional_pairs() {
    let (db, (alice, office)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let links = EntityLinkDao::new(exec);
        let alice = entities.create("employee", "Alice", "", None, None, None)?;
        let office = entities.create("department", "Office", "", None, None, None)?;
        links.create(&EntityLink {
            subject_entity_id: alice,
            target_entity_id: office,
            relation_type: "works_in".into(),
            method: "rule".into(),
            confidence: 1.0,
            evidence: None,
        })?;
        links.create(&EntityLink {
            subject_entity_id: office,
            target_entity_id: alice,
            relation_type: "works_in".into(),
            method: "rule".into(),
            confidence: 1.0,
            evidence: None,
        })?;
        Ok((alice, office))
    });

    let response = links(
        &db,
        Some(serde_json::json!({ "entity_id": alice.to_string() })),
    )
    .unwrap();
    let out = response["links"].as_array().unwrap();
    assert_eq!(out.len(), 1, "one entry per (target, relation): {response}");
    assert_eq!(out[0]["target_entity_id"], office);
    assert_eq!(out[0]["relation_type"], "works_in");
}

/// Oracle `TestHandleGetEntityLinks_NilTargetGuard`: a link whose target
/// row is gone is skipped silently. The v5 FKs make such a row
/// uncreatable through the DAOs (recorded deviation 6), so the fixture
/// inserts past the FK on this connection only.
#[test]
fn links_skip_missing_target_rows() {
    let (db, charlie) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        entities.create("employee", "Charlie", "", None, None, None)
    });

    db.with_conn(|conn| {
        conn.pragma_update(None, "foreign_keys", "OFF")
            .and_then(|_| {
                conn.execute(
                    "INSERT INTO entity_links \
                     (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (charlie, 99_999, "knows", "rule", 1.0f64, None::<String>),
                )
            })
            .and_then(|_| conn.pragma_update(None, "foreign_keys", "ON"))
            .map_err(db::DbError::from)
    })
    .and_then(|result| result)
    .expect("dangling link row inserted past the FK");

    let response = links(
        &db,
        Some(serde_json::json!({ "entity_id": charlie.to_string() })),
    )
    .unwrap();
    assert_eq!(response["links"], serde_json::json!([]), "{response}");
}

/// Oracle `TestHandleGetEntityLinks_DomainDisambiguation`: a domain
/// narrows the lookup (case-insensitive), multiple matches without a
/// domain are an error listing the candidates, and a single match
/// succeeds.
#[test]
fn links_domain_disambiguates_and_lists_candidates() {
    let (db, (id_hr, id_product, unique)) = seed_db(|exec| {
        let entities = EntityDao::new(exec);
        let id_hr = entities.create("employee", "Alice", "hr", None, None, None)?;
        let id_product = entities.create("employee", "Alice", "product", None, None, None)?;
        let unique = entities.create("employee", "UniqueBob", "hr", None, None, None)?;
        Ok((id_hr, id_product, unique))
    });

    let by_hr = links(
        &db,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "hr" })),
    )
    .unwrap();
    assert_eq!(by_hr["entity"]["id"], id_hr);

    let by_product = links(
        &db,
        Some(serde_json::json!({ "entity_name": "Alice", "domain": "PRODUCT" })),
    )
    .unwrap();
    assert_eq!(by_product["entity"]["id"], id_product);

    let ambiguous = links(&db, Some(serde_json::json!({ "entity_name": "Alice" }))).unwrap_err();
    assert!(
        matches!(ambiguous, McpError::InvalidArguments { .. }),
        "got: {ambiguous:?}"
    );
    assert!(
        ambiguous.to_string().contains("multiple entities match"),
        "got: {ambiguous}"
    );

    let single = links(&db, Some(serde_json::json!({ "entity_name": "UniqueBob" }))).unwrap();
    assert_eq!(single["entity"]["id"], unique);
}
