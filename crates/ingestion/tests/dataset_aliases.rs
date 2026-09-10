//! Dataset alias map (the ontology `<aliases>` blocks, design D4 revised)
//! scenario tests (change `multilingual-entity-resolution`, task 4.2).
//!
//! These tests pin the dataset-alias tier (design D4): the normalized alias
//! surface form → canonical name map consulted as tier 3 of
//! `find_best_candidate` (before the alias memory), and the creation path —
//! a surface form that matches no tier creates the entity under the
//! canonical name.
//!
//! Generic words only (NDA): no subject-matter entity names. The Jaro-Winkler
//! values referenced in the comments are pinned to the actual values so the
//! scenarios are stable (JW(`"washington"`, `"wilmington"`) = 0.8200 < 0.85,
//! so the similarity tier can never resolve the pair at the 0.85 threshold).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use db::test_util::in_memory_db;
use db::{ConnectionOrTx, Db, DocumentDao, EntityAliasDao, EntityDao};
use ingestion::{NerEntity, Resolver};
use serde_json::Map;

const TYPE: &str = "person";
const DOMAIN: &str = "x";

/// Builds a NER entity with the default type/domain.
fn entity(name: &str) -> NerEntity {
    entity_in(name, TYPE, DOMAIN)
}

/// Builds a NER entity with an explicit type and domain.
fn entity_in(name: &str, entity_type: &str, domain: &str) -> NerEntity {
    NerEntity {
        name: name.to_string(),
        entity_type: entity_type.to_string(),
        description: String::new(),
        confidence: 1.0,
        domain: domain.to_string(),
        metadata: Map::new(),
    }
}

/// Opens an in-memory knowledge DB and creates one document — the FK target
/// for the `entity_sources` provenance links the creating APIs write.
fn setup() -> (Db, i64) {
    let db = in_memory_db();
    let doc_id = db
        .with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .create("markdown", "/t.md", None, None)
                .expect("create document")
        })
        .expect("with_conn");
    (db, doc_id)
}

/// The alias map for the scenarios: `"Washington" → "Wilmington"`.
fn alias_map() -> HashMap<String, String> {
    HashMap::from([("Washington".to_string(), "Wilmington".to_string())])
}

/// Runs `resolver.add_entities` (cluster + resolve + create + link) over one
/// pooled connection.
fn add(db: &Db, doc_id: i64, resolver: &Resolver, entities: &[NerEntity]) {
    db.with_conn(|conn| {
        let _ = resolver
            .add_entities(ConnectionOrTx::Connection(conn), doc_id, entities)
            .expect("add_entities");
    })
    .expect("with_conn");
}

/// Non-creating lookup of one entity, aligned with the input.
fn lookup(db: &Db, resolver: &Resolver, entities: &[NerEntity]) -> Vec<Option<i64>> {
    db.with_conn(|conn| {
        resolver
            .lookup(ConnectionOrTx::Connection(conn), entities)
            .expect("lookup")
    })
    .expect("with_conn")
}

/// All persisted entity names, ordered by name (deterministic).
fn names(db: &Db) -> Vec<String> {
    db.with_conn(|conn| {
        EntityDao::new(ConnectionOrTx::Connection(conn))
            .list()
            .expect("list entities")
    })
    .expect("with_conn")
    .into_iter()
    .map(|entity| entity.name)
    .collect()
}

/// The id of the entity with the exact (type, name, domain) key.
fn id_of(db: &Db, entity_type: &str, name: &str, domain: &str) -> i64 {
    db.with_conn(|conn| {
        EntityDao::new(ConnectionOrTx::Connection(conn))
            .get_by_name(entity_type, name, domain)
            .expect("get_by_name")
            .expect("entity exists")
            .id
    })
    .expect("with_conn")
}

/// All alias surface forms recorded for `id`, ordered for determinism.
fn aliases_of(db: &Db, id: i64) -> Vec<String> {
    let mut got = db
        .with_conn(|conn| {
            EntityAliasDao::new(ConnectionOrTx::Connection(conn))
                .aliases_of(id)
                .expect("aliases_of")
        })
        .expect("with_conn");
    got.sort();
    got
}

/// (a) The alias surface form resolves to the existing canonical entity
/// (same type+domain) WITHOUT any similarity work. The threshold is raised
/// to 0.99 — above JW(`"washington"`, `"wilmington"`) = 0.8200 — so ONLY
/// the score-1.0 tiers (1–3) can settle the pair: tier 1 (match key) and
/// tier 2 (stem key) have distinct keys for the pair, and the alias memory
/// is empty (no `entity_aliases` rows), leaving the dataset-alias tier as
/// the sole possible resolver.
#[test]
fn a_alias_resolves_to_existing_canonical_without_similarity() {
    let (db, _doc_id) = setup();
    let id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Wilmington", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    let resolver = Resolver::with_aliases(0.99, &alias_map());
    assert_eq!(
        lookup(&db, &resolver, &[entity("Washington")]),
        vec![Some(id)]
    );
}

/// (b) The alias surface form with NO pre-existing entity creates the entity
/// UNDER THE CANONICAL NAME and records the surface form in the alias memory
/// (design D3).
#[test]
fn b_alias_creates_entity_under_canonical_name_and_records_alias() {
    let (db, doc_id) = setup();
    let resolver = Resolver::with_aliases(0.85, &alias_map());
    add(&db, doc_id, &resolver, &[entity("Washington")]);

    assert_eq!(names(&db), vec!["Wilmington".to_string()]);
    let id = id_of(&db, TYPE, "Wilmington", DOMAIN);
    assert_eq!(aliases_of(&db, id), vec!["Washington".to_string()]);
}

/// (c) After the alias created the entity under the canonical name, the
/// canonical name extracted directly resolves to the SAME entity (tier 1).
#[test]
fn c_canonical_name_resolves_to_the_same_entity_later() {
    let (db, doc_id) = setup();
    let resolver = Resolver::with_aliases(0.85, &alias_map());
    add(&db, doc_id, &resolver, &[entity("Washington")]);
    let id = id_of(&db, TYPE, "Wilmington", DOMAIN);

    // A fresh resolver (fresh in-memory index, re-hydrated from the DB):
    // the canonical name lands on tier 1.
    let fresh = Resolver::with_aliases(0.85, &alias_map());
    assert_eq!(lookup(&db, &fresh, &[entity("Wilmington")]), vec![Some(id)]);

    // And the alias still lands on the same entity.
    assert_eq!(lookup(&db, &fresh, &[entity("Washington")]), vec![Some(id)]);
}

/// (d) An alias entry does NOT resolve across type or domain: the canonical
/// exists only in a different type (and a different domain), so the alias
/// stays unresolved (JW = 0.8200 < 0.85 cannot bridge it either).
#[test]
fn d_alias_does_not_resolve_across_type_or_domain() {
    let (db, _doc_id) = setup();
    let _org_id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create("org", "Wilmington", "alpha", None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");
    let _person_id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Wilmington", "beta", None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    let resolver = Resolver::with_aliases(0.85, &alias_map());
    // (type "person", domain "x") matches neither the "org"/"alpha" entity
    // nor the "person"/"beta" entity.
    assert_eq!(lookup(&db, &resolver, &[entity("Washington")]), vec![None]);
    // And nothing is created by the non-creating lookup.
    assert_eq!(
        names(&db),
        vec!["Wilmington".to_string(), "Wilmington".to_string()]
    );
}

/// (e) An empty alias map behaves exactly like the pre-map resolution
/// (task 3.2): the JW near-miss pair stays separate.
#[test]
fn e_empty_map_behaves_like_the_pre_map_resolution() {
    let (db, doc_id) = setup();
    let resolver = Resolver::with_aliases(0.85, &HashMap::new());
    add(
        &db,
        doc_id,
        &resolver,
        &[entity("washington"), entity("wilmington")],
    );
    let mut got = names(&db);
    got.sort();
    assert_eq!(
        got,
        vec!["washington".to_string(), "wilmington".to_string()]
    );
}

/// (f) The map keys are normalized (trim + lowercase + whitespace collapse)
/// on the loader side, so a padded/mixed-case YAML key still matches the
/// extracted surface form.
#[test]
fn f_map_keys_are_normalized_before_lookup() {
    let (db, _doc_id) = setup();
    let id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Wilmington", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    let map = HashMap::from([("  Washington  ".to_string(), "Wilmington".to_string())]);
    let resolver = Resolver::with_aliases(0.85, &map);
    assert_eq!(
        lookup(&db, &resolver, &[entity("Washington")]),
        vec![Some(id)]
    );
}

/// (g) The dataset-alias tier is consulted BEFORE the alias memory: both
/// point at different entities, and the dataset map wins.
#[test]
fn g_dataset_alias_tier_precedes_the_alias_memory() {
    let (db, _doc_id) = setup();
    let memory_target = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Wilmington", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");
    let map_target = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Harbor Town", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    // The alias memory (a surface form recorded by an earlier run) points
    // "Washington" at the Wilmington entity…
    db.with_conn(|conn| {
        EntityAliasDao::new(ConnectionOrTx::Connection(conn))
            .insert_or_ignore(memory_target, "Washington")
            .expect("insert alias")
    })
    .expect("with_conn");

    // …but the dataset map re-points it at "Harbor Town".
    let map = HashMap::from([("Washington".to_string(), "Harbor Town".to_string())]);
    let resolver = Resolver::with_aliases(0.85, &map);
    assert_eq!(
        lookup(&db, &resolver, &[entity("Washington")]),
        vec![Some(map_target)]
    );
}

/// (h) A batch containing BOTH the alias and the canonical (a JW pair below
/// the threshold, so they cluster separately) yields exactly ONE entity,
/// named after the canonical, regardless of cluster order.
#[test]
fn h_mixed_batch_collapses_to_one_canonical_entity() {
    let (db, doc_id) = setup();
    let resolver = Resolver::with_aliases(0.85, &alias_map());
    add(
        &db,
        doc_id,
        &resolver,
        &[entity("washington"), entity("Wilmington")],
    );
    let mut got = names(&db);
    got.sort();
    assert_eq!(got, vec!["Wilmington".to_string()]);
}
