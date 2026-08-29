//! Post-pipeline maintenance: orphan cleanup, source pruning and cross-domain
//! entity linking (pipeline task 3.8).
//!
//! Oracle mapping: `internal/ingestion/runner/runner.go` —
//! `cleanupOrphanedDataLocked`, `PruneDeleted`, `BuildEntityLinks` (and the
//! `IngestAll` tail that calls the first two).
//!
//! **Conscious deviations from the oracle** (behavior ported, not
//! transcribed):
//!
//! - [`OrphanCleanupStats`] has no `errors` field: a failure propagates as
//!   `Err` (Rust convention); the oracle's per-step error list is subsumed
//!   by the single `Result`.
//! - The vector-orphan reconciliation runs OUTSIDE the SQLite transaction
//!   (design D5): vectors live in the vectors engine, not in SQLite, so
//!   cross-store atomicity is impossible — eventual consistency with the
//!   chunk row as the source of truth.
//! - [`Runner::prune_deleted`] removes a document only when its file is
//!   genuinely gone (`NotFound`); any other stat error (transient I/O)
//!   keeps the document. The oracle deleted on ANY `os.Stat` error.
//! - [`Runner::prune_deleted`] does not touch the pruned document's
//!   vectors (the `db::GcDao` deviation, design D5): they become orphans
//!   and the next [`Runner::cleanup_orphaned_data`] reconciles them.
//! - [`Runner::build_entity_links`] reads the `last_linking_run` app_kv
//!   value (the oracle's `relations.KVKeyLastLinkingRun` string, reused
//!   verbatim) from the CACHE database but does not use it as a filter: the
//!   Rust linker is always a full rebuild (recorded graph-crate deviation,
//!   YAGNI). The run timestamp is still recorded after every successful run
//!   (on the cache database), preserving the oracle's observable contract.
//!   The LLM decision cache and this marker both live on the cache database
//!   (task 1.10); without it the linker runs uncached and records no marker
//!   (the oracle's nil-store no-op, non-fatal).
//! - The runner mutex serializes these mutating entry points (design D4);
//!   the oracle's `PruneDeleted` did not take `r.mu`, which allowed
//!   concurrent SQLite writes from the file watcher.

use std::collections::HashSet;
use std::time::SystemTime;

use db::{AppKv, ChunkDao, ConnectionOrTx, Db, DocumentDao, GcDao};
use graph::LinkResult;
use vectors::VectorIndex;

use super::Runner;
use crate::error::IngestionError;
use crate::parsers::format_rfc3339_utc;

/// app_kv key of the last entity-linking run timestamp (the oracle's
/// `relations.KVKeyLastLinkingRun` string — the key is data contract).
pub const LAST_LINKING_RUN_KEY: &str = "last_linking_run";

/// Counters of one orphan-cleanup run (oracle `OrphanCleanupStats`).
///
/// The oracle's `Errors` field is deliberately absent: failures propagate
/// as `Err` from [`Runner::cleanup_orphaned_data`] instead of being
/// collected (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanCleanupStats {
    /// Entities deleted (no `entity_sources` links, no fact reference, and
    /// not the shared `'EntityType'` infrastructure).
    pub entities_deleted: i64,
    /// Facts deleted (no `fact_sources` rows, unapproved).
    pub facts_deleted: i64,
    /// Documents deleted (no chunks, `entity_sources` or `fact_sources`).
    pub documents_deleted: i64,
    /// Vectors deleted from the index (chunk ids with no live chunk row).
    pub vectors_deleted: usize,
}

impl<'a> Runner<'a> {
    /// Removes orphaned entities, facts and documents in ONE SQLite
    /// transaction, then reconciles the vector index against the live chunk
    /// ids outside the transaction (design D5).
    ///
    /// The chunk row is the source of truth: index vectors whose chunk id
    /// no longer exists are dropped; live chunks keep their vectors.
    pub fn cleanup_orphaned_data(&self) -> Result<OrphanCleanupStats, IngestionError> {
        let _guard = self.lock();
        self.cleanup_orphaned_data_locked()
    }

    /// Removes indexed documents whose source file no longer exists on disk
    /// (oracle `PruneDeleted`): for each document under an enabled source
    /// root whose file is gone, one transaction performs the full
    /// per-document cleanup (chunks, provenance, scoped orphans) and the
    /// document-row deletion. Returns the number of removed documents.
    pub fn prune_deleted(&self) -> Result<usize, IngestionError> {
        let _guard = self.lock();
        self.prune_deleted_locked()
    }

    /// Runs cross-domain entity linking (oracle `BuildEntityLinks`).
    ///
    /// Skips cleanly (empty [`LinkResult`]) when the ontology carries no
    /// `cross-domain-links` block. The last-run timestamp under
    /// [`LAST_LINKING_RUN_KEY`] is read for logging; the Rust linker is
    /// always a full rebuild (recorded graph-crate deviation), so the value
    /// does not filter the run. After a successful run, the current
    /// timestamp is recorded (a record failure warns and does not fail the
    /// run — oracle parity: it is bookkeeping for the next run).
    pub fn build_entity_links(&self) -> Result<LinkResult, IngestionError> {
        let _guard = self.lock();
        self.build_entity_links_locked()
    }

    /// The unlocked core of [`Self::cleanup_orphaned_data`] (callers hold
    /// the runner mutex — the `ingest_all` tail calls this directly).
    pub(super) fn cleanup_orphaned_data_locked(
        &self,
    ) -> Result<OrphanCleanupStats, IngestionError> {
        let mut stats = OrphanCleanupStats::default();
        self.db.exec_tx(|tx| -> Result<(), IngestionError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let gc = GcDao::new(exec);
            stats.entities_deleted = gc.delete_orphaned_entity_ids()?;
            stats.facts_deleted = gc.delete_orphaned_facts()?;
            stats.documents_deleted = gc.delete_orphaned_documents()?;
            Ok(())
        })?;
        stats.vectors_deleted = reconcile_vectors(self.db, self.vectors)?;
        Ok(stats)
    }

    /// The unlocked core of [`Self::prune_deleted`] (callers hold the
    /// runner mutex).
    pub(super) fn prune_deleted_locked(&self) -> Result<usize, IngestionError> {
        let docs = self
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())??;
        let mut removed = 0usize;
        for doc in &docs {
            if !self.belongs_to_source(&doc.original_path) {
                continue; // not under an enabled source root: not ours
            }
            if !file_is_gone(&doc.original_path) {
                continue;
            }
            self.clear_and_delete_doc(doc.id)?;
            removed += 1;
        }
        Ok(removed)
    }

    /// The unlocked core of [`Self::build_entity_links`] (callers hold the
    /// runner mutex — the `ingest_all` tail calls this directly).
    pub(super) fn build_entity_links_locked(&self) -> Result<LinkResult, IngestionError> {
        let Some(links_config) = self.global.and_then(|g| g.cross_domain_links.as_ref()) else {
            eprintln!("build entity links: no cross-domain-links config, skipping");
            return Ok(LinkResult::default());
        };
        // The cache database holds BOTH the linker decision cache
        // (`llm_linker_cache`) and the `last_linking_run` marker (task 1.10).
        // Without it the linker runs uncached and records no marker (the
        // oracle's nil-store no-op, non-fatal).
        let cache = self.llm_cache.as_ref();
        let last_run = match cache {
            Some(cache) => cache.with_conn(|conn| {
                AppKv::new(ConnectionOrTx::Connection(conn)).get(LAST_LINKING_RUN_KEY)
            })??,
            None => None,
        };
        match &last_run {
            Some(since) => eprintln!("entity linking: full rebuild (last run {since})"),
            None => eprintln!("entity linking: full rebuild (no previous run)"),
        }
        let result = graph::build_entity_links(
            self.db,
            cache,
            links_config,
            self.linker_cfg,
            self.prompts_path,
        )?;
        if result.errors.is_empty() {
            eprintln!(
                "entity links built: created={}, skipped={}",
                result.links_created, result.links_skipped
            );
        } else {
            eprintln!(
                "entity links built with {} error(s): {:#?}",
                result.errors.len(),
                result.errors
            );
        }
        record_linking_run(cache);
        Ok(result)
    }
}

/// Drops index vectors whose chunk row no longer exists (design D5
/// reconciliation; the chunk row is the source of truth).
fn reconcile_vectors(db: &Db, vectors: &dyn VectorIndex) -> Result<usize, IngestionError> {
    let live: HashSet<u32> = db
        .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())??
        .into_iter()
        .map(|chunk| chunk.id as u32)
        .collect();
    let stored = vectors.chunk_ids()?;
    let orphaned: Vec<u32> = stored.into_iter().filter(|id| !live.contains(id)).collect();
    if !orphaned.is_empty() {
        vectors.delete_by_chunk_ids(&orphaned)?;
    }
    Ok(orphaned.len())
}

/// True when `path` no longer exists on disk: `NotFound` is "gone"; any
/// other I/O error (transient) is treated as "still there" so a stat
/// failure never destroys a document (see the module docs).
fn file_is_gone(path: &str) -> bool {
    matches!(
        std::fs::metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

/// Records the current run timestamp under [`LAST_LINKING_RUN_KEY`] on the
/// cache database (oracle `kv.Set(relations.KVKeyLastLinkingRun, now)`).
/// `None` (caching disabled) records no marker — the oracle's nil-store
/// no-op. A failure warns and does not propagate (oracle parity: bookkeeping
/// only — the next run is a full rebuild either way).
fn record_linking_run(cache: Option<&Db>) {
    let Some(cache) = cache else {
        return;
    };
    let Some(now) = format_rfc3339_utc(SystemTime::now()) else {
        eprintln!("entity linking: cannot format run timestamp, skipping the record");
        return;
    };
    let recorded = cache
        .with_conn(|conn| {
            AppKv::new(ConnectionOrTx::Connection(conn)).set(LAST_LINKING_RUN_KEY, &now)
        })
        .map_err(|err| err.to_string())
        .and_then(|inner| inner.map(|_| ()).map_err(|err| err.to_string()));
    if let Err(msg) = recorded {
        eprintln!("warning: failed to record linking run timestamp: {msg}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use config::ontology::{CrossDomainLinksConfig, LinkExpression, LinkMethod, SourceType};
    use db::{AppKv, ChunkDao, ConnectionOrTx, Db, DocumentDao, EntityDao, FactDao};
    use graph::LinkResult;
    use vectors::VectorIndex;

    use super::{LAST_LINKING_RUN_KEY, OrphanCleanupStats};
    use crate::runner::tests::{Harness, TempDir, source_config};

    /// A `cross-domain-links` config with the `equals` method (the fixtures
    /// carry no cross-domain candidate pairs, so this is a clean no-op
    /// success).
    fn links_config() -> CrossDomainLinksConfig {
        CrossDomainLinksConfig {
            methods: vec![LinkMethod::Equals],
            equals: None,
            llm_confidence_threshold: 0.7,
            batch_size: 5,
            expressions: Vec::new(),
        }
    }

    /// Reads the last-linking-run timestamp straight from app_kv.
    fn linking_run_ts(db: &Db) -> Option<String> {
        db.with_conn(|conn| AppKv::new(ConnectionOrTx::Connection(conn)).get(LAST_LINKING_RUN_KEY))
            .unwrap()
            .unwrap()
    }

    /// Builds a harness with one enabled unstructured source at `src`.
    fn harness_with_source(src: &std::path::Path) -> Harness {
        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        harness
    }

    #[test]
    fn cleanup_orphaned_data_sweeps_every_orphan_kind_and_keeps_live_data() {
        let root = TempDir::new("cleanup");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello world\n").unwrap();

        let harness = harness_with_source(&src);
        let runner = harness.runner();
        runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();

        // Live data: an entity referenced by a fact (both are protected —
        // the fact is approved, the entity is fact-referenced).
        let live_entity = harness
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    "person",
                    "Live Person",
                    "hr",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        let live_fact = harness
            .db
            .with_conn(|conn| {
                FactDao::new(ConnectionOrTx::Connection(conn)).create(
                    Some(live_entity),
                    "relates_to",
                    None,
                    "hr",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        // One orphan of every kind.
        harness
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    "person",
                    "Ghost Person",
                    "hr",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        let orphan_fact = harness
            .db
            .with_conn(|conn| {
                FactDao::new(ConnectionOrTx::Connection(conn))
                    .create(None, "dangling", None, "hr", None, None, None)
            })
            .unwrap()
            .unwrap();
        // `create` stores 'approved' (protected from orphan GC): flip the
        // status so the fact is a genuine orphan candidate.
        harness
            .db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE facts SET status = 'draft' WHERE id = ?",
                    [orphan_fact],
                )
            })
            .unwrap()
            .unwrap();
        harness
            .db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                    "test",
                    "/elsewhere/ghost.txt",
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        let stats = runner.cleanup_orphaned_data().unwrap();
        assert_eq!(
            stats,
            OrphanCleanupStats {
                entities_deleted: 1,
                facts_deleted: 1,
                documents_deleted: 1,
                vectors_deleted: 0,
            }
        );

        // Live data survived the sweep.
        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 1, "the ingested document must survive");
        assert!(
            harness
                .db
                .with_conn(|conn| {
                    EntityDao::new(ConnectionOrTx::Connection(conn)).get_by_id(live_entity)
                })
                .unwrap()
                .unwrap()
                .is_some(),
            "the fact-referenced entity must survive"
        );
        assert!(
            harness
                .db
                .with_conn(
                    |conn| FactDao::new(ConnectionOrTx::Connection(conn)).get_by_id(live_fact)
                )
                .unwrap()
                .unwrap()
                .is_some(),
            "the approved fact must survive"
        );
        // The live chunk's vector survived the reconciliation.
        assert_eq!(harness.sink.chunk_ids().unwrap().len(), 1);
    }

    #[test]
    fn cleanup_orphaned_data_reconciles_stale_vectors() {
        let root = TempDir::new("vector-reconcile");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "line one\nline two\n").unwrap();

        let harness = harness_with_source(&src);
        let runner = harness.runner();
        runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();

        let chunks = harness
            .db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())
            .unwrap()
            .unwrap();
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        let stale = chunks[0].id;
        let live = chunks[1].id;
        harness
            .db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).delete(stale))
            .unwrap()
            .unwrap();

        let stats = runner.cleanup_orphaned_data().unwrap();
        assert_eq!(stats.vectors_deleted, 1, "{stats:?}");
        // The document survives (it still has a chunk); only the stale
        // vector was dropped.
        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 1, "{docs:?}");
        assert_eq!(harness.sink.chunk_ids().unwrap(), vec![live as u32]);
    }

    #[test]
    fn prune_deleted_removes_vanished_documents_and_keeps_live_ones() {
        let root = TempDir::new("prune");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), "alpha\n").unwrap();
        fs::write(src.join("b.txt"), "beta\n").unwrap();

        let harness = harness_with_source(&src);
        let runner = harness.runner();
        runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();

        fs::remove_file(src.join("a.txt")).unwrap();
        let removed = runner.prune_deleted().unwrap();
        assert_eq!(removed, 1);

        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 1, "{docs:?}");
        assert!(docs[0].original_path.ends_with("b.txt"), "{docs:?}");
        // The pruned document's chunk rows were full-cleared...
        let chunks = harness
            .db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())
            .unwrap()
            .unwrap();
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        // ...and the next cleanup reconciles the pruned chunk's vector
        // (design D5: the chunk row is the source of truth).
        let cleanup = runner.cleanup_orphaned_data().unwrap();
        assert_eq!(cleanup.vectors_deleted, 1, "{cleanup:?}");
        assert_eq!(harness.sink.chunk_ids().unwrap().len(), 1);
    }

    #[test]
    fn prune_deleted_ignores_documents_outside_enabled_sources() {
        let root = TempDir::new("prune-foreign");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let harness = harness_with_source(&src);
        let runner = harness.runner();
        runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();

        // A foreign document row whose file does not exist: not under any
        // enabled source root, so prune must leave it alone.
        let external = harness
            .db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                    "test",
                    "/elsewhere/gone.txt",
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        let removed = runner.prune_deleted().unwrap();
        assert_eq!(removed, 0);
        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 2, "{docs:?}");
        assert!(
            harness
                .db
                .with_conn(
                    |conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).get_by_id(external)
                )
                .unwrap()
                .unwrap()
                .is_some(),
            "the foreign document must survive"
        );
    }

    #[test]
    fn build_entity_links_skips_cleanly_without_config() {
        let harness = Harness::new();
        let runner = harness.runner();

        let result = runner.build_entity_links().unwrap();
        assert_eq!(result, LinkResult::default());

        // The skip path never records a run (marker lives on the cache Db).
        assert!(linking_run_ts(&harness.cache).is_none());
    }

    #[test]
    fn build_entity_links_records_run_timestamp_roundtrip() {
        let mut harness = Harness::new();
        harness.global.cross_domain_links = Some(links_config());
        let runner = harness.runner();

        let result = runner.build_entity_links().unwrap();
        assert_eq!(result.links_created, 0, "{result:?}");
        assert!(result.errors.is_empty(), "{result:?}");

        let first = linking_run_ts(&harness.cache);
        assert!(
            first.as_deref().is_some_and(|ts| ts.ends_with('Z')),
            "{first:?}"
        );

        // A second run upserts the key (full rebuild, idempotent).
        runner.build_entity_links().unwrap();
        assert_eq!(linking_run_ts(&harness.cache), first);
    }

    #[test]
    fn ingest_all_tail_records_cleanup_failure_in_errors() {
        let root = TempDir::new("tail-cleanup");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let harness = harness_with_source(&src);
        let runner = harness.runner();

        // First run: ingest + a clean tail.
        let first = runner.ingest_all(false);
        assert_eq!(first.sources_processed, 1, "{first:?}");
        assert!(first.errors.is_empty(), "{first:?}");

        // Second run: the source is skipped (dedup), but the cleanup tail
        // now fails in the vector engine → the error lands in the summary
        // and never aborts the run.
        harness.sink.set_fail_reads(true);
        let second = runner.ingest_all(false);
        assert_eq!(second.sources_processed, 1, "{second:?}");
        assert_eq!(second.documents_skipped, 1, "{second:?}");
        assert_eq!(second.errors.len(), 1, "{second:?}");
        assert!(
            second.errors[0].starts_with("cleanup orphaned data"),
            "{}",
            second.errors[0]
        );
    }

    #[test]
    fn ingest_all_tail_records_link_errors_in_errors() {
        let mut harness = Harness::new();
        harness.global.cross_domain_links = Some(CrossDomainLinksConfig {
            methods: vec![LinkMethod::Expression],
            equals: None,
            llm_confidence_threshold: 0.7,
            batch_size: 5,
            expressions: vec![LinkExpression {
                name: "bad-rule".to_owned(),
                description: String::new(),
                priority: 0,
                where_: "not a CEL expression ][".to_owned(),
                relation_type: "same_entity".to_owned(),
            }],
        });
        // A candidate pair (same type + name, different domains) kept alive
        // by a fact, so the cleanup tail of `ingest_all` does not sweep it
        // before the linking tail runs.
        let a = harness
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    "person",
                    "Shared Name",
                    "hr",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        let b = harness
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    "person",
                    "Shared Name",
                    "legal",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        harness
            .db
            .with_conn(|conn| {
                FactDao::new(ConnectionOrTx::Connection(conn)).create(
                    Some(a),
                    "related_to",
                    Some(b),
                    "hr",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        let runner = harness.runner();

        // Direct call: the run succeeds, the per-link failure is in the
        // result (non-fatal).
        let result = runner.build_entity_links().unwrap();
        assert_eq!(result.links_created, 0, "{result:?}");
        assert_eq!(result.errors.len(), 1, "{result:?}");
        assert!(
            result.errors[0].starts_with("init expression linker"),
            "{}",
            result.errors[0]
        );

        // Via the ingest_all tail: the same error lands in the summary.
        let stats = runner.ingest_all(false);
        assert!(
            stats
                .errors
                .iter()
                .any(|err| err.starts_with("init expression linker")),
            "{stats:?}"
        );
    }
}
