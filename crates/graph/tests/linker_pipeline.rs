//! End-to-end cross-domain linking (task 1.9 acceptance): the ontology is
//! loaded from a real `global.xml` (config crate), the pipeline runs over a
//! two-domain database, and a second run must be idempotent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use config::ontology::load_global_config;
use config::preset::LinkerConfig;
use db::test_util::in_memory_db;
use db::{ConnectionOrTx, EntityDao, EntityLinkDao, FactDao};
use graph::build_entity_links;

fn ontology_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linker-ontology")
}

/// Loads the fixture ontology and requires its `<cross-domain-links>` block.
fn links_config() -> config::ontology::CrossDomainLinksConfig {
    let global = load_global_config(ontology_dir())
        .expect("fixture global.xml must load")
        .expect("fixture must carry a cross-domain-links block");
    global
        .cross_domain_links
        .expect("fixture must carry a cross-domain-links block")
}

/// The two-domain fixture database:
/// - "Acme" (ORGANIZATION) in `hr` and `it` — the equals pair;
/// - "John Doe" (PERSON) in `hr` and `it` — the equals pair AND the
///   expression pair (John in `hr` works at Acme);
/// - "Jane Roe" (PERSON) in `hr` and `it` — equals only (no facts).
///
/// Returns the entity ids in insertion order.
fn fixture_db() -> (db::Db, Vec<i64>) {
    let db = in_memory_db();
    let ids = db
        .with_conn(|conn| -> Result<Vec<i64>, db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let mut ids: Vec<i64> = Vec::new();
            for (entity_type, name, domain) in [
                ("ORGANIZATION", "Acme", "hr"),
                ("ORGANIZATION", "Acme", "it"),
                ("PERSON", "John Doe", "hr"),
                ("PERSON", "John Doe", "it"),
                ("PERSON", "Jane Roe", "hr"),
                ("PERSON", "Jane Roe", "it"),
            ] {
                ids.push(entities.create(entity_type, name, domain, None, None, None)?);
            }
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(ids[2]),
                "works_at",
                Some(ids[0]),
                "hr",
                None,
                None,
                None,
            )?;
            Ok(ids)
        })
        .unwrap()
        .unwrap();
    (db, ids)
}

fn all_links(db: &db::Db) -> Vec<db::EntityLink> {
    db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap()
}

#[test]
fn end_to_end_linking_and_idempotent_rerun() {
    let config = links_config();
    assert_eq!(
        config.methods,
        vec![
            config::ontology::LinkMethod::Equals,
            config::ontology::LinkMethod::Expression
        ],
        "the fixture applies equals before expression"
    );

    let (db, ids) = fixture_db();
    let linker = LinkerConfig::default();

    // ── First run ─────────────────────────────────────────────────────────
    let first = build_entity_links(&db, &config, &linker).expect("first run must succeed");
    assert!(
        first.errors.is_empty(),
        "first run must not error: {:?}",
        first.errors
    );
    // equals: Acme, John, Jane — 3 pairs. expression: John only — 1 pair.
    assert_eq!(first.links_created, 4);
    assert_eq!(first.links_skipped, 0);

    let links = all_links(&db);
    assert_eq!(links.len(), 8, "Acme 2 + John 4 + Jane 2 rows");

    // equals rows: same_entity / equals / 0.9 for all three pairs.
    let equals_rows: Vec<&db::EntityLink> = links
        .iter()
        .filter(|link| link.method == "equals")
        .collect();
    assert_eq!(equals_rows.len(), 6, "Acme (2) + John (2) + Jane (2)");
    for link in &equals_rows {
        assert_eq!(link.relation_type, "same_entity");
        assert!((link.confidence - 0.9).abs() < f64::EPSILON);
    }
    // Jane's pair: equals only, no expression row.
    let jane_rows: Vec<&db::EntityLink> = links
        .iter()
        .filter(|link| link.subject_entity_id == ids[4] || link.target_entity_id == ids[4])
        .collect();
    assert_eq!(jane_rows.len(), 2);
    assert!(jane_rows.iter().all(|link| link.method == "equals"));

    // John's pair: equals AND the rule row (works_at_same_org / expression /
    // 1.0 / "expression: acme_workers").
    let john_rows: Vec<&db::EntityLink> = links
        .iter()
        .filter(|link| link.subject_entity_id == ids[2] || link.target_entity_id == ids[2])
        .collect();
    assert_eq!(john_rows.len(), 4);
    let rule_rows: Vec<&db::EntityLink> = john_rows
        .into_iter()
        .filter(|link| link.method == "expression")
        .collect();
    assert_eq!(rule_rows.len(), 2, "the rule link is bidirectional");
    for link in &rule_rows {
        assert_eq!(link.relation_type, "works_at_same_org");
        assert!((link.confidence - 1.0).abs() < f64::EPSILON);
        assert_eq!(link.evidence.as_deref(), Some("expression: acme_workers"));
        assert!(
            (link.subject_entity_id, link.target_entity_id) == (ids[2], ids[3])
                || (link.subject_entity_id, link.target_entity_id) == (ids[3], ids[2])
        );
    }

    // ── Second run: idempotent ────────────────────────────────────────────
    let second = build_entity_links(&db, &config, &linker).expect("second run must succeed");
    assert!(
        second.errors.is_empty(),
        "second run must not error: {:?}",
        second.errors
    );
    assert_eq!(second.links_created, 0, "no duplicates on the re-run");
    assert_eq!(second.links_skipped, 4, "every pair is already linked");
    assert_eq!(all_links(&db).len(), 8, "the row count is unchanged");
}
