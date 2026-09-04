//! Integration tests for the `facts` DAO ([`db::FactDao`]), relocated from
//! the inline `#[cfg(test)]` module in `crates/db/src/fact.rs` (change
//! `test-hygiene-phase-1`, task 1.2).
//!
//! The tests exercise only the public API; names and assertions are carried
//! over verbatim, so the move changes no behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use db::Db;
use db::document::DocumentDao;
use db::entity::EntityDao;
use db::test_util::in_memory_db;
use db::{ConnectionOrTx, DbError, FactDao, FactFilter};
use rusqlite::params;

/// Run `f` with a DAO bound to a pooled connection (checked out for the
/// closure's duration).
fn with_facts<T>(db: &Db, f: impl FnOnce(&FactDao<'_>) -> T) -> T {
    db.with_conn(|conn| f(&FactDao::new(ConnectionOrTx::Connection(conn))))
        .unwrap()
}

/// Create an entity through a pooled connection and return its id (the
/// fact endpoint FKs require existing entities; `foreign_keys=ON`).
fn insert_entity(db: &Db, entity_type: &str, name: &str, domain: &str) -> i64 {
    db.with_conn(|conn| {
        let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
        entities.create(entity_type, name, domain, None, None, None)
    })
    .unwrap()
    .unwrap()
}

/// Insert a document row and return its id (the `fact_sources`
/// `document_id` FK requires an existing document; `foreign_keys=ON`).
fn insert_document(db: &Db, path: &str) -> i64 {
    db.with_conn(|conn| {
        let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
        docs.create("markdown", path, None, None)
    })
    .unwrap()
    .unwrap()
}

/// Insert a `fact_sources` row (the FactSource DAO lands in task 1.14).
fn insert_fact_source(db: &Db, fact_id: i64, document_id: i64) {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO fact_sources (fact_id, document_id, quote) VALUES (?1, ?2, 'q')",
            params![fact_id, document_id],
        )
        .unwrap()
    })
    .unwrap();
}

/// Remove one `fact_sources` row (the weight-decrease path).
fn delete_fact_source(db: &Db, fact_id: i64, document_id: i64) {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM fact_sources WHERE fact_id = ?1 AND document_id = ?2",
            params![fact_id, document_id],
        )
        .unwrap()
    })
    .unwrap();
}

/// Set a fact's status directly (the DAO has no status update, so status
/// transitions are exercised with raw SQL).
fn set_fact_status(db: &Db, fact_id: i64, status: &str) {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE facts SET status = ? WHERE id = ?",
            params![status, fact_id],
        )
        .unwrap()
    })
    .unwrap();
}

/// The `fact_sources` row count of one fact.
fn source_count(db: &Db, fact_id: i64) -> i64 {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM fact_sources WHERE fact_id = ?",
            [fact_id],
            |r| r.get(0),
        )
        .unwrap()
    })
    .unwrap()
}

// (a1) create + get_by_id round-trip, all fields.
#[test]
fn create_then_get_by_id_round_trip() {
    let db = in_memory_db();
    let subject = insert_entity(&db, "PERSON", "Alice", "hr");
    let object = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        let id = facts
            .create(
                Some(subject),
                "works_at",
                Some(object),
                "hr",
                Some(r#"{"threshold":100}"#),
                Some("2024-01-01"),
                Some("2024-12-31"),
            )
            .unwrap();
        let fact = facts
            .get_by_id(id)
            .unwrap()
            .expect("created fact must exist");
        assert_eq!(fact.id, id);
        assert_eq!(fact.subject_entity_id, Some(subject));
        assert_eq!(fact.predicate, "works_at");
        assert_eq!(fact.object_entity_id, Some(object));
        assert_eq!(fact.domain, "hr");
        assert_eq!(fact.metadata_json.as_deref(), Some(r#"{"threshold":100}"#));
        assert_eq!(fact.status, "approved", "the constructor's default status");
        assert_eq!(fact.valid_from.as_deref(), Some("2024-01-01"));
        assert_eq!(fact.valid_to.as_deref(), Some("2024-12-31"));
        assert_eq!(fact.weight, 1, "schema default");
        assert!(
            !fact.created_at.is_empty(),
            "created_at default must be set"
        );
        assert!(
            !fact.updated_at.is_empty(),
            "updated_at default must be set"
        );
        assert_eq!(facts.get_by_id(999_999).unwrap(), None);
    });
}

// (a2) create: metadata variants (None / JSON / empty string) and NULL
// endpoints (stored as NULL, not the zero value 0).
#[test]
fn create_metadata_and_null_endpoint_variants() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        let no_meta = facts
            .create(Some(subj), "founded_by", Some(obj), "hr", None, None, None)
            .unwrap();
        let json_meta = facts
            .create(
                Some(subj),
                "works_at",
                Some(obj),
                "hr",
                Some(r#"{"threshold":100}"#),
                None,
                None,
            )
            .unwrap();
        let empty_meta = facts
            .create(Some(subj), "owns", Some(obj), "hr", Some(""), None, None)
            .unwrap();
        let nulls = facts
            .create(
                None,
                "located_in",
                None,
                "geo",
                None,
                Some("2024-01-01"),
                None,
            )
            .unwrap();

        assert_eq!(
            facts.get_by_id(no_meta).unwrap().unwrap().metadata_json,
            None
        );
        assert_eq!(
            facts
                .get_by_id(json_meta)
                .unwrap()
                .unwrap()
                .metadata_json
                .as_deref(),
            Some(r#"{"threshold":100}"#)
        );
        assert_eq!(
            facts
                .get_by_id(empty_meta)
                .unwrap()
                .unwrap()
                .metadata_json
                .as_deref(),
            Some(""),
            "an empty string is a value, not NULL"
        );
        let null_fact = facts.get_by_id(nulls).unwrap().unwrap();
        assert_eq!(null_fact.subject_entity_id, None, "None endpoint → NULL");
        assert_eq!(null_fact.object_entity_id, None, "None endpoint → NULL");
        assert_eq!(null_fact.valid_to, None);
    });
}

// (a3) create: the unique key (subject, object, predicate) is enforced —
// a plain create (unlike create_or_ignore) fails on a duplicate.
#[test]
fn create_duplicate_key_fails() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        facts
            .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
            .unwrap();
        let err = facts
            .create(Some(subj), "works_at", Some(obj), "it", None, None, None)
            .expect_err("duplicate unique key must fail");
        assert!(matches!(err, DbError::Sqlite { .. }));
    });
}

// (a4) delete: true on hit, false on miss; the fact_sources rows cascade
// per the schema FK.
#[test]
fn delete_cascades_sources_and_reports_missing() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let id = with_facts(&db, |facts| {
        facts
            .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
            .unwrap()
    });
    let doc1 = insert_document(&db, "/docs/a.md");
    let doc2 = insert_document(&db, "/docs/b.md");
    insert_fact_source(&db, id, doc1);
    insert_fact_source(&db, id, doc2);

    with_facts(&db, |facts| {
        assert!(facts.delete(id).unwrap(), "existing id must report true");
        assert_eq!(facts.get_by_id(id).unwrap(), None);
        assert!(
            !facts.delete(id).unwrap(),
            "second delete must report false"
        );
    });
    assert_eq!(source_count(&db, id), 0, "fact_sources must cascade");
}

// (a5) list_by_entity_id: both sides, approved only, id-DESC order
// (created_at ties at second resolution).
#[test]
fn list_by_entity_id_approved_only_both_sides() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let other = insert_entity(&db, "ORGANIZATION", "Beta", "it");

    let (works, manages, owns, knows1, knows2) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "manages", Some(other), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "owns", Some(other), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "knows_1", Some(other), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "knows_2", Some(other), "hr", None, None, None)
                .unwrap(),
        )
    });
    set_fact_status(&db, manages, "draft");
    set_fact_status(&db, owns, "rejected");

    with_facts(&db, |facts| {
        let subject_side: Vec<i64> = facts
            .list_by_entity_id(subj)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            subject_side,
            vec![knows2, knows1, works],
            "approved only, id DESC (created_at ties)"
        );
        let object_side: Vec<i64> = facts
            .list_by_entity_id(obj)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(object_side, vec![works], "object side sees its fact");
        assert!(
            facts.list_by_entity_id(999_999).unwrap().is_empty(),
            "unknown entity → empty"
        );
    });
}

// (a6) list_all: approved only, id ASC order.
#[test]
fn list_all_approved_only_id_order() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (f1, f2, f3) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
                .unwrap(),
        )
    });
    set_fact_status(&db, f2, "draft");

    with_facts(&db, |facts| {
        let ids: Vec<i64> = facts
            .list_all()
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec![f1, f3], "approved only, id ASC");
    });
}

// (a7) get_by_ids: several, missing ids absent, empty input.
#[test]
fn get_by_ids() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (f1, f2, f3) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
                .unwrap(),
        )
    });

    with_facts(&db, |facts| {
        let found = facts.get_by_ids(&[f2, f1]).unwrap();
        let got: HashSet<i64> = found.iter().map(|f| f.id).collect();
        assert_eq!(got, [f1, f2].into_iter().collect());
        let p1 = found
            .iter()
            .find(|f| f.id == f1)
            .expect("f1 must be present");
        assert_eq!(p1.predicate, "p1");

        assert_eq!(
            facts.get_by_ids(&[f1, 999_999, f3]).unwrap().len(),
            2,
            "missing ids are absent"
        );
        assert!(facts.get_by_ids(&[]).unwrap().is_empty());
    });
}

// (a8) get_by_ids across the D9 batch boundary (501 ids → 2 statements).
#[test]
fn get_by_ids_batches_over_500() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let mut ids: Vec<i64> = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..501 {
            ids.push(facts.create(
                Some(subj),
                &format!("p-{i}"),
                Some(obj),
                "hr",
                None,
                None,
                None,
            )?);
        }
        Ok(())
    })
    .expect("seed commits");

    with_facts(&db, |facts| {
        let requested: Vec<i64> = ids.iter().rev().copied().collect();
        let found = facts.get_by_ids(&requested).unwrap();
        assert_eq!(found.len(), 501);
        let got: HashSet<i64> = found.iter().map(|f| f.id).collect();
        assert_eq!(got, ids.into_iter().collect::<HashSet<i64>>());
    });
}

// (g) count: all statuses, 0 on an empty table.
#[test]
fn count_all_statuses() {
    let db = in_memory_db();
    with_facts(&db, |facts| {
        assert_eq!(facts.count().unwrap(), 0);
    });
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (f1, f3) = with_facts(&db, |facts| {
        let f1 = facts
            .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
            .unwrap();
        // p2 stays approved; only its existence matters for the count.
        facts
            .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
            .unwrap();
        let f3 = facts
            .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
            .unwrap();
        (f1, f3)
    });
    set_fact_status(&db, f1, "draft");
    set_fact_status(&db, f3, "rejected");
    with_facts(&db, |facts| {
        assert_eq!(facts.count().unwrap(), 3, "count ignores status");
    });
}

// (b1) create_or_ignore: repeated calls with the same key → one row,
// the same id, status 'approved' (D5).
#[test]
fn create_or_ignore_repeated_returns_same_id() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        let id1 = facts
            .create_or_ignore(Some(subj), "works_at", Some(obj), "hr", None, None, None)
            .unwrap();
        let id2 = facts
            .create_or_ignore(Some(subj), "works_at", Some(obj), "hr", None, None, None)
            .unwrap();
        assert_eq!(id1, id2, "conflict must return the existing id");
        assert_eq!(facts.count().unwrap(), 1, "no duplicate row");
        assert_eq!(facts.get_by_id(id1).unwrap().unwrap().status, "approved");
    });
}

// (b2) create_or_ignore: a conflict must NOT overwrite the existing row
// (status and metadata survive).
#[test]
fn create_or_ignore_conflict_keeps_existing_row() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    let id = with_facts(&db, |facts| {
        facts
            .create_or_ignore(
                Some(subj),
                "works_at",
                Some(obj),
                "hr",
                Some(r#"{"v":1}"#),
                None,
                None,
            )
            .unwrap()
    });
    set_fact_status(&db, id, "pending");

    with_facts(&db, |facts| {
        let id2 = facts
            .create_or_ignore(
                Some(subj),
                "works_at",
                Some(obj),
                "hr",
                Some(r#"{"v":2}"#),
                Some("2025-01-01"),
                None,
            )
            .unwrap();
        assert_eq!(id2, id);
        let fact = facts.get_by_id(id).unwrap().unwrap();
        assert_eq!(fact.status, "pending", "status must not be overwritten");
        assert_eq!(
            fact.metadata_json.as_deref(),
            Some(r#"{"v":1}"#),
            "metadata must not be overwritten"
        );
        assert_eq!(fact.valid_from, None, "validity must not be overwritten");
    });
}

// (b3) create_or_ignore: NULL endpoints are never de-duplicated (SQLite
// unique indexes treat NULLs as distinct) — every call inserts.
#[test]
fn create_or_ignore_null_endpoints_always_insert() {
    let db = in_memory_db();
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        let id1 = facts
            .create_or_ignore(None, "located_in", Some(obj), "geo", None, None, None)
            .unwrap();
        let id2 = facts
            .create_or_ignore(None, "located_in", Some(obj), "geo", None, None, None)
            .unwrap();
        assert_ne!(id1, id2, "NULL subject cannot hit the unique index");
        assert_eq!(facts.count().unwrap(), 2);
    });
}

// (b4) create_or_ignore: the unique key is the full triple — a different
// predicate is a different fact.
#[test]
fn create_or_ignore_distinct_predicate_inserts() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    with_facts(&db, |facts| {
        let id1 = facts
            .create_or_ignore(Some(subj), "p1", Some(obj), "hr", None, None, None)
            .unwrap();
        let id2 = facts
            .create_or_ignore(Some(subj), "p2", Some(obj), "hr", None, None, None)
            .unwrap();
        assert_ne!(id1, id2, "the predicate is part of the unique key");
        assert_eq!(facts.count().unwrap(), 2);
    });
}

// (c1) list_by_entity_ids: a fact is attached to its subject AND its
// object entry, approved only; single-id and multi-id calls are both
// asserted.
#[test]
fn list_by_entity_ids_groups_subject_and_object() {
    let db = in_memory_db();
    let alice = insert_entity(&db, "PERSON", "Alice", "hr");
    let bob = insert_entity(&db, "PERSON", "Bob", "hr");
    let acme = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    let (works, employed, knows) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(alice), "works_at", Some(acme), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(bob), "employed_by", Some(acme), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(alice), "knows", Some(bob), "hr", None, None, None)
                .unwrap(),
        )
    });
    set_fact_status(&db, knows, "draft");

    with_facts(&db, |facts| {
        // Subject side: alice's approved fact only (the draft is
        // excluded); the fact is also attached to its object entry.
        let map = facts.list_by_entity_ids(&[alice]).unwrap();
        assert_eq!(map.get(&alice).map(Vec::len), Some(1));
        assert_eq!(map.get(&alice).unwrap()[0].id, works);
        assert_eq!(map.get(&acme).map(Vec::len), Some(1));
        assert_eq!(map.get(&acme).unwrap()[0].id, works);

        // Object side: acme is the object of both approved facts.
        let map = facts.list_by_entity_ids(&[acme]).unwrap();
        let acme_ids: Vec<i64> = map
            .get(&acme)
            .expect("acme must be present")
            .iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(acme_ids.len(), 2, "both approved facts attach to acme");
        assert!(acme_ids.contains(&works) && acme_ids.contains(&employed));

        // bob's draft fact is excluded, his approved one is present.
        let map = facts.list_by_entity_ids(&[bob]).unwrap();
        assert_eq!(map.get(&bob).map(Vec::len), Some(1));
        assert_eq!(map.get(&bob).unwrap()[0].id, employed);

        // Ids with no facts are absent from the map.
        assert!(facts.list_by_entity_ids(&[999_999]).unwrap().is_empty());

        // Multi-id: one call covers every requested id (subject AND
        // object side) — each entity sees exactly its approved facts,
        // the draft is still excluded, and a fact-free id has no entry.
        let map = facts
            .list_by_entity_ids(&[alice, bob, acme, 999_999])
            .unwrap();
        assert_eq!(map.get(&alice).map(Vec::len), Some(1));
        assert_eq!(map.get(&alice).unwrap()[0].id, works);
        assert_eq!(map.get(&bob).map(Vec::len), Some(1));
        assert_eq!(map.get(&bob).unwrap()[0].id, employed);
        let acme_ids: Vec<i64> = map
            .get(&acme)
            .expect("acme must be present")
            .iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(acme_ids.len(), 2, "both approved facts attach to acme");
        assert!(acme_ids.contains(&works) && acme_ids.contains(&employed));
        assert!(!map.contains_key(&999_999), "no facts → no map entry");
    });
}

// (c2) list_by_entity_ids: empty input → empty map.
#[test]
fn list_by_entity_ids_empty_input() {
    let db = in_memory_db();
    with_facts(&db, |facts| {
        assert!(facts.list_by_entity_ids(&[]).unwrap().is_empty());
    });
}

// (c3) list_by_entity_ids: repeated input ids must not change the result
// (ids are de-duplicated before the query), for single- and multi-id
// inputs alike.
#[test]
fn list_by_entity_ids_deduplicates_input_ids() {
    let db = in_memory_db();
    let alice = insert_entity(&db, "PERSON", "Alice", "hr");
    let bob = insert_entity(&db, "PERSON", "Bob", "hr");
    let acme = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    with_facts(&db, |facts| {
        facts
            .create(Some(alice), "works_at", Some(acme), "hr", None, None, None)
            .unwrap();
        facts
            .create(Some(bob), "employed_by", Some(acme), "hr", None, None, None)
            .unwrap();
        let map = facts.list_by_entity_ids(&[alice, alice, alice]).unwrap();
        assert_eq!(
            map.get(&alice).map(Vec::len),
            Some(1),
            "repeated input ids must not duplicate the fact"
        );
        // Multi-id with repeats: the result is identical to the
        // de-duplicated input — every entity sees each fact exactly once.
        let map = facts.list_by_entity_ids(&[alice, bob, alice, bob]).unwrap();
        assert_eq!(
            map.get(&alice).map(Vec::len),
            Some(1),
            "repeated input ids must not duplicate the fact"
        );
        assert_eq!(
            map.get(&bob).map(Vec::len),
            Some(1),
            "repeated input ids must not duplicate the fact"
        );
        assert_eq!(
            map.get(&acme).map(Vec::len),
            Some(2),
            "both facts attach to acme, once each"
        );
    });
}

// (c4) REGRESSION (fixed by task 1.17): the SQL carries TWO `IN` lists
// (subject, object) with n placeholders each, and the parameters must be
// bound full-list-then-full-list. An earlier implementation passed them
// INTERLEAVED as [b0, b0, b1, b1, ...] (`flat_map(|id| [*id, *id])`), so
// positional binding filled the subject list with the first half of the
// batch and the object list with the second half — half of the requested
// ids were missing from each list. The batch order comes from a HashSet (random per call), so EVERY
// multi-id call was order-dependent (this is what made the first version
// of the (c1)/(c3) tests flake).
//
// Deterministic reproduction: facts A→B and B→A; under either batch
// order exactly one fact is missed, so the map holds 2 entries instead
// of 4.
#[test]
fn list_by_entity_ids_multi_id_regression() {
    let db = in_memory_db();
    let a = insert_entity(&db, "PERSON", "Alice", "hr");
    let b = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    with_facts(&db, |facts| {
        facts
            .create(Some(a), "p_ab", Some(b), "hr", None, None, None)
            .unwrap();
        facts
            .create(Some(b), "p_ba", Some(a), "hr", None, None, None)
            .unwrap();
        let map = facts.list_by_entity_ids(&[a, b]).unwrap();
        let entries: usize = map.values().map(Vec::len).sum();
        assert_eq!(entries, 4, "both facts under both entities");
    });
}

// (c5) REGRESSION (fixed by task 1.17): with more than 500 unique ids,
// `list_by_entity_ids` runs one query per 500-id batch and a fact whose
// subject and object land in DIFFERENT batches is selected by both
// queries — it must be attached to the result map exactly once
// (de-duplicated by fact id). The duplication was masked by bug #1 (the
// parameter interleaving) and surfaced once that was fixed.
#[test]
fn list_by_entity_ids_batches_over_500_no_duplicates() {
    let db = in_memory_db();
    let mut ids: Vec<i64> = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..501 {
            let s = entities.create("PERSON", &format!("S-{i}"), "hr", None, None, None)?;
            let o = entities.create("ORGANIZATION", &format!("O-{i}"), "hr", None, None, None)?;
            facts.create(Some(s), &format!("p-{i}"), Some(o), "hr", None, None, None)?;
            ids.extend([s, o]);
        }
        Ok(())
    })
    .expect("seed commits");

    let map = with_facts(&db, |facts| facts.list_by_entity_ids(&ids).unwrap());
    for id in &ids {
        assert_eq!(
            map.get(id).map(Vec::len),
            Some(1),
            "entity {id}: exactly one fact — no cross-batch duplication"
        );
    }
}

// (d) validate_fact_domain: matching domains → true; mismatch,
// case/whitespace normalization, and missing endpoints → false.
#[test]
fn validate_fact_domain() {
    let db = in_memory_db();
    let alice = insert_entity(&db, "PERSON", "Alice", "hr");
    let bob = insert_entity(&db, "PERSON", "Bob", "hr");
    let carol = insert_entity(&db, "PERSON", "Carol", "policy");
    let dave = insert_entity(&db, "PERSON", "Dave", " HR ");

    with_facts(&db, |facts| {
        assert!(
            facts.validate_fact_domain(alice, bob, "hr").unwrap(),
            "same domain must pass"
        );
        assert!(
            !facts.validate_fact_domain(alice, carol, "hr").unwrap(),
            "cross-domain object must fail"
        );
        assert!(
            !facts.validate_fact_domain(carol, bob, "hr").unwrap(),
            "cross-domain subject must fail"
        );
        assert!(
            facts.validate_fact_domain(alice, bob, "HR").unwrap(),
            "the fact domain is normalized (case)"
        );
        assert!(
            facts.validate_fact_domain(alice, bob, "  hr  ").unwrap(),
            "the fact domain is normalized (whitespace)"
        );
        assert!(
            facts.validate_fact_domain(alice, dave, "hr").unwrap(),
            "the stored entity domain is normalized too"
        );
        assert!(
            !facts.validate_fact_domain(alice, 999_999, "hr").unwrap(),
            "missing object entity must fail"
        );
        assert!(
            !facts.validate_fact_domain(999_999, bob, "hr").unwrap(),
            "missing subject entity must fail"
        );
    });
}

// (e1) find_orphaned_fact_ids: no-source facts only, exclude_approved,
// candidate scoping. Facts with live sources (and live entities) are
// never reported.
#[test]
fn find_orphaned_fact_ids() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (sourced, orphan_approved, orphan_draft) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "drafted", Some(obj), "hr", None, None, None)
                .unwrap(),
        )
    });
    let doc = insert_document(&db, "/docs/a.md");
    insert_fact_source(&db, sourced, doc);
    set_fact_status(&db, orphan_draft, "draft");

    with_facts(&db, |facts| {
        assert_eq!(
            facts.find_orphaned_fact_ids(false, &[]).unwrap(),
            vec![orphan_approved, orphan_draft],
            "all no-source facts, id order"
        );
        assert_eq!(
            facts.find_orphaned_fact_ids(true, &[]).unwrap(),
            vec![orphan_draft],
            "approved facts are excluded"
        );
        assert_eq!(
            facts.find_orphaned_fact_ids(false, &[sourced]).unwrap(),
            Vec::<i64>::new(),
            "a sourced fact is not orphaned"
        );
        assert_eq!(
            facts
                .find_orphaned_fact_ids(false, &[sourced, orphan_approved, 999_999])
                .unwrap(),
            vec![orphan_approved],
            "candidates scope the search"
        );
    });
}

// (e2) delete_orphaned_facts: deletes exactly the listed ids (empty → 0);
// a fact with live entities and sources is never touched.
#[test]
fn delete_orphaned_facts() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (sourced, orphan1, orphan2) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "drafted", Some(obj), "hr", None, None, None)
                .unwrap(),
        )
    });
    let doc = insert_document(&db, "/docs/a.md");
    insert_fact_source(&db, sourced, doc);
    set_fact_status(&db, orphan2, "draft");

    with_facts(&db, |facts| {
        assert_eq!(facts.delete_orphaned_facts(&[]).unwrap(), 0);
        assert_eq!(
            facts
                .delete_orphaned_facts(&[orphan1, orphan2, 999_999])
                .unwrap(),
            2,
            "exactly the listed existing ids"
        );
        assert_eq!(facts.get_by_id(orphan1).unwrap(), None);
        assert_eq!(facts.get_by_id(orphan2).unwrap(), None);
        assert!(
            facts.get_by_id(sourced).unwrap().is_some(),
            "the sourced fact with live entities must survive"
        );
        assert_eq!(facts.count().unwrap(), 1);
    });
}

// (e3) find_orphaned_fact_ids + delete_orphaned_facts across the D9
// batch boundary (501 candidates → 2 statements each).
#[test]
fn orphan_cleanup_batches_over_500() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let mut ids: Vec<i64> = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..501 {
            ids.push(facts.create(
                Some(subj),
                &format!("p-{i}"),
                Some(obj),
                "hr",
                None,
                None,
                None,
            )?);
        }
        Ok(())
    })
    .expect("seed commits");

    with_facts(&db, |facts| {
        let candidates: Vec<i64> = ids.iter().rev().copied().collect();
        let orphaned = facts.find_orphaned_fact_ids(false, &candidates).unwrap();
        assert_eq!(orphaned.len(), 501, "all sourceless facts are orphaned");

        assert_eq!(
            facts.delete_orphaned_facts(&candidates).unwrap(),
            501,
            "every listed fact is deleted"
        );
        assert_eq!(facts.count().unwrap(), 0);
    });
}

/// Search fixture: 4 entities and 6 facts covering every filter axis —
/// approved/draft statuses, hr/it domains, LIKE wildcards in predicates,
/// and a fact whose BOTH endpoint names match one pattern (the fact a
/// naive `INNER JOIN` duplicated in the page).
fn seed_search_db() -> (Db, [i64; 6]) {
    let db = in_memory_db();
    let e1 = insert_entity(&db, "PERSON", "Alpha One", "hr");
    let e2 = insert_entity(&db, "ORGANIZATION", "Alpha Two", "hr");
    let e3 = insert_entity(&db, "ORGANIZATION", "Beta Corp", "it");
    let e4 = insert_entity(&db, "ORGANIZATION", "Gamma Lab", "it");
    let ids: [i64; 6] = with_facts(&db, |facts| {
        [
            facts
                .create(Some(e1), "works_at", Some(e2), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(e1), "manages", Some(e3), "it", None, None, None)
                .unwrap(),
            facts
                .create(Some(e3), "supplies", Some(e2), "it", None, None, None)
                .unwrap(),
            facts
                .create(Some(e4), "funds", Some(e3), "it", None, None, None)
                .unwrap(),
            facts
                .create(Some(e1), "a_b", Some(e4), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(e1), "axb", Some(e4), "hr", None, None, None)
                .unwrap(),
        ]
    });
    set_fact_status(&db, ids[4], "draft");
    (db, ids)
}

// (e4, search) no filter: every fact (all statuses), id order; page and
// total agree.
#[test]
fn search_paginated_no_filter() {
    let (db, ids) = seed_search_db();
    with_facts(&db, |facts| {
        let (page, total) = facts
            .search_paginated(0, 100, &FactFilter::default())
            .unwrap();
        assert_eq!(total, 6, "no filter matches every status");
        let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids, ids.to_vec(), "id ASC order");
    });
}

// (e5, search) REGRESSION: a fact whose subject AND object names both
// match the entity-name filter must appear ONCE in the page and ONCE in
// the total (the legacy `INNER JOIN` duplicated it in the page while its
// COUNT(DISTINCT) total did not).
#[test]
fn search_paginated_entity_name_both_endpoints_match_once() {
    let (db, ids) = seed_search_db();
    with_facts(&db, |facts| {
        let filter = FactFilter {
            entity_name: Some("alpha".into()),
            ..Default::default()
        };
        let (page, total) = facts.search_paginated(0, 100, &filter).unwrap();
        // f0 (Alpha One → Alpha Two), f1, f4, f5 (subject Alpha One) and
        // f2 (object Alpha Two); f3 (Beta/Gamma) does not match.
        assert_eq!(total, 5);
        let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids.len(), 5, "page and total must agree");
        assert_eq!(
            page_ids.iter().filter(|id| **id == ids[0]).count(),
            1,
            "the both-endpoints-matching fact must appear exactly once"
        );
        let expected: HashSet<i64> = [ids[0], ids[1], ids[2], ids[4], ids[5]]
            .into_iter()
            .collect();
        assert_eq!(page_ids.into_iter().collect::<HashSet<_>>(), expected);
    });
}

// (e6, search) predicate filter: case-insensitive substring, and LIKE
// wildcards in the input match LITERALLY (escaped).
#[test]
fn search_paginated_predicate_filter() {
    let (db, ids) = seed_search_db();
    with_facts(&db, |facts| {
        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    predicate: Some("WORKS".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1, "case-insensitive substring");
        assert_eq!(page[0].id, ids[0]);

        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    predicate: Some("a_b".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1, "the underscore must be matched literally");
        assert_eq!(page[0].id, ids[4], "'axb' must not match the pattern 'a_b'");
    });
}

// (e7, search) status and domain filters (exact match), combined with the
// predicate filter.
#[test]
fn search_paginated_status_domain_and_combined_filters() {
    let (db, ids) = seed_search_db();
    with_facts(&db, |facts| {
        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    status: Some("draft".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(page[0].id, ids[4]);

        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    status: Some("approved".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 5);

        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    domain: Some("it".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 3);
        let page_ids: HashSet<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids, [ids[1], ids[2], ids[3]].into_iter().collect());

        let (page, total) = facts
            .search_paginated(
                0,
                100,
                &FactFilter {
                    predicate: Some("a".into()),
                    status: Some("approved".into()),
                    domain: Some("hr".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 2, "hr + approved + predicate containing 'a'");
        let page_ids: HashSet<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids, [ids[0], ids[5]].into_iter().collect());
    });
}

// (e8, search) pagination windows: page contents follow id order, the
// total stays stable, a window past the end is empty.
#[test]
fn search_paginated_windows() {
    let (db, ids) = seed_search_db();
    with_facts(&db, |facts| {
        let (page, total) = facts
            .search_paginated(0, 2, &FactFilter::default())
            .unwrap();
        assert_eq!(total, 6);
        let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids, vec![ids[0], ids[1]]);

        let (page, total) = facts
            .search_paginated(4, 2, &FactFilter::default())
            .unwrap();
        assert_eq!(total, 6);
        let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
        assert_eq!(page_ids, vec![ids[4], ids[5]]);

        let (page, total) = facts
            .search_paginated(6, 2, &FactFilter::default())
            .unwrap();
        assert!(page.is_empty(), "past the end → empty page");
        assert_eq!(total, 6, "the total does not follow the window");
    });
}

// (z1) recompute_weights: weight = COUNT(fact_sources) (0 when none),
// decreases after a source is removed, returns the rows updated; empty
// input is a no-op.
#[test]
fn recompute_weights() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let (f1, f2) = with_facts(&db, |facts| {
        (
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap(),
            facts
                .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                .unwrap(),
        )
    });
    let doc1 = insert_document(&db, "/docs/a.md");
    let doc2 = insert_document(&db, "/docs/b.md");
    insert_fact_source(&db, f1, doc1);
    insert_fact_source(&db, f1, doc2);

    with_facts(&db, |facts| {
        assert_eq!(
            facts.recompute_weights(&[]).unwrap(),
            0,
            "empty input is a no-op"
        );
        assert_eq!(
            facts.recompute_weights(&[f1, f2]).unwrap(),
            2,
            "every listed fact is updated"
        );
        assert_eq!(
            facts.get_by_id(f1).unwrap().unwrap().weight,
            2,
            "one weight per source"
        );
        assert_eq!(
            facts.get_by_id(f2).unwrap().unwrap().weight,
            0,
            "no sources → weight 0 (the schema default 1 is replaced)"
        );

        delete_fact_source(&db, f1, doc1);
        facts.recompute_weights(&[f1]).unwrap();
        assert_eq!(
            facts.get_by_id(f1).unwrap().unwrap().weight,
            1,
            "the weight follows the source count down"
        );
    });
}

// (z2) recompute_weights across the D9 batch boundary (501 facts → 2
// statements; 500 × 2 = 1000 parameters max per statement stays far
// below SQLite's 32766 bound).
#[test]
fn recompute_weights_batches_over_500() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
    let doc = insert_document(&db, "/docs/batch.md");
    let mut fact_ids: Vec<i64> = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..501 {
            let id = facts.create(
                Some(subj),
                &format!("p-{i}"),
                Some(obj),
                "hr",
                None,
                None,
                None,
            )?;
            tx.execute(
                "INSERT INTO fact_sources (fact_id, document_id) VALUES (?1, ?2)",
                params![id, doc],
            )?;
            fact_ids.push(id);
        }
        Ok(())
    })
    .expect("seed commits");

    with_facts(&db, |facts| {
        assert_eq!(
            facts.recompute_weights(&fact_ids).unwrap(),
            501,
            "every listed fact is updated across the batch boundary"
        );
        let weight_one: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM facts WHERE weight = 1", [], |r| {
                    r.get(0)
                })
            })
            .unwrap()
            .unwrap();
        assert_eq!(weight_one, 501, "every fact has exactly one source");
    });
}

// The DAO works over a transaction: commit and rollback paths
// (house pattern, as in the sibling DAOs).
#[test]
fn create_inside_transaction() {
    let db = in_memory_db();
    let subj = insert_entity(&db, "PERSON", "Alice", "hr");
    let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

    db.exec_tx(|tx| -> Result<(), DbError> {
        let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
        facts.create(Some(subj), "tx_commit", Some(obj), "hr", None, None, None)?;
        Ok(())
    })
    .expect("commit");

    let err = db
        .exec_tx(|tx| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            facts.create(Some(subj), "tx_rollback", Some(obj), "hr", None, None, None)?;
            // A genuine failure: UNIQUE(subject, object, predicate).
            facts.create(Some(subj), "tx_rollback", Some(obj), "hr", None, None, None)?;
            Ok(())
        })
        .expect_err("closure error must surface");
    assert!(matches!(err, DbError::Sqlite { .. }));

    with_facts(&db, |facts| {
        assert_eq!(facts.count().unwrap(), 1, "only the committed fact");
        let (page, total) = facts
            .search_paginated(
                0,
                10,
                &FactFilter {
                    predicate: Some("tx_rollback".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 0, "the rolled-back fact must be gone");
        assert!(page.is_empty());
    });
}
