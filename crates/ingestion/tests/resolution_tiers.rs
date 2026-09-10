//! Entity-resolution tier-order + alias-memory scenario tests (change
//! `multilingual-entity-resolution`, task 3.2).
//!
//! These tests pin the four-tier `find_best_candidate` order
//! (article-stripped exact → stem → alias memory → Jaro-Winkler), the alias
//! write-through (design D3), and the matching tier order in `cluster_batch`.
//!
//! Generic words only (NDA): no subject-matter entity names. The Jaro-Winkler
//! values referenced in the comments are pinned to the actual values so the
//! scenarios are stable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use db::test_util::in_memory_db;
use db::{ConnectionOrTx, Db, DocumentDao, EntityAliasDao, EntityDao};
use ingestion::{NerEntity, Resolver, cluster_batch};
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

/// (a) `The X` / `X` in the same type+domain resolve to ONE entity with the
/// longest name, regardless of the Jaro-Winkler threshold: the article-
/// stripped exact tier (match key `"library" == "library"`) settles the pair
/// even though JW(`"The Library"`, `"Library"`) = 0.8788 < 0.95.
#[test]
fn a_article_variant_merges_regardless_of_threshold() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.95);
    add(
        &db,
        doc_id,
        &resolver,
        &[entity("The Library"), entity("Library")],
    );
    assert_eq!(names(&db), vec!["The Library".to_string()]);
}

/// (b) A Russian case-variant pair the JW tier alone would miss
/// (JW = 0.8483 < 0.85) resolves to ONE entity via the stem tier: both forms
/// stem to `"люд"`.
#[test]
fn b_ru_case_variant_merges_via_stem_tier() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.85);
    add(&db, doc_id, &resolver, &[entity("люди"), entity("людей")]);
    // canonical_proto picks the longer form ("людей", 5 runes).
    assert_eq!(names(&db), vec!["людей".to_string()]);
}

/// (c) A cross-script pair is NOT merged by tiers 1–2 (and JW = 0.0): the
/// match/stem keys differ across scripts, so the two stay separate absent an
/// alias.
#[test]
fn c_cross_script_pair_stays_separate() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.85);
    add(&db, doc_id, &resolver, &[entity("город"), entity("city")]);
    let mut got = names(&db);
    got.sort();
    assert_eq!(got, vec!["city".to_string(), "город".to_string()]);
}

/// (d) A Jaro-Winkler near-miss just below the threshold
/// (JW(`"washington"`, `"wilmington"`) = 0.8200 < 0.85) with distinct tier
/// 1–2 keys is still NOT merged (distinct entities).
#[test]
fn d_jw_near_miss_below_threshold_stays_separate() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.85);
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

/// (e) A Jaro-Winkler pair at or above the threshold
/// (JW(`"martha"`, `"marhta"`) = 0.9611 ≥ 0.85) with distinct tier 1–2 keys
/// IS merged — the existing JW-tier behavior is preserved.
#[test]
fn e_jw_pair_at_or_above_threshold_merges() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.85);
    add(
        &db,
        doc_id,
        &resolver,
        &[entity("martha"), entity("marhta")],
    );
    // canonical_proto: equal rune count, first-encountered wins.
    assert_eq!(names(&db), vec!["martha".to_string()]);
}

/// (f) Domain isolation holds for ALL tiers: the same name+type in two
/// domains does not merge even though JW = 1.0 (identical names) — every
/// tier key is domain-scoped.
#[test]
fn f_domain_isolation_holds_across_all_tiers() {
    let (db, doc_id) = setup();
    let resolver = Resolver::new(0.85);
    add(
        &db,
        doc_id,
        &resolver,
        &[
            entity_in("The City", TYPE, "alpha"),
            entity_in("The City", TYPE, "beta"),
        ],
    );
    assert_eq!(names(&db).len(), 2);
}

/// (g1) After a non-creating resolution, the incoming surface form is
/// recorded in `entity_aliases` (design D3): resolving the shorter variant
/// against a longer canonical records the variant as an alias.
#[test]
fn g1_resolution_records_surface_form_in_alias_memory() {
    let (db, doc_id) = setup();
    let id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "The Library", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    let resolver = Resolver::new(0.85);
    // "Library" merges into "The Library" (tier 1, match key "library"); the
    // shorter name is not a promotion, so the surface form is recorded.
    add(&db, doc_id, &resolver, &[entity("Library")]);

    let aliases = db
        .with_conn(|conn| {
            EntityAliasDao::new(ConnectionOrTx::Connection(conn))
                .aliases_of(id)
                .expect("aliases_of")
        })
        .expect("with_conn");
    assert_eq!(aliases, vec!["Library".to_string()]);
}

/// (g2) A surface form present in the alias memory resolves by the alias
/// tier WITHOUT touching Jaro-Winkler: tiers 1–2 keys differ and
/// JW(`"washington"`, `"wilmington"`) = 0.8200 < 0.85, so only the pre-
/// seeded alias tier can resolve `"Washington"` to the Wilmington entity.
#[test]
fn g2_preseeded_alias_resolves_via_alias_tier_without_jw() {
    let (db, _doc_id) = setup();
    let id = db
        .with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .create(TYPE, "Wilmington", DOMAIN, None, Some(1.0), None)
                .expect("create entity")
        })
        .expect("with_conn");

    // Pre-seed the alias memory (a surface form recorded by an earlier run).
    db.with_conn(|conn| {
        EntityAliasDao::new(ConnectionOrTx::Connection(conn))
            .insert_or_ignore(id, "Washington")
            .expect("insert alias")
    })
    .expect("with_conn");

    // A fresh resolver hydrates the alias map from `entity_aliases` on its
    // first lookup, so "Washington" is in the tier-3 map.
    let resolver = Resolver::new(0.85);
    let found = db
        .with_conn(|conn| {
            resolver
                .lookup(ConnectionOrTx::Connection(conn), &[entity("Washington")])
                .expect("lookup")
        })
        .expect("with_conn");
    assert_eq!(found, vec![Some(id)]);
}

/// (h) `cluster_batch` merges the same pairs the resolver's tier order does
/// (differential on a mixed batch): the tier-1 article pair, the tier-2 stem
/// pair, and the tier-4 JW pair each collapse to one cluster, while the
/// cross-script pair, the lone name, and the two domain-isolated entities
/// stay separate. The resolver (same tier order) creates exactly one entity
/// per cluster.
#[test]
fn h_cluster_batch_merges_the_same_pairs_as_the_tier_order() {
    let batch = vec![
        entity_in("The Library", TYPE, DOMAIN),
        entity_in("Library", TYPE, DOMAIN),
        entity_in("люди", TYPE, DOMAIN),
        entity_in("людей", TYPE, DOMAIN),
        entity_in("martha", TYPE, DOMAIN),
        entity_in("marhta", TYPE, DOMAIN),
        entity_in("город", TYPE, DOMAIN),
        entity_in("city", TYPE, DOMAIN),
        entity_in("bank", "org", "alpha"),
        entity_in("bank", "org", "beta"),
    ];
    let threshold = 0.85;

    let clusters = cluster_batch(&batch, threshold);
    assert_eq!(clusters.len(), 7, "expected seven clusters: {clusters:?}");

    let as_set = |cluster: &[NerEntity]| -> BTreeSet<(String, String)> {
        cluster
            .iter()
            .map(|e| (e.name.clone(), e.domain.clone()))
            .collect()
    };
    let got: Vec<BTreeSet<(String, String)>> = clusters.iter().map(|c| as_set(c)).collect();

    let pair = |a: &str, b: &str| -> BTreeSet<(String, String)> {
        [
            (a.to_string(), DOMAIN.to_string()),
            (b.to_string(), DOMAIN.to_string()),
        ]
        .into_iter()
        .collect()
    };
    let single = |a: &str, d: &str| -> BTreeSet<(String, String)> {
        [(a.to_string(), d.to_string())].into_iter().collect()
    };
    let want = [
        pair("The Library", "Library"),
        pair("люди", "людей"),
        pair("martha", "marhta"),
        single("город", DOMAIN),
        single("city", DOMAIN),
        single("bank", "alpha"),
        single("bank", "beta"),
    ];

    let mut got_sorted: Vec<Vec<(String, String)>> =
        got.iter().map(|s| s.iter().cloned().collect()).collect();
    let mut want_sorted: Vec<Vec<(String, String)>> =
        want.iter().map(|s| s.iter().cloned().collect()).collect();
    got_sorted.sort();
    want_sorted.sort();
    assert_eq!(got_sorted, want_sorted);

    // Differential: the resolver (same tier order) creates exactly one
    // entity per cluster_batch cluster.
    let (db, doc_id) = setup();
    let resolver = Resolver::new(threshold);
    add(&db, doc_id, &resolver, &batch);
    assert_eq!(names(&db).len(), 7);
}
