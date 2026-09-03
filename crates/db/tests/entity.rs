//! Integration tests for the `entities` DAO ([`db::EntityDao`]), relocated
//! from the inline `#[cfg(test)]` module in `crates/db/src/entity.rs`
//! (change `test-hygiene-phase-2`, task 2.2).
//!
//! The tests exercise only the public API (plus the always-compiled
//! [`db::test_util`] database fixtures); names and assertions are carried
//! over verbatim, so the move changes no behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use db::entity::{EntityDao, EntityFilter};
use db::test_util::{in_memory_db, temp_file_db};
use db::{ConnectionOrTx, Db, DbError};

/// Run `f` with a DAO bound to a pooled connection (checked out for the
/// closure's duration).
fn with_entities<T>(db: &Db, f: impl FnOnce(&EntityDao<'_>) -> T) -> T {
    db.with_conn(|conn| f(&EntityDao::new(ConnectionOrTx::Connection(conn))))
        .unwrap()
}

/// Insert a document row and return its id (needed for `entity_sources`).
fn insert_document(db: &Db, path: &str) -> i64 {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO documents (source_type, original_path) VALUES ('markdown', ?)",
            [path],
        )
        .unwrap();
        conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .unwrap()
    })
    .unwrap()
}

/// Insert a fact row directly (the Fact DAO lands in task 1.6).
fn insert_fact(db: &Db, subject: i64, predicate: &str, object: i64) {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO facts (subject_entity_id, predicate, object_entity_id) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![subject, predicate, object],
        )
        .unwrap()
    })
    .unwrap();
}

/// Insert a fact with a NULL subject (allowed by the v5 schema) — the
/// regression trigger for the oracle's `NOT IN` bug.
fn insert_null_subject_fact(db: &Db, predicate: &str, object: i64) {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO facts (predicate, object_entity_id) VALUES (?1, ?2)",
            rusqlite::params![predicate, object],
        )
        .unwrap()
    })
    .unwrap();
}

/// Link an entity to a document via `entity_sources`.
fn insert_entity_source(db: &Db, entity_id: i64, document_id: i64) {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO entity_sources (entity_id, document_id) VALUES (?1, ?2)",
            rusqlite::params![entity_id, document_id],
        )
        .unwrap()
    })
    .unwrap();
}

// (a) create + get_by_id round-trip, all fields.
#[test]
fn create_then_get_by_id_round_trip() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create(
                "PERSON",
                "Alice Smith",
                "hr",
                Some("HR lead"),
                Some(0.92),
                Some(r#"{"source":"llm"}"#),
            )
            .unwrap();
        let ent = entities
            .get_by_id(id)
            .unwrap()
            .expect("created entity must exist");
        assert_eq!(ent.id, id);
        assert_eq!(ent.entity_type, "PERSON");
        assert_eq!(ent.name, "Alice Smith");
        assert_eq!(ent.domain, "hr");
        assert_eq!(ent.description.as_deref(), Some("HR lead"));
        assert_eq!(ent.confidence, Some(0.92));
        assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"source":"llm"}"#));
        assert!(!ent.created_at.is_empty(), "created_at default must be set");
        assert_eq!(entities.get_by_id(999_999).unwrap(), None);
    });
}

// (b1) get_by_name: full unique key (type, name, domain).
#[test]
fn get_by_name_full_key() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        let found = entities
            .get_by_name("PERSON", "Alice", "hr")
            .unwrap()
            .expect("exact key must match");
        assert_eq!(found.id, id);

        // Same name+domain, different type → no match (full key).
        entities
            .create("ORGANIZATION", "Alice", "hr", None, None, None)
            .unwrap();
        assert_eq!(
            entities
                .get_by_name("ORGANIZATION", "Alice", "policy")
                .unwrap(),
            None,
            "wrong domain must not match"
        );
        assert_eq!(
            entities.get_by_name("POLICY", "Alice", "hr").unwrap(),
            None,
            "wrong type must not match"
        );
    });
}

// (b2) get_by_name_fold: case + whitespace normalization.
#[test]
fn get_by_name_fold_normalizes_case_and_whitespace() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create("PERSON", "John Smith", "hr", None, None, None)
            .unwrap();
        for query in ["John Smith", "john smith", "JOHN SMITH", "  john   smith  "] {
            let found = entities
                .get_by_name_fold(query, "hr")
                .unwrap()
                .unwrap_or_else(|| panic!("query {query:?} must match"));
            assert_eq!(found.id, id, "query {query:?}");
        }
        assert_eq!(
            entities.get_by_name_fold("John Smith", "policy").unwrap(),
            None,
            "wrong domain must not match"
        );
        assert_eq!(
            entities.get_by_name_fold("Nobody", "hr").unwrap(),
            None,
            "unknown name must not match"
        );
    });
}

// (b3) list_by_name_fold: all domains, ordered by id.
#[test]
fn list_by_name_fold_all_domains_ordered_by_id() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id_hr = entities
            .create("PERSON", "Jane Doe", "hr", None, None, None)
            .unwrap();
        let id_policy = entities
            .create("PERSON", "Jane Doe", "policy", None, None, None)
            .unwrap();

        for query in ["Jane Doe", "jane doe", "JANE DOE"] {
            let found = entities.list_by_name_fold(query).unwrap();
            assert_eq!(
                found.iter().map(|e| e.id).collect::<Vec<_>>(),
                vec![id_hr, id_policy],
                "query {query:?}: both domains, id order"
            );
        }
        assert!(
            entities.list_by_name_fold("Nobody").unwrap().is_empty(),
            "unknown name → empty"
        );
    });
}

// (a2) update: type/description/metadata; missing id → false.
#[test]
fn update() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        assert!(
            entities
                .update(id, "EMPLOYEE", Some("updated"), Some(r#"{"k":1}"#))
                .unwrap()
        );
        let ent = entities.get_by_id(id).unwrap().unwrap();
        assert_eq!(ent.entity_type, "EMPLOYEE");
        assert_eq!(ent.description.as_deref(), Some("updated"));
        assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"k":1}"#));
        // Name and domain are NOT changeable via update (oracle contract).
        assert_eq!(ent.name, "Alice");
        assert_eq!(ent.domain, "hr");
        assert!(
            !entities.update(999_999, "X", None, None).unwrap(),
            "missing id must report false"
        );
    });
}

// (a3) update_name: rename; missing id → false.
#[test]
fn update_name() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create("ORGANIZATION", "Apple", "", None, None, None)
            .unwrap();
        assert!(entities.update_name(id, "Apple Inc.").unwrap());
        assert_eq!(entities.get_by_id(id).unwrap().unwrap().name, "Apple Inc.");
        assert!(
            !entities.update_name(999_999, "Ghost").unwrap(),
            "missing id must report false"
        );
    });
}

// (a4) delete; repeat → false; get → None.
#[test]
fn delete() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        assert!(entities.delete(id).unwrap());
        assert_eq!(entities.get_by_id(id).unwrap(), None);
        assert!(
            !entities.delete(id).unwrap(),
            "second delete must report false"
        );
    });
}

// (g1) count: total and with filters.
#[test]
fn count_respects_filters() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Bob", "hr", None, None, None)
            .unwrap();
        entities
            .create("ORGANIZATION", "Acme", "it", None, None, None)
            .unwrap();
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 3);
        assert_eq!(
            entities
                .count(&EntityFilter {
                    entity_type: Some("PERSON".into()),
                    ..Default::default()
                })
                .unwrap(),
            2
        );
        assert_eq!(
            entities
                .count(&EntityFilter {
                    domain: Some("it".into()),
                    ..Default::default()
                })
                .unwrap(),
            1
        );
        assert_eq!(
            entities
                .count(&EntityFilter {
                    name: Some("acme".into()),
                    ..Default::default()
                })
                .unwrap(),
            1
        );
    });
}

// (g2) list orders by name.
#[test]
fn list_orders_by_name() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Zed", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Bob", "hr", None, None, None)
            .unwrap();
        let names: Vec<String> = entities
            .list()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["Alice", "Bob", "Zed"]);
    });
}

// (g3) list_by_type with and without the domain filter.
#[test]
fn list_by_type() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Bob", "it", None, None, None)
            .unwrap();
        entities
            .create("ORGANIZATION", "Acme", "hr", None, None, None)
            .unwrap();

        let all_persons = entities.list_by_type("PERSON", None).unwrap();
        assert_eq!(all_persons.len(), 2);

        let hr_persons = entities.list_by_type("PERSON", Some("hr")).unwrap();
        assert_eq!(hr_persons.len(), 1);
        assert_eq!(hr_persons[0].name, "Alice");

        assert!(
            entities.list_by_type("POLICY", None).unwrap().is_empty(),
            "unknown type → empty"
        );
    });
}

// (d1) list_paginated with all three filters.
#[test]
fn list_paginated_filters() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Bob", "hr", None, None, None)
            .unwrap();
        entities
            .create("ORGANIZATION", "Acme Corp", "it", None, None, None)
            .unwrap();

        let (page, total) = entities
            .list_paginated(
                0,
                10,
                &EntityFilter {
                    entity_type: Some("PERSON".into()),
                    domain: Some("hr".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.len(), 2);

        let (page, total) = entities
            .list_paginated(
                0,
                10,
                &EntityFilter {
                    name: Some("acme".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(page[0].name, "Acme Corp");

        let (page, total) = entities
            .list_paginated(0, 10, &EntityFilter::default())
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(page.len(), 3);
    });
}

// (d2) list_paginated: LIKE wildcards in the name filter match literally.
#[test]
fn list_paginated_name_filter_escapes_like_wildcards() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Report_2024", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Report 2024", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "100%_off", "hr", None, None, None)
            .unwrap();

        let (page, total) = entities
            .list_paginated(
                0,
                10,
                &EntityFilter {
                    name: Some("Report_2024".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1, "escaped underscore must be literal");
        assert_eq!(page[0].name, "Report_2024");

        let (page, total) = entities
            .list_paginated(
                0,
                10,
                &EntityFilter {
                    name: Some("100%_off".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(page[0].name, "100%_off");
    });
}

// (d3) list_paginated: page windows over id order.
#[test]
fn list_paginated_pages_by_id() {
    let db = in_memory_db();
    let mut ids = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..5 {
            ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
        }
        Ok(())
    })
    .expect("seed transaction commits");

    with_entities(&db, |entities| {
        let (page, total) = entities
            .list_paginated(1, 2, &EntityFilter::default())
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, ids[1]);
        assert_eq!(page[1].id, ids[2]);

        let (page, total) = entities
            .list_paginated(10, 2, &EntityFilter::default())
            .unwrap();
        assert!(page.is_empty());
        assert_eq!(total, 5);
    });
}

// (c1) get_or_create: repeated calls with the same key → one row, one id.
#[test]
fn get_or_create_repeated_returns_same_id() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let id = entities
            .get_or_create(
                "PERSON",
                "Test Entity",
                "",
                Some("desc"),
                Some(0.88),
                Some(r#"{"key":"value"}"#),
            )
            .unwrap();
        let id2 = entities
            .get_or_create("PERSON", "Test Entity", "", None, Some(0.50), None)
            .unwrap();
        assert_eq!(id, id2, "second call must return the existing id");
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 1);

        // The first call's fields are kept; the second call's are ignored.
        let ent = entities.get_by_id(id).unwrap().unwrap();
        assert_eq!(ent.confidence, Some(0.88));
        assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"key":"value"}"#));
        assert_eq!(ent.description.as_deref(), Some("desc"));
    });
}

// (c2) get_or_create: same name+domain, DIFFERENT type → second row
// (the full unique key is the contract; the oracle would have returned
// the other type's row — documented deviation).
#[test]
fn get_or_create_different_type_creates_second_row() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let person = entities
            .get_or_create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        let org = entities
            .get_or_create("ORGANIZATION", "Alice", "hr", None, None, None)
            .unwrap();
        assert_ne!(person, org, "different types are different entities");
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 2);
    });
}

// (c3) get_or_create: concurrent calls with the same key → one row, all
// callers get the same id (D5 atomicity; SQLite serializes the writes
// across pool connections, the ON CONFLICT clause is the guarantee).
//
// File-backed WAL database (task 1.16): on the shared-cache `:memory:`
// database the concurrent writers flake with SQLITE_LOCKED_SHAREDCACHE
// (extended code 262) — shared-cache table locks bypass the busy
// handler, a known SQLite limitation. The file-backed configuration is
// the production one, where busy_timeout=5000 (D8) serializes writers.
#[test]
fn get_or_create_is_atomic_under_concurrency() {
    let db = temp_file_db();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            db.with_conn(|conn| {
                let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
                entities
                    .get_or_create("PERSON", "Raced Entity", "", None, None, None)
                    .unwrap()
            })
            .unwrap()
        }));
    }
    let ids: Vec<i64> = handles
        .into_iter()
        .map(|h| h.join().expect("thread must not panic"))
        .collect();
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "all callers must get the same id, got {ids:?}"
    );
    with_entities(&db, |entities| {
        assert_eq!(
            entities.count(&EntityFilter::default()).unwrap(),
            1,
            "exactly one row must exist"
        );
    });
}

// (f1) get_by_ids: several, missing absent, empty input.
#[test]
fn get_by_ids() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        let e1 = entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        let e2 = entities
            .create("ORGANIZATION", "Acme", "hr", None, None, None)
            .unwrap();
        let e3 = entities
            .create("LOCATION", "New York", "geo", None, None, None)
            .unwrap();

        let found = entities.get_by_ids(&[e2, e1]).unwrap();
        let got: HashSet<i64> = found.iter().map(|e| e.id).collect();
        assert_eq!(got, [e1, e2].into_iter().collect());
        let alice = found.iter().find(|e| e.id == e1).unwrap();
        assert_eq!(alice.name, "Alice");
        assert_eq!(alice.entity_type, "PERSON");

        let mixed = entities.get_by_ids(&[e1, 999_999, e3]).unwrap();
        assert_eq!(mixed.len(), 2, "missing ids are absent");

        assert!(entities.get_by_ids(&[]).unwrap().is_empty());
    });
}

// (f2) get_by_ids across the D9 batch boundary (3 × 500).
#[test]
fn get_by_ids_batches_over_500() {
    let db = in_memory_db();
    let mut ids = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..1200 {
            ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
        }
        Ok(())
    })
    .expect("seed transaction commits");

    with_entities(&db, |entities| {
        let requested: Vec<i64> = ids.iter().rev().copied().collect();
        let found = entities.get_by_ids(&requested).unwrap();
        assert_eq!(found.len(), 1200);
        let got: HashSet<i64> = found.iter().map(|e| e.id).collect();
        assert_eq!(got, ids.into_iter().collect::<HashSet<i64>>());
    });
}

// (e1) delete_orphaned_entity_ids: orphans deleted; source-linked,
// fact-referenced and EntityType entities survive.
#[test]
fn delete_orphaned_entity_ids() {
    let db = in_memory_db();
    let doc = insert_document(&db, "/docs/hr.md");

    let (orphan, linked, fact_subj, fact_obj, type_id) = with_entities(&db, |entities| {
        (
            entities
                .create("PERSON", "Charlie", "hr", None, None, None)
                .unwrap(),
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap(),
            entities
                .create("PERSON", "Bob", "hr", None, None, None)
                .unwrap(),
            entities
                .create("ORGANIZATION", "Acme", "it", None, None, None)
                .unwrap(),
            entities
                .create("EntityType", "PERSON", "", None, None, None)
                .unwrap(),
        )
    });

    insert_entity_source(&db, linked, doc);
    insert_fact(&db, fact_subj, "works_at", fact_obj);

    with_entities(&db, |entities| {
        let deleted = entities.delete_orphaned_entity_ids().unwrap();
        assert_eq!(deleted, 1, "only the orphan must be deleted");

        assert_eq!(entities.get_by_id(orphan).unwrap(), None);
        for survivor in [linked, fact_subj, fact_obj, type_id] {
            assert!(
                entities.get_by_id(survivor).unwrap().is_some(),
                "entity {survivor} must survive"
            );
        }
    });
}

// (e1b) Go-bug regression: a fact with a NULL subject must not disable
// the whole cleanup (the oracle's `NOT IN` would match nothing).
#[test]
fn delete_orphaned_entity_ids_with_null_fact_endpoint() {
    let db = in_memory_db();
    let (orphan, referenced) = with_entities(&db, |entities| {
        (
            entities
                .create("PERSON", "Charlie", "hr", None, None, None)
                .unwrap(),
            entities
                .create("ORGANIZATION", "Acme", "it", None, None, None)
                .unwrap(),
        )
    });
    // A fact with a NULL subject still references its object.
    insert_null_subject_fact(&db, "located_in", referenced);

    with_entities(&db, |entities| {
        let deleted = entities.delete_orphaned_entity_ids().unwrap();
        assert_eq!(deleted, 1, "the orphan must still be deleted");
        assert_eq!(entities.get_by_id(orphan).unwrap(), None);
        assert!(
            entities.get_by_id(referenced).unwrap().is_some(),
            "the fact-referenced entity must survive"
        );
    });
}

// (e2) delete_orphaned_by_ids: only the orphan candidates are deleted.
#[test]
fn delete_orphaned_by_ids() {
    let db = in_memory_db();
    let doc = insert_document(&db, "/docs/hr.md");

    let (orphan, linked, type_id) = with_entities(&db, |entities| {
        (
            entities
                .create("PERSON", "Charlie", "hr", None, None, None)
                .unwrap(),
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap(),
            entities
                .create("EntityType", "PERSON", "", None, None, None)
                .unwrap(),
        )
    });
    insert_entity_source(&db, linked, doc);

    with_entities(&db, |entities| {
        // Empty candidate list → 0, nothing touched.
        assert_eq!(entities.delete_orphaned_by_ids(&[]).unwrap(), 0);

        let deleted = entities
            .delete_orphaned_by_ids(&[orphan, linked, type_id])
            .unwrap();
        assert_eq!(deleted, 1, "only the orphan candidate must be deleted");
        assert_eq!(entities.get_by_id(orphan).unwrap(), None);
        assert!(entities.get_by_id(linked).unwrap().is_some());
        assert!(entities.get_by_id(type_id).unwrap().is_some());
    });
}

// (e3) delete_orphaned_by_ids across the D9 batch boundary.
#[test]
fn delete_orphaned_by_ids_batches_over_500() {
    let db = in_memory_db();
    let mut ids = Vec::new();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        for i in 0..1200 {
            ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
        }
        Ok(())
    })
    .expect("seed transaction commits");

    with_entities(&db, |entities| {
        let candidates: Vec<i64> = ids.iter().rev().copied().collect();
        let deleted = entities.delete_orphaned_by_ids(&candidates).unwrap();
        assert_eq!(deleted, 1200, "all orphan candidates must be deleted");
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 0);
    });
}

// (g4) types_by_count / domains_by_count / unique_types.
#[test]
fn counts_and_unique_types() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        entities
            .create("PERSON", "Alice", "hr", None, None, None)
            .unwrap();
        entities
            .create("PERSON", "Bob", "hr", None, None, None)
            .unwrap();
        entities
            .create("ORGANIZATION", "Acme", "it", None, None, None)
            .unwrap();

        let by_type = entities.types_by_count().unwrap();
        assert_eq!(by_type.get("PERSON"), Some(&2));
        assert_eq!(by_type.get("ORGANIZATION"), Some(&1));
        assert_eq!(by_type.len(), 2);

        let by_domain = entities.domains_by_count().unwrap();
        assert_eq!(by_domain.get("hr"), Some(&2));
        assert_eq!(by_domain.get("it"), Some(&1));
        assert_eq!(by_domain.len(), 2);

        assert_eq!(
            entities.unique_types().unwrap(),
            vec!["ORGANIZATION", "PERSON"],
            "distinct types, sorted"
        );
    });
}

// (g4b) empty table: all aggregations report empty, not an error.
#[test]
fn aggregations_on_empty_table() {
    let db = in_memory_db();
    with_entities(&db, |entities| {
        assert!(entities.types_by_count().unwrap().is_empty());
        assert!(entities.domains_by_count().unwrap().is_empty());
        assert!(entities.unique_types().unwrap().is_empty());
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 0);
    });
}

// (h) list_created_since: strictly after, ordered by created_at.
#[test]
fn list_created_since_strictly_after() {
    let db = in_memory_db();
    let alice = db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO entities (type, name, domain, created_at) \
             VALUES ('PERSON', 'Alice', 'hr', '2024-01-01 10:00:00')",
                [],
            )
            .unwrap();
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap();
    let bob = db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO entities (type, name, domain, created_at) \
             VALUES ('PERSON', 'Bob', 'hr', '2024-01-15 12:00:00')",
                [],
            )
            .unwrap();
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap();
    let acme = db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO entities (type, name, domain, created_at) \
             VALUES ('ORGANIZATION', 'Acme', 'it', '2024-02-01 08:00:00')",
                [],
            )
            .unwrap();
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap();

    with_entities(&db, |entities| {
        for (since, want_ids) in [
            ("2023-12-31 23:59:59", &[alice, bob, acme][..]),
            ("2024-01-10 00:00:00", &[bob, acme][..]),
            ("2024-03-01 00:00:00", &[][..]),
        ] {
            let found: Vec<i64> = entities
                .list_created_since(since)
                .unwrap()
                .into_iter()
                .map(|e| e.id)
                .collect();
            assert_eq!(found, want_ids, "since {since}");
        }
        // Strictly greater: a timestamp equal to `since` is excluded.
        let found: Vec<i64> = entities
            .list_created_since("2024-01-15 12:00:00")
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(found, vec![acme], "equal timestamp must be excluded");
    });
}

// The DAO works over a transaction: commit and rollback paths.
#[test]
fn create_inside_transaction() {
    let db = in_memory_db();
    db.exec_tx(|tx| -> Result<(), DbError> {
        let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
        entities.create("PERSON", "/tx.md", "hr", None, None, None)?;
        Ok(())
    })
    .expect("commit");

    let err = db
        .exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            entities.create("PERSON", "tx-rollback", "hr", None, None, None)?;
            // A genuine failure: UNIQUE(type, name, domain).
            entities.create("PERSON", "tx-rollback", "hr", None, None, None)?;
            Ok(())
        })
        .expect_err("closure error must surface");
    assert!(matches!(err, DbError::Sqlite { .. }));

    with_entities(&db, |entities| {
        assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 1);
        assert_eq!(
            entities.get_by_name("PERSON", "tx-rollback", "hr").unwrap(),
            None
        );
    });
}
