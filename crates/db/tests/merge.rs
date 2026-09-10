//! Integration tests for the transactional entity merge
//! ([`db::merge_entities`]), relocated from the inline `#[cfg(test)]` module
//! in `crates/db/src/merge.rs` (change `multilingual-entity-resolution`,
//! task 2.2).
//!
//! The tests exercise only the public API (plus the always-compiled
//! [`db::test_util`] database fixtures); names and assertions are carried
//! over verbatim, so the move changes no behavior.
//!
//! # NDA
//!
//! Subject-matter entity names must not appear in these tests; the fixture
//! uses generic words only (`Into`, `From`, `Other`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::types::ValueRef;

use db::chunk::ChunkDao;
use db::document::DocumentDao;
use db::entity::{Entity, EntityDao};
use db::entity_alias::EntityAliasDao;
use db::entity_link::{EntityLink, EntityLinkDao};
use db::entity_source::EntitySourceDao;
use db::fact::FactDao;
use db::test_util::in_memory_db;
use db::{ChunkEntityDao, ConnectionOrTx, Db, DbError, merge_entities};

/// Render one row as a pipe-joined string (NULL → literal `NULL`).
fn row_str(r: &rusqlite::Row, col_count: usize) -> String {
    (0..col_count)
        .map(|i| match r.get_ref(i).unwrap() {
            ValueRef::Null => "NULL".to_string(),
            ValueRef::Integer(i) => i.to_string(),
            ValueRef::Real(f) => f.to_string(),
            ValueRef::Text(t) => String::from_utf8_lossy(t).to_string(),
            ValueRef::Blob(b) => format!("blob({})", b.len()),
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// A comparable, sorted snapshot of every merge-relevant table (the
/// "byte-identical / unmodified" oracle for the precondition and
/// rollback tests). NULLs are rendered as the literal `NULL`.
fn db_state(db: &Db) -> Vec<String> {
    db.with_conn(|conn| {
        let mut out: Vec<String> = Vec::new();
        let mut read = |sql: &str| {
            let mut stmt = conn.prepare(sql).unwrap();
            let col_count = stmt.column_count();
            let rows = stmt.query_map([], |r| Ok(row_str(r, col_count))).unwrap();
            for row in rows {
                out.push(row.unwrap());
            }
        };
        read("SELECT subject_entity_id, predicate, object_entity_id FROM facts ORDER BY id");
        read("SELECT chunk_id, entity_id FROM chunk_entities ORDER BY chunk_id, entity_id");
        read(
            "SELECT entity_id, document_id FROM entity_sources \
             ORDER BY entity_id, document_id",
        );
        read(
            "SELECT subject_entity_id, target_entity_id, relation_type FROM entity_links \
             ORDER BY subject_entity_id, target_entity_id, relation_type",
        );
        read("SELECT entity_id, alias FROM entity_aliases ORDER BY entity_id, alias");
        read("SELECT id, type, name, domain FROM entities ORDER BY id");
        out
    })
    .unwrap()
}

/// Create one entity and return its id.
fn make_entity(db: &Db, typ: &str, name: &str, domain: &str) -> i64 {
    db.with_conn(|conn| {
        let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
        entities.create(typ, name, domain, None, None, None)
    })
    .unwrap()
    .unwrap()
}

fn link(subject: i64, target: i64, rel: &str) -> EntityLink {
    EntityLink {
        subject_entity_id: subject,
        target_entity_id: target,
        relation_type: rel.to_string(),
        method: "rule".to_string(),
        confidence: 0.9,
        evidence: None,
    }
}

/// The full merge fixture: three same-type+domain entities where `from`
/// references facts/chunks/sources/links in both directions, plus a
/// pre-existing `into` row in each junction that forces a collision.
/// Returns the three entity ids.
fn seed_full_fixture(db: &Db) -> (i64, i64, i64) {
    db.exec_tx(|tx| -> Result<(i64, i64, i64), DbError> {
        let conn = ConnectionOrTx::Transaction(&*tx);
        let entities = EntityDao::new(conn);
        let facts = FactDao::new(conn);
        let docs = DocumentDao::new(conn);
        let chunks = ChunkDao::new(conn);
        let ce = ChunkEntityDao::new(conn);
        let es = EntitySourceDao::new(conn);
        let el = EntityLinkDao::new(conn);

        let into = entities.create("PERSON", "Into", "hr", None, None, None)?;
        let from = entities.create("PERSON", "From", "hr", None, None, None)?;
        let other = entities.create("PERSON", "Other", "hr", None, None, None)?;

        let d1 = docs.create("markdown", "/f/d1.md", None, None)?;
        let d2 = docs.create("markdown", "/f/d2.md", None, None)?;
        let d3 = docs.create("markdown", "/f/d3.md", None, None)?;
        let c1 = chunks.create(d1, "chunk one", 0, None, None)?;
        let c2 = chunks.create(d1, "chunk two", 1, None, None)?;
        let c3 = chunks.create(d1, "chunk three", 2, None, None)?;

        // facts: four re-pointed, two dropped (one collides with a
        // non-from fact, one with a lower-id from-fact).
        facts.create(Some(from), "p1", Some(other), "hr", None, None, None)?; // -> (into, other, p1)
        facts.create(Some(other), "p2", Some(from), "hr", None, None, None)?; // -> (other, into, p2)
        facts.create(Some(from), "p3", Some(from), "hr", None, None, None)?; // -> (into, into, p3)
        facts.create(Some(into), "p4", Some(other), "hr", None, None, None)?; // non-from (into, other, p4)
        facts.create(Some(from), "p4", Some(other), "hr", None, None, None)?; // -> (into, other, p4): DROPPED (A)
        facts.create(Some(from), "p6", Some(into), "hr", None, None, None)?; // -> (into, into, p6)
        facts.create(Some(into), "p6", Some(from), "hr", None, None, None)?; // -> (into, into, p6): DROPPED (B)

        // chunk_entities: two plain re-points + one PK collision.
        ce.link(c1, from)?;
        ce.link(c2, from)?;
        ce.link(c3, into)?; // pre-existing into row
        ce.link(c3, from)?; // collides with (c3, into)

        // entity_sources: two plain moves + one (entity, doc) collision.
        es.create(from, d1)?;
        es.create(from, d2)?;
        es.create(into, d3)?; // pre-existing into row
        es.create(from, d3)?; // collides with (into, d3)

        // entity_links: two re-points, one related_to re-point, and two
        // rows that would become self-links (dropped).
        el.create(&link(from, other, "same_entity"))?;
        el.create(&link(other, from, "same_entity"))?;
        el.create(&link(from, into, "same_entity"))?; // -> self-link, dropped
        el.create(&link(into, from, "same_entity"))?; // -> self-link, dropped
        el.create(&link(from, other, "related_to"))?;

        Ok((into, from, other))
    })
    .expect("seed fixture")
}

// (acceptance) Full fixture: no row references the deleted id, both names
// are aliased, the canonical row is unchanged, and the summary counts are
// exact.
#[test]
fn merge_full_fixture() {
    let db = in_memory_db();
    let (into, from, _other) = seed_full_fixture(&db);

    let summary = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), into, from))
        .expect("merge commits");

    // Summary: exact re-pointed / dropped counts.
    assert_eq!(summary.into_id, into);
    assert_eq!(summary.from_id, from);
    assert_eq!(summary.surviving_name, "Into");
    assert_eq!(summary.merged_name, "From");
    assert_eq!(summary.facts_repointed, 4);
    assert_eq!(summary.facts_dropped, 2);
    assert_eq!(summary.chunk_entities_repointed, 3);
    assert_eq!(summary.entity_sources_repointed, 3);
    assert_eq!(summary.entity_links_repointed, 3);
    assert_eq!(
        summary.aliases,
        vec!["From".to_string(), "Into".to_string()]
    );

    // No row in any dependent table references the deleted id.
    fn q(db: &Db, sql: &str, params: impl rusqlite::Params) -> i64 {
        db.with_conn(|conn| conn.query_row(sql, params, |r| r.get::<_, i64>(0)))
            .unwrap()
            .unwrap()
    }
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM facts WHERE subject_entity_id = ? OR object_entity_id = ?",
            rusqlite::params![from, from]
        ),
        0,
        "no fact may reference the deleted entity"
    );
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM chunk_entities WHERE entity_id = ?",
            rusqlite::params![from]
        ),
        0,
        "no chunk link may reference the deleted entity"
    );
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM entity_sources WHERE entity_id = ?",
            rusqlite::params![from]
        ),
        0,
        "no source may reference the deleted entity"
    );
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM entity_links WHERE subject_entity_id = ? OR target_entity_id = ?",
            rusqlite::params![from, from]
        ),
        0,
        "no link may reference the deleted entity"
    );

    // The re-pointed facts exist, and the collision triple is single.
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM facts WHERE subject_entity_id = ? AND object_entity_id = ? AND predicate = 'p6'",
            rusqlite::params![into, into]
        ),
        1,
        "the (into, into, p6) triple must be exactly one row"
    );
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM facts WHERE subject_entity_id = ? AND object_entity_id = ? AND predicate = 'p3'",
            rusqlite::params![into, into]
        ),
        1,
        "the (from, from, p3) fact re-pointed to (into, into, p3)"
    );
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM facts WHERE subject_entity_id = ? AND predicate = 'p1'",
            rusqlite::params![into]
        ),
        1,
        "the (from, other, p1) fact re-pointed to (into, other, p1)"
    );

    // The merged-away entity is gone; the survivor is unchanged.
    let (gone, survivor) = db
        .with_conn(
            |conn| -> Result<(Option<Entity>, Option<Entity>), DbError> {
                let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
                Ok((entities.get_by_id(from)?, entities.get_by_id(into)?))
            },
        )
        .unwrap()
        .unwrap();
    assert!(gone.is_none(), "the from entity must be deleted");
    let survivor = survivor.expect("the into entity must survive");
    assert_eq!(survivor.id, into);
    assert_eq!(survivor.name, "Into");
    assert_eq!(survivor.entity_type, "PERSON");
    assert_eq!(survivor.domain, "hr");

    // Both names are aliases of the survivor (ordered by alias).
    let aliases = db
        .with_conn(|conn| {
            let dao = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            dao.aliases_of(into)
        })
        .unwrap()
        .unwrap();
    assert_eq!(aliases, vec!["From".to_string(), "Into".to_string()]);
    assert_eq!(
        q(
            &db,
            "SELECT COUNT(*) FROM entity_links WHERE subject_entity_id = ? AND target_entity_id = ?",
            rusqlite::params![into, into]
        ),
        0,
        "no self-link may be created"
    );
}

// (acceptance) into_id == from_id -> Err, DB unmodified.
#[test]
fn merge_same_id_rejected() {
    let db = in_memory_db();
    let a = make_entity(&db, "PERSON", "Into", "hr");
    let before = db_state(&db);

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), a, a))
        .expect_err("must reject");
    assert!(
        matches!(err, DbError::MergePrecondition { .. }),
        "expected MergePrecondition, got {err:?}"
    );
    assert_eq!(before, db_state(&db), "the DB must be unmodified");
}

// (acceptance) a missing `from` id -> Err, DB unmodified.
#[test]
fn merge_missing_from_rejected() {
    let db = in_memory_db();
    let a = make_entity(&db, "PERSON", "Into", "hr");
    let before = db_state(&db);

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), a, 999_999))
        .expect_err("must reject");
    assert!(
        matches!(err, DbError::MergePrecondition { .. }),
        "expected MergePrecondition, got {err:?}"
    );
    assert_eq!(before, db_state(&db), "the DB must be unmodified");
}

// (acceptance) a missing `into` id -> Err, DB unmodified.
#[test]
fn merge_missing_into_rejected() {
    let db = in_memory_db();
    let b = make_entity(&db, "PERSON", "From", "hr");
    let before = db_state(&db);

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), 999_999, b))
        .expect_err("must reject");
    assert!(
        matches!(err, DbError::MergePrecondition { .. }),
        "expected MergePrecondition, got {err:?}"
    );
    assert_eq!(before, db_state(&db), "the DB must be unmodified");
}

// (acceptance) different type -> Err, DB unmodified.
#[test]
fn merge_different_type_rejected() {
    let db = in_memory_db();
    let a = make_entity(&db, "PERSON", "Into", "hr");
    let b = make_entity(&db, "ORGANIZATION", "From", "hr");
    let before = db_state(&db);

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), a, b))
        .expect_err("must reject");
    assert!(
        matches!(err, DbError::MergePrecondition { .. }),
        "expected MergePrecondition, got {err:?}"
    );
    assert_eq!(before, db_state(&db), "the DB must be unmodified");
}

// (acceptance) different domain -> Err, DB unmodified.
#[test]
fn merge_different_domain_rejected() {
    let db = in_memory_db();
    let a = make_entity(&db, "PERSON", "Into", "hr");
    let b = make_entity(&db, "PERSON", "From", "product");
    let before = db_state(&db);

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), a, b))
        .expect_err("must reject");
    assert!(
        matches!(err, DbError::MergePrecondition { .. }),
        "expected MergePrecondition, got {err:?}"
    );
    assert_eq!(before, db_state(&db), "the DB must be unmodified");
}

// (design D2) a mid-way SQL failure rolls the whole transaction back:
// the from-fact is not re-pointed and the from entity survives.
#[test]
fn merge_midway_failure_rolls_back() {
    let db = in_memory_db();
    let into = make_entity(&db, "PERSON", "Into", "hr");
    let from = make_entity(&db, "PERSON", "From", "hr");
    db.with_conn(|conn| -> Result<(), DbError> {
        let facts = FactDao::new(ConnectionOrTx::Connection(conn));
        facts.create(Some(from), "p", Some(into), "hr", None, None, None)?;
        Ok(())
    })
    .unwrap()
    .unwrap();

    let before = db_state(&db);

    // Force the first facts UPDATE to fail via a trigger.
    db.with_conn(|conn| {
        conn.execute(
            "CREATE TRIGGER merge_fail BEFORE UPDATE ON facts \
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
            [],
        )
        .unwrap();
    })
    .unwrap();

    let err = db
        .exec_tx(|tx| merge_entities(ConnectionOrTx::Transaction(&*tx), into, from))
        .expect_err("the forced failure must surface");
    assert!(matches!(err, DbError::Sqlite { .. }), "got {err:?}");

    db.with_conn(|conn| {
        conn.execute("DROP TRIGGER merge_fail", []).unwrap();
    })
    .unwrap();

    assert_eq!(
        before,
        db_state(&db),
        "a mid-way failure must roll back atomically"
    );
}
