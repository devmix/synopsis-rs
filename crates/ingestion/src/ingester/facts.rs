//! Fact persistence for the per-document pipeline (task 3.5).
//!
//! The entity half (entity creation + chunk links) is inlined in
//! [`super::Ingester::process_document`] (task 3.4); this module holds the
//! fact half.
//!
//! Flow (one call per chunk, inside the per-document transaction): unique
//! `(name, type, domain)` endpoints of all facts → synthetic [`NerEntity`]s
//! → [`Resolver::lookup_or_create_with_stats`] → chunk links → per-fact
//! `facts` row (de-duplicated by the unique `(subject, object, predicate)`
//! key) + `fact_sources` row with a quote
//! ([`super::extract_quote_from_chunk`]) → one `recompute_weights` over the
//! touched facts.
//!
//! Design decisions:
//! - The endpoint key is a `(name, type, domain)` tuple, not a delimited
//!   string join — no delimiter ambiguity by construction (task directive).
//! - Unique endpoints are collected in first-seen order, so synthetic-entity
//!   creation order is deterministic per run.
//! - The "unresolved endpoint" warn branch is defensive: the resolver
//!   contract guarantees a hit on a miss (`LookupOrCreate` creates on a
//!   miss), so the branch is unreachable today. It is kept so that if the
//!   resolver contract ever changes, a fact degrades to a skipped fact with a
//!   warning instead of an erroring document. It is unit-tested by calling
//!   [`persist_facts`] with a hand-built map.
//! - The `facts` and `fact_sources` counters are incremented together on
//!   every stored fact (always equal); one count feeds both tracker fields.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use db::{ChunkEntityDao, ConnectionOrTx, FactDao, FactSourceDao};
use serde_json::Map;

use crate::entities::{EntityChanges, Resolver};
use crate::error::IngestionError;
use crate::ner::{NerEntity, NerFact, normalize};
use crate::parsers::format_rfc3339_utc;
use crate::progress::ProgressTracker;

use super::extract_quote_from_chunk;

/// Identity of a fact endpoint: (name, entity type, normalized domain).
type EntityKey = (String, String, String);

/// Stores the fact half of entity storage for one chunk: synthetic endpoint
/// entities, `facts` rows, `fact_sources` rows with quotes, and one weight
/// recompute over the touched facts. Returns the resolver's change report
/// (created/updated entity ids) for the synthetic endpoint entities.
///
/// Must run inside the per-document transaction (the call site passes the
/// transaction's [`ConnectionOrTx`], so entity resolution never hits the
/// SQLite write lock from a second connection). A no-op when `facts` is
/// empty (returns an empty change report).
///
/// # Errors
///
/// Resolver, DAO and JSON-serialization errors.
#[allow(clippy::too_many_arguments)]
pub(super) fn store_facts(
    exec: ConnectionOrTx<'_>,
    resolver: &Resolver,
    tracker: &mut ProgressTracker,
    doc_id: i64,
    chunk_id: i64,
    chunk_text: &str,
    facts: &[NerFact],
    source_path: &str,
    sequence_num: usize,
) -> Result<EntityChanges, IngestionError> {
    if facts.is_empty() {
        return Ok(EntityChanges::default());
    }

    let endpoints = collect_endpoints(facts);
    let synthetic: Vec<NerEntity> = endpoints
        .iter()
        .map(|(name, entity_type, domain)| NerEntity {
            name: name.clone(),
            entity_type: entity_type.clone(),
            description: String::new(),
            // The synthetic endpoint entity carries the zero confidence: the
            // row is a fact-derived placeholder, not an extraction.
            confidence: 0.0,
            domain: domain.clone(),
            metadata: Map::new(),
        })
        .collect();

    let (ids, created, changes) = resolver.lookup_or_create_with_stats(exec, doc_id, &synthetic)?;
    if created > 0 {
        tracker.add_entities(created as u64);
    }

    let links = ChunkEntityDao::new(exec);
    for id in &ids {
        links.link(chunk_id, *id)?;
    }

    let entity_map: HashMap<EntityKey, i64> = endpoints
        .iter()
        .zip(ids.iter())
        .map(|(key, &id)| (key.clone(), id))
        .collect();

    persist_facts(
        exec,
        doc_id,
        chunk_text,
        facts,
        &entity_map,
        tracker,
        source_path,
        sequence_num,
    )?;
    Ok(changes)
}

/// Persists `facts` against the resolved `entity_map` (the per-fact half of
/// [`store_facts`], separated so the skip branches are unit-testable with a
/// hand-built map).
///
/// A fact is skipped (warned, not an error) when an endpoint is missing from
/// the map or when [`FactDao::validate_fact_domain`] rejects the domain
/// pairing. Every stored fact gets one `fact_sources` row with a quote and
/// an RFC 3339 `extracted_at`, and the touched facts get one
/// `recompute_weights` pass at the end.
#[allow(clippy::too_many_arguments)]
fn persist_facts(
    exec: ConnectionOrTx<'_>,
    doc_id: i64,
    chunk_text: &str,
    facts: &[NerFact],
    entity_map: &HashMap<EntityKey, i64>,
    tracker: &mut ProgressTracker,
    source_path: &str,
    sequence_num: usize,
) -> Result<(), IngestionError> {
    let facts_dao = FactDao::new(exec);
    let sources_dao = FactSourceDao::new(exec);

    let mut fact_ids = Vec::with_capacity(facts.len());
    for fact in facts {
        let domain = normalize(&fact.domain);
        let subject_key = (
            fact.subject_name.clone(),
            fact.subject_type.clone(),
            domain.clone(),
        );
        let object_key = (
            fact.object_name.clone(),
            fact.object_type.clone(),
            domain.clone(),
        );
        let (Some(&subject_id), Some(&object_id)) =
            (entity_map.get(&subject_key), entity_map.get(&object_key))
        else {
            eprintln!(
                "warning: skip fact with unresolved entity {}:{} subject={}({}) predicate={} object={}({})",
                source_path,
                sequence_num,
                fact.subject_name,
                fact.subject_type,
                fact.predicate,
                fact.object_name,
                fact.object_type
            );
            continue;
        };

        if !facts_dao.validate_fact_domain(subject_id, object_id, &domain)? {
            eprintln!(
                "warning: skip cross-domain fact {}:{} subject={} predicate={} object={} fact_domain={}",
                source_path,
                sequence_num,
                fact.subject_name,
                fact.predicate,
                fact.object_name,
                domain
            );
            continue;
        }

        let metadata_json = if fact.metadata.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&fact.metadata)
                    .map_err(|source| IngestionError::MetadataJson { source })?,
            )
        };

        let fact_id = facts_dao.create_or_ignore(
            Some(subject_id),
            &fact.predicate,
            Some(object_id),
            &domain,
            metadata_json.as_deref(),
            None,
            None,
        )?;

        // The quote is always stored (even when empty), and `extracted_at`
        // is RFC 3339 UTC (the workspace convention,
        // `crate::parsers::format_rfc3339_utc`).
        let quote = extract_quote_from_chunk(chunk_text, &fact.subject_name, &fact.object_name);
        let extracted_at = format_rfc3339_utc(SystemTime::now());
        sources_dao.create(fact_id, doc_id, Some(&quote), extracted_at.as_deref())?;
        fact_ids.push(fact_id);
    }

    if !fact_ids.is_empty() {
        facts_dao.recompute_weights(&fact_ids)?;
        tracker.add_facts(fact_ids.len() as u64);
        tracker.add_fact_sources(fact_ids.len() as u64);
    }
    Ok(())
}

/// The unique `(name, type, normalized-domain)` endpoints of all facts
/// (subject + object side), in first-seen order — deterministic.
fn collect_endpoints(facts: &[NerFact]) -> Vec<EntityKey> {
    let mut seen: HashSet<EntityKey> = HashSet::new();
    let mut endpoints = Vec::new();
    for fact in facts {
        let domain = normalize(&fact.domain);
        for (name, entity_type) in [
            (fact.subject_name.as_str(), fact.subject_type.as_str()),
            (fact.object_name.as_str(), fact.object_type.as_str()),
        ] {
            let key = (name.to_owned(), entity_type.to_owned(), domain.clone());
            if seen.insert(key.clone()) {
                endpoints.push(key);
            }
        }
    }
    endpoints
}

#[cfg(test)]
mod tests {
    //! `persist_facts` unit tests over an in-memory database: the skip
    //! branches (unresolved endpoint, cross-domain) are not reachable through
    //! the public path — the resolver always resolves — so the seam is
    //! tested directly with hand-built entity maps.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use db::test_util::in_memory_db;
    use db::{ChunkDao, ConnectionOrTx, DocumentDao, EntityDao, FactDao, FactSourceDao};

    use super::*;

    /// Seeds a document + one chunk + two endpoint entities; returns
    /// `(doc_id, chunk_id, subject_id, object_id)`.
    fn seed(db: &db::Db, subject_domain: &str, object_domain: &str) -> (i64, i64, i64, i64) {
        db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let doc_id = DocumentDao::new(exec)
                .create("test", "/docs/a.txt", None, None)
                .unwrap();
            let chunk_id = ChunkDao::new(exec)
                .create(doc_id, "Alice works at Acme Corp.", 0, Some(0), Some(26))
                .unwrap();
            let entities = EntityDao::new(exec);
            let subject = entities
                .create("person", "Alice", subject_domain, None, None, None)
                .unwrap();
            let object = entities
                .create("organization", "Acme Corp", object_domain, None, None, None)
                .unwrap();
            (doc_id, chunk_id, subject, object)
        })
        .unwrap()
    }

    fn fact(
        subject: &str,
        subject_type: &str,
        predicate: &str,
        object: &str,
        object_type: &str,
        domain: &str,
    ) -> NerFact {
        NerFact {
            subject_type: subject_type.to_owned(),
            subject_name: subject.to_owned(),
            predicate: predicate.to_owned(),
            object_type: object_type.to_owned(),
            object_name: object.to_owned(),
            domain: domain.to_owned(),
            metadata: Map::new(),
        }
    }

    /// A full endpoint map for the seeded `Alice`/`Acme Corp` pair.
    fn full_map(subject: i64, object: i64) -> HashMap<EntityKey, i64> {
        [
            (
                ("Alice".to_owned(), "person".to_owned(), String::new()),
                subject,
            ),
            (
                (
                    "Acme Corp".to_owned(),
                    "organization".to_owned(),
                    String::new(),
                ),
                object,
            ),
        ]
        .into_iter()
        .collect()
    }

    /// Happy path: fact + source + quote + metadata, weights recomputed.
    #[test]
    fn persist_stores_fact_source_and_quote() {
        let db = in_memory_db();
        let (doc_id, _chunk_id, subject, object) = seed(&db, "", "");
        let mut fact = fact(
            "Alice",
            "person",
            "works_at",
            "Acme Corp",
            "organization",
            "",
        );
        fact.metadata.insert(
            "rule".to_owned(),
            serde_json::Value::String("r1".to_owned()),
        );

        let mut tracker = ProgressTracker::new(0, "test");
        db.with_conn(|conn| {
            persist_facts(
                ConnectionOrTx::Connection(conn),
                doc_id,
                "Alice works at Acme Corp.",
                std::slice::from_ref(&fact),
                &full_map(subject, object),
                &mut tracker,
                "/docs/a.txt",
                0,
            )
        })
        .unwrap()
        .unwrap();

        assert_eq!(tracker.stats().facts_created, 1);
        assert_eq!(tracker.stats().fact_sources_created, 1);

        let (fact_row, source) = db
            .with_conn(|conn| {
                let exec = ConnectionOrTx::Connection(conn);
                let fact_row = FactDao::new(exec)
                    .list_all()
                    .unwrap()
                    .pop()
                    .expect("one fact");
                let source = FactSourceDao::new(exec)
                    .get_by_fact_id(fact_row.id)
                    .unwrap()
                    .pop()
                    .expect("one source");
                (fact_row, source)
            })
            .unwrap();
        assert_eq!(fact_row.predicate, "works_at");
        assert_eq!(fact_row.subject_entity_id, Some(subject));
        assert_eq!(fact_row.object_entity_id, Some(object));
        assert_eq!(fact_row.domain, "");
        assert_eq!(fact_row.metadata_json.as_deref(), Some(r#"{"rule":"r1"}"#),);
        assert_eq!(fact_row.status, "approved");
        assert_eq!(fact_row.weight, 1, "recomputed from the single source");

        assert_eq!(source.document_id, doc_id);
        assert_eq!(
            source.quote.as_deref(),
            Some("Alice works at Acme Corp."),
            "the whole chunk fits the quote window"
        );
        assert_eq!(
            source.extracted_at.len(),
            20,
            "RFC 3339 second precision: {:?}",
            source.extracted_at
        );
        assert!(
            source.extracted_at.ends_with('Z'),
            "{:?}",
            source.extracted_at
        );
    }

    /// An endpoint missing from the entity map (defensive branch — the
    /// resolver never produces one today) skips the fact, not the document.
    #[test]
    fn unresolved_endpoint_is_skipped() {
        let db = in_memory_db();
        let (doc_id, _chunk_id, subject, _object) = seed(&db, "", "");
        // Only the subject is resolvable; the object key is missing.
        let map: HashMap<EntityKey, i64> = [(
            ("Alice".to_owned(), "person".to_owned(), String::new()),
            subject,
        )]
        .into_iter()
        .collect();

        let mut tracker = ProgressTracker::new(0, "test");
        let facts = vec![fact(
            "Alice",
            "person",
            "works_at",
            "Acme Corp",
            "organization",
            "",
        )];
        db.with_conn(|conn| {
            persist_facts(
                ConnectionOrTx::Connection(conn),
                doc_id,
                "Alice works at Acme Corp.",
                &facts,
                &map,
                &mut tracker,
                "/docs/a.txt",
                0,
            )
        })
        .unwrap()
        .unwrap();

        assert_eq!(tracker.stats().facts_created, 0);
        assert_eq!(tracker.stats().fact_sources_created, 0);
        let count = db
            .with_conn(|conn| FactDao::new(ConnectionOrTx::Connection(conn)).count())
            .unwrap()
            .unwrap();
        assert_eq!(count, 0, "no fact row for an unresolved endpoint");
    }

    /// `validate_fact_domain` rejects a fact whose domain differs from the
    /// endpoint entities' domains: warned and skipped.
    #[test]
    fn cross_domain_fact_is_skipped() {
        let db = in_memory_db();
        let (doc_id, _chunk_id, subject, object) = seed(&db, "hr", "it");

        let mut tracker = ProgressTracker::new(0, "test");
        // Fact domain "" differs from both endpoint domains.
        let facts = vec![fact(
            "Alice",
            "person",
            "works_at",
            "Acme Corp",
            "organization",
            "",
        )];
        db.with_conn(|conn| {
            persist_facts(
                ConnectionOrTx::Connection(conn),
                doc_id,
                "Alice works at Acme Corp.",
                &facts,
                &full_map(subject, object),
                &mut tracker,
                "/docs/a.txt",
                0,
            )
        })
        .unwrap()
        .unwrap();

        assert_eq!(tracker.stats().facts_created, 0);
        let count = db
            .with_conn(|conn| FactDao::new(ConnectionOrTx::Connection(conn)).count())
            .unwrap()
            .unwrap();
        assert_eq!(count, 0, "no fact row for a cross-domain fact");
    }

    /// `store_facts` with an empty fact list is a no-op (the entities-only
    /// and empty `NerResult` paths in the pipeline).
    #[test]
    fn store_facts_empty_is_a_no_op() {
        let db = in_memory_db();
        let resolver = Resolver::new(0.8);
        let mut tracker = ProgressTracker::new(0, "test");
        db.with_conn(|conn| {
            store_facts(
                ConnectionOrTx::Connection(conn),
                &resolver,
                &mut tracker,
                1,
                1,
                "text",
                &[],
                "/docs/a.txt",
                0,
            )
        })
        .unwrap()
        .unwrap();
        assert_eq!(tracker.stats().facts_created, 0);
        assert_eq!(tracker.stats().entities_extracted, 0);
    }
}
