//! Document garbage collection: cascading per-document cleanup and orphan
//! document removal.
//!
//! `GcDao` composes the existing DAOs (entity sources, fact sources,
//! chunks, facts, entities) instead of re-stating their SQL (DRY); the
//! oracle's `DocumentGC` wrapper did the same.
//!
//! **Conscious deviations / Go bug fixes:**
//! - Vector deletion is NOT part of this module (oracle step 5,
//!   `DeleteVectorsByChunkIDs`): vectors live in the `vectors` engine, not in
//!   SQLite (ingestion-pipeline design D5). The caller reconciles vector
//!   orphans through the engine after the chunk rows are gone — eventual
//!   consistency instead of the oracle's in-transaction atomicity.
//! - `delete_orphaned_documents` fixes an oracle SQL bug: the oracle's
//!   `entity_sources` subquery selected `entity_id` instead of
//!   `document_id`, so a document referenced through `entity_sources`
//!   survived only when its id coincidentally equaled one of the linked
//!   entity ids. The `CAST(...) AS INTEGER` / `IS NOT NULL` guards on the
//!   `fact_sources` subquery are dropped: `fact_sources.document_id` is
//!   `INTEGER NOT NULL` in the v5 schema (human decision 2026-08-20).
//! - `NOT EXISTS` instead of the oracle's `NOT IN` subqueries (house style,
//!   NULL-safe, as in `entity.rs`).
//!
//! The deletion order of [`GcDao::full_clear_doc_by_id`] mirrors the oracle;
//! it is the FK-safe order for the v5 schema with `foreign_keys=ON` (D8):
//! 1. `entity_sources` rows of the document (collect the affected entity ids);
//! 2. `fact_sources` rows of the document (collect the affected fact ids);
//! 3. scoped orphan-fact cleanup — facts whose sources all came from this
//!    document (deleted regardless of status: they are ingestion facts);
//! 4. weight recompute for the surviving affected facts (their weight drops
//!    by the number of removed sources);
//! 5. the document's chunks (`chunk_entities` cascade via the schema FK);
//! 6. scoped entity orphan cleanup — only this document's candidates that
//!    lost every `entity_sources` row AND are not referenced by any fact
//!    (the facts→entities FK has no CASCADE); `'EntityType'` entities are
//!    never deleted.
//!
//! The document row itself is NOT deleted: the method serves the update path
//! (re-ingest). The row is removed by the caller (`DocumentDao::delete`) or
//! by [`GcDao::delete_orphaned_documents`].

use crate::chunk::ChunkDao;
use crate::entity::EntityDao;
use crate::entity_source::EntitySourceDao;
use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::fact::FactDao;
use crate::fact_source::FactSourceDao;

/// Garbage collection over document data: cascading per-document cleanup and
/// orphan removal.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewDocumentGC(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DbError, GcDao};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let gc = GcDao::new(ConnectionOrTx::Connection(conn));
///     gc.full_clear_doc_by_id(1)?;
///     let removed = gc.delete_orphaned_documents()?;
///     assert!(removed >= 0);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct GcDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> GcDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Remove all per-document data of `doc_id` (see the module docs for the
    /// order and the FK rationale). The document row itself is kept (the
    /// update path re-uses it). Unknown or empty documents are a no-op.
    pub fn full_clear_doc_by_id(&self, doc_id: i64) -> Result<(), DbError> {
        // 1 + 2. Provenance rows first: they are leaf tables (nothing
        // references them), and the affected ids feed the scoped cleanups
        // below.
        let entity_ids = EntitySourceDao::new(self.exec).delete_by_document_id(doc_id)?;
        let fact_ids = FactSourceDao::new(self.exec).delete_by_document_id(doc_id)?;

        // 3 + 4. Facts whose sources all came from this document are orphans
        // now; the survivors lose this document's sources (weight recompute
        // — an UPDATE over already-deleted ids is a harmless no-op).
        if !fact_ids.is_empty() {
            self.delete_orphaned_facts_scoped(&fact_ids)?;
            FactDao::new(self.exec).recompute_weights(&fact_ids)?;
        }

        // 5. Chunks: `chunk_entities` cascade via the schema FK
        // (foreign_keys=ON, D8). Vectors of these chunks are reconciled by
        // the vectors engine's caller (design D5), not here.
        let chunk_ids: Vec<i64> = ChunkDao::new(self.exec)
            .list_by_doc_id(doc_id)?
            .into_iter()
            .map(|chunk| chunk.id)
            .collect();
        if !chunk_ids.is_empty() {
            ChunkDao::new(self.exec).delete_by_ids(&chunk_ids)?;
        }

        // 6. Scoped entity orphan cleanup: only this document's candidates,
        // and only those that lost every `entity_sources` row and are not
        // referenced by any fact.
        if !entity_ids.is_empty() {
            EntityDao::new(self.exec).delete_orphaned_by_ids(&entity_ids)?;
        }
        Ok(())
    }

    /// Remove documents with no chunks, no `entity_sources` rows and no
    /// `fact_sources` rows. Returns the number of documents deleted.
    ///
    /// FK-safe with `foreign_keys=ON`: the three cascading tables hold no
    /// rows for the deleted documents by construction.
    pub fn delete_orphaned_documents(&self) -> Result<i64, DbError> {
        let changed = self.exec.execute(
            "DELETE FROM documents \
             WHERE NOT EXISTS (SELECT 1 FROM chunks c WHERE c.doc_id = documents.id) \
               AND NOT EXISTS (SELECT 1 FROM entity_sources es WHERE es.document_id = documents.id) \
               AND NOT EXISTS (SELECT 1 FROM fact_sources fs WHERE fs.document_id = documents.id)",
            [],
        )?;
        Ok(changed as i64)
    }

    /// Batch-delete entities with no `entity_sources` links, excluding
    /// `'EntityType'` entities and entities referenced by any fact. Thin
    /// delegation to [`EntityDao::delete_orphaned_entity_ids`] (the
    /// oracle's `DocumentGC.DeleteOrphanedEntityIDs`).
    pub fn delete_orphaned_entity_ids(&self) -> Result<i64, DbError> {
        EntityDao::new(self.exec).delete_orphaned_entity_ids()
    }

    /// Batch-delete facts with no `fact_sources` rows, excluding approved
    /// facts. Thin delegation to [`FactDao::find_orphaned_fact_ids`] +
    /// [`FactDao::delete_orphaned_facts`] (the oracle's
    /// `DocumentGC.DeleteOrphanedFacts`).
    pub fn delete_orphaned_facts(&self) -> Result<i64, DbError> {
        let facts = FactDao::new(self.exec);
        let orphaned = facts.find_orphaned_fact_ids(true, &[])?;
        if orphaned.is_empty() {
            return Ok(0);
        }
        facts.delete_orphaned_facts(&orphaned)
    }

    /// Scoped orphan-fact cleanup: among `candidate_fact_ids`, delete the
    /// facts with no remaining `fact_sources` rows — regardless of status
    /// (oracle `FindAndDeleteOrphanedFacts`: the document-refresh path must
    /// not preserve facts whose only sources were just removed). Returns the
    /// number of facts deleted.
    fn delete_orphaned_facts_scoped(&self, candidate_fact_ids: &[i64]) -> Result<i64, DbError> {
        let facts = FactDao::new(self.exec);
        let orphaned = facts.find_orphaned_fact_ids(false, candidate_fact_ids)?;
        if orphaned.is_empty() {
            return Ok(0);
        }
        facts.delete_orphaned_facts(&orphaned)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::document::DocumentDao;
    use crate::test_util::in_memory_db;

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_gc<T>(db: &Db, f: impl FnOnce(&GcDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&GcDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Row count of one table filtered by one column (test assertions).
    fn count(db: &Db, table: &str, column: &str, value: i64) -> i64 {
        db.with_conn(|conn| {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?"),
                [value],
                |r| r.get(0),
            )
        })
        .unwrap()
        .unwrap()
    }

    /// Create a document and return its id.
    fn seed_doc(db: &Db, path: &str) -> i64 {
        db.with_conn(|conn| {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            docs.create("markdown", path, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Create an entity and return its id.
    fn seed_entity(db: &Db, entity_type: &str, name: &str) -> i64 {
        db.with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            entities.create(entity_type, name, "hr", None, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Create a fact and return its id.
    fn seed_fact(db: &Db, subject: i64, predicate: &str, object: i64) -> i64 {
        db.with_conn(|conn| {
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(subject),
                predicate,
                Some(object),
                "hr",
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap()
    }

    /// Create a fact source.
    fn seed_fact_source(db: &Db, fact_id: i64, doc_id: i64) {
        db.with_conn(|conn| {
            let sources = FactSourceDao::new(ConnectionOrTx::Connection(conn));
            sources
                .create(fact_id, doc_id, Some("quote"), None)
                .map(|_| ())
        })
        .unwrap()
        .unwrap();
    }

    /// Link an entity to a document via `entity_sources`.
    fn seed_entity_source(db: &Db, entity_id: i64, doc_id: i64) {
        db.with_conn(|conn| {
            let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
            sources.create(entity_id, doc_id).map(|_| ())
        })
        .unwrap()
        .unwrap();
    }

    /// Create a chunk and return its id.
    fn seed_chunk(db: &Db, doc_id: i64, sequence: i64) -> i64 {
        db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            chunks.create(doc_id, "chunk text", sequence, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Set a fact's status directly (the DAO has no status update; the
    /// sibling `fact.rs` tests use the same raw-SQL helper).
    fn set_fact_status(db: &Db, fact_id: i64, status: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE facts SET status = ? WHERE id = ?",
                rusqlite::params![status, fact_id],
            )
        })
        .unwrap()
        .unwrap();
    }

    // (a) full_clear removes exactly the document's data and nothing else
    // (oracle TestFullClearDocByID_RemovesAllData).
    #[test]
    fn full_clear_removes_only_the_document_data() {
        let db = in_memory_db();
        let doc1 = seed_doc(&db, "/test/doc1.md");
        let doc2 = seed_doc(&db, "/test/doc2.md");
        let entity1 = seed_entity(&db, "PERSON", "Alice");
        let entity2 = seed_entity(&db, "ORGANIZATION", "Acme Corp");
        seed_entity_source(&db, entity1, doc1);
        seed_entity_source(&db, entity2, doc2);
        let fact = seed_fact(&db, entity1, "works_at", entity2);
        seed_fact_source(&db, fact, doc1);
        seed_fact_source(&db, fact, doc2);
        seed_chunk(&db, doc1, 0);
        seed_chunk(&db, doc2, 0);

        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(doc1).unwrap();
        });

        // doc1's provenance is gone, doc2's survives.
        assert_eq!(count(&db, "entity_sources", "document_id", doc1), 0);
        assert_eq!(count(&db, "entity_sources", "document_id", doc2), 1);
        assert_eq!(count(&db, "fact_sources", "document_id", doc1), 0);
        assert_eq!(count(&db, "fact_sources", "document_id", doc2), 1);

        // The fact survives (it has a source in doc2).
        assert_eq!(count(&db, "facts", "id", fact), 1);

        // Chunks: doc1's gone, doc2's untouched.
        assert_eq!(count(&db, "chunks", "doc_id", doc1), 0);
        assert_eq!(count(&db, "chunks", "doc_id", doc2), 1);

        // entity1 survives: referenced by the surviving fact (the
        // facts→entities FK has no CASCADE). entity2 survives: linked to
        // doc2. The document rows themselves survive (the update path).
        db.with_conn(|conn| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            assert!(
                entities.get_by_id(entity1).unwrap().is_some(),
                "entity1 must survive"
            );
            assert!(
                entities.get_by_id(entity2).unwrap().is_some(),
                "entity2 must survive"
            );
            assert!(
                docs.get_by_id(doc1).unwrap().is_some(),
                "the doc1 row must survive"
            );
            assert!(
                docs.get_by_id(doc2).unwrap().is_some(),
                "the doc2 row must survive"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (b) weight decrease: a surviving fact's weight drops as its sources
    // are cleared document by document (oracle
    // TestFullClearDocByID_WeightDecrease).
    #[test]
    fn full_clear_decreases_weights() {
        let db = in_memory_db();
        let doc1 = seed_doc(&db, "/test/doc1.md");
        let doc2 = seed_doc(&db, "/test/doc2.md");
        let doc3 = seed_doc(&db, "/test/doc3.md");
        let subject = seed_entity(&db, "PERSON", "Alice");
        let object = seed_entity(&db, "ORGANIZATION", "Acme Corp");
        let fact = seed_fact(&db, subject, "works_at", object);
        for doc in [doc1, doc2, doc3] {
            seed_fact_source(&db, fact, doc);
        }

        db.with_conn(|conn| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.recompute_weights(&[fact])?;
            assert_eq!(facts.get_by_id(fact).unwrap().unwrap().weight, 3);
            Ok(())
        })
        .unwrap()
        .unwrap();

        with_gc(&db, |gc| gc.full_clear_doc_by_id(doc1).unwrap());
        let weight = db
            .with_conn(|conn| {
                let facts = FactDao::new(ConnectionOrTx::Connection(conn));
                facts.get_by_id(fact).unwrap().unwrap().weight
            })
            .unwrap();
        assert_eq!(weight, 2, "clearing doc1 must drop the weight to 2");

        with_gc(&db, |gc| gc.full_clear_doc_by_id(doc2).unwrap());
        let weight = db
            .with_conn(|conn| {
                let facts = FactDao::new(ConnectionOrTx::Connection(conn));
                facts.get_by_id(fact).unwrap().unwrap().weight
            })
            .unwrap();
        assert_eq!(weight, 1, "clearing doc2 must drop the weight to 1");

        // The last source cleared: the fact is an orphan now and is deleted
        // (regardless of status — it is an ingestion fact).
        with_gc(&db, |gc| gc.full_clear_doc_by_id(doc3).unwrap());
        assert_eq!(
            count(&db, "facts", "id", fact),
            0,
            "orphaned fact must be deleted"
        );
    }

    // (c) both endpoint entities are linked ONLY to doc1 and have no
    // remaining entity_sources after the clear, but they survive because a
    // surviving fact references them (oracle
    // TestFullClearDocByID_EntityReferencedByFactSurvives).
    #[test]
    fn full_clear_keeps_fact_referenced_entities() {
        let db = in_memory_db();
        let doc1 = seed_doc(&db, "/test/doc1.md");
        let doc2 = seed_doc(&db, "/test/doc2.md");
        let subject = seed_entity(&db, "PERSON", "Alice");
        let object = seed_entity(&db, "ORGANIZATION", "Acme Corp");
        seed_entity_source(&db, subject, doc1);
        seed_entity_source(&db, object, doc1);
        let fact = seed_fact(&db, subject, "works_at", object);
        seed_fact_source(&db, fact, doc2);

        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(doc1).unwrap();
        });

        assert_eq!(count(&db, "entity_sources", "document_id", doc1), 0);
        db.with_conn(|conn| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(
                entities.get_by_id(subject).unwrap().is_some(),
                "the subject must survive (referenced by a surviving fact)"
            );
            assert!(
                entities.get_by_id(object).unwrap().is_some(),
                "the object must survive (referenced by a surviving fact)"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (d) scoped entity cleanup: an entity linked only to doc1 and not
    // referenced by any fact IS deleted; an 'EntityType' entity is never
    // deleted; an entity linked to another document survives.
    #[test]
    fn full_clear_deletes_only_unreferenced_entities() {
        let db = in_memory_db();
        let doc1 = seed_doc(&db, "/test/doc1.md");
        let doc2 = seed_doc(&db, "/test/doc2.md");
        let orphan = seed_entity(&db, "PERSON", "Charlie");
        let type_id = seed_entity(&db, "EntityType", "PERSON");
        let shared = seed_entity(&db, "PERSON", "Alice");
        seed_entity_source(&db, orphan, doc1);
        seed_entity_source(&db, type_id, doc1);
        seed_entity_source(&db, shared, doc1);
        seed_entity_source(&db, shared, doc2);

        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(doc1).unwrap();
        });

        db.with_conn(|conn| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(
                entities.get_by_id(orphan).unwrap().is_none(),
                "the unreferenced orphan must be deleted"
            );
            assert!(
                entities.get_by_id(type_id).unwrap().is_some(),
                "'EntityType' entities must never be deleted"
            );
            assert!(
                entities.get_by_id(shared).unwrap().is_some(),
                "an entity linked to doc2 must survive"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (e) empty-document and unknown-document edge cases: both are no-ops,
    // and a second clear of the same document is a no-op (idempotence).
    #[test]
    fn full_clear_empty_and_unknown_documents_are_no_ops() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/test/doc.md");
        let entity = seed_entity(&db, "PERSON", "Alice");
        seed_entity_source(&db, entity, doc);
        let chunk = seed_chunk(&db, doc, 0);

        // Unknown document: no error, nothing touched.
        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(999_999).unwrap();
        });
        assert_eq!(count(&db, "entity_sources", "document_id", doc), 1);
        assert_eq!(count(&db, "chunks", "id", chunk), 1);

        // A document with no data: no error.
        let empty = seed_doc(&db, "/test/empty.md");
        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(empty).unwrap();
        });

        // First clear removes the data; the second is a no-op.
        with_gc(&db, |gc| {
            gc.full_clear_doc_by_id(doc).unwrap();
            gc.full_clear_doc_by_id(doc).unwrap();
        });
        assert_eq!(count(&db, "entity_sources", "document_id", doc), 0);
        assert_eq!(count(&db, "chunks", "doc_id", doc), 0);
        assert_eq!(
            count(&db, "documents", "id", doc),
            1,
            "the row must survive"
        );
    }

    // (f) delete_orphaned_documents: documents with no chunks, no
    // entity_sources and no fact_sources are deleted; a document referenced
    // through ANY of the three tables survives (oracle
    // TestDeleteOrphanedDocuments, extended with the entity_sources and
    // fact_sources paths — the regression surface of the oracle's
    // `entity_id`-instead-of-`document_id` subquery bug).
    #[test]
    fn delete_orphaned_documents() {
        let db = in_memory_db();
        let orphan = seed_doc(&db, "/test/orphan.md");
        let by_chunk = seed_doc(&db, "/test/by_chunk.md");
        let by_entity = seed_doc(&db, "/test/by_entity.md");
        let by_fact = seed_doc(&db, "/test/by_fact.md");

        seed_chunk(&db, by_chunk, 0);
        let entity = seed_entity(&db, "PERSON", "Alice");
        seed_entity_source(&db, entity, by_entity);
        let object = seed_entity(&db, "ORGANIZATION", "Acme Corp");
        let fact = seed_fact(&db, entity, "works_at", object);
        seed_fact_source(&db, fact, by_fact);

        with_gc(&db, |gc| {
            let deleted = gc.delete_orphaned_documents().unwrap();
            assert_eq!(
                deleted, 1,
                "only the fully orphaned document must be deleted"
            );
        });

        db.with_conn(|conn| -> Result<(), DbError> {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            assert!(
                docs.get_by_id(orphan).unwrap().is_none(),
                "the orphan must be deleted"
            );
            for survivor in [by_chunk, by_entity, by_fact] {
                assert!(
                    docs.get_by_id(survivor).unwrap().is_some(),
                    "document {survivor} must survive (it is referenced)"
                );
            }
            Ok(())
        })
        .unwrap()
        .unwrap();

        // Idempotence: nothing left to delete on a second run.
        with_gc(&db, |gc| {
            assert_eq!(gc.delete_orphaned_documents().unwrap(), 0);
        });
    }

    // (g) the DAO works over a transaction: a failure after the clear rolls
    // back the whole unit of work (house pattern, as in the sibling DAOs).
    #[test]
    fn full_clear_rolls_back_in_transaction() {
        let db = in_memory_db();
        let doc1 = seed_doc(&db, "/test/doc1.md");
        let entity = seed_entity(&db, "PERSON", "Alice");
        seed_entity_source(&db, entity, doc1);
        let chunk = seed_chunk(&db, doc1, 0);

        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let gc = GcDao::new(ConnectionOrTx::Transaction(&*tx));
                gc.full_clear_doc_by_id(doc1)?;
                // A genuine failure after the clear (CHECK violation).
                tx.execute(
                    "INSERT INTO facts (predicate, status) VALUES ('p', 'bogus')",
                    [],
                )?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        // Everything is restored by the rollback.
        assert_eq!(count(&db, "entity_sources", "document_id", doc1), 1);
        assert_eq!(count(&db, "chunks", "id", chunk), 1);
        db.with_conn(|conn| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(entities.get_by_id(entity).unwrap().is_some());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // (h) delete_orphaned_facts delegation: non-approved orphan facts are
    // deleted; approved orphans and sourced facts survive.
    #[test]
    fn delete_orphaned_facts_delegates() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/test/doc.md");
        let subject = seed_entity(&db, "PERSON", "Alice");
        let object = seed_entity(&db, "ORGANIZATION", "Acme Corp");
        let draft = seed_fact(&db, subject, "p_draft", object);
        let approved = seed_fact(&db, subject, "p_approved", object);
        let sourced = seed_fact(&db, subject, "p_sourced", object);
        set_fact_status(&db, draft, "draft");
        set_fact_status(&db, approved, "approved");
        seed_fact_source(&db, sourced, doc);

        with_gc(&db, |gc| {
            let deleted = gc.delete_orphaned_facts().unwrap();
            assert_eq!(deleted, 1, "only the draft orphan must be deleted");
        });

        assert_eq!(count(&db, "facts", "id", draft), 0);
        assert_eq!(
            count(&db, "facts", "id", approved),
            1,
            "approved orphans survive"
        );
        assert_eq!(
            count(&db, "facts", "id", sourced),
            1,
            "sourced facts survive"
        );
    }

    // (i) delete_orphaned_entity_ids delegation: orphan entities are
    // deleted; source-linked and 'EntityType' entities survive.
    #[test]
    fn delete_orphaned_entity_ids_delegates() {
        let db = in_memory_db();
        let doc = seed_doc(&db, "/test/doc.md");
        let orphan = seed_entity(&db, "PERSON", "Charlie");
        let linked = seed_entity(&db, "PERSON", "Alice");
        let type_id = seed_entity(&db, "EntityType", "PERSON");
        seed_entity_source(&db, linked, doc);

        with_gc(&db, |gc| {
            let deleted = gc.delete_orphaned_entity_ids().unwrap();
            assert_eq!(deleted, 1, "only the orphan must be deleted");
        });

        db.with_conn(|conn| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            assert!(entities.get_by_id(orphan).unwrap().is_none());
            assert!(entities.get_by_id(linked).unwrap().is_some());
            assert!(entities.get_by_id(type_id).unwrap().is_some());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
