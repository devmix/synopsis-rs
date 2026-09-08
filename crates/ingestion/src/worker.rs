//! Background event-queue worker (event-queue-incremental-linking task 1.2).
//!
//! [`DocumentWorker`] is the sole consumer of the `queue_tasks` queue: it
//! claims due rows ONE AT A TIME (task 1.5 — at most one row in
//! `processing` at any instant, so `processing` means "currently
//! executing"), dispatches by [`QueueTaskType`] (index / delete /
//! entity-link), records failures with exponential backoff, and sweeps
//! orphaned data after each cycle.
//!
//! The worker runs on the serve owner thread (the [`Runner`] is `!Send +
//! !Sync` — it holds `&dyn` references and a `Mutex<()>` that cannot cross
//! thread boundaries). The owner-thread serve loop drives one blocking
//! [`DocumentWorker::run_once`] call per poll tick, interleaved with the
//! shutdown signal via `tokio::select!`.
//!
//! Cooperative shutdown (fix-serve-signal-shutdown D1/D2): the worker
//! optionally carries a [`ShutdownFlag`] — the minimal synchronous channel
//! from the serve signal task into this `!Send` owner-thread cycle (the
//! worker cannot await anything). When present,
//! [`run_once`](Self::run_once) checks the flag before every claim
//! (including the first); a set flag stops the claim loop and the cycle
//! tail (GC sweep, per-cycle vector persistence, summary) runs uniformly,
//! bounding the graceful-stop delay to the one in-flight task.
//!
//! Backoff schedule (design): `30 * 2^(attempts-1)` seconds, i.e. 30s /
//! 60s / 120s for attempts 1→2→3, after which the task flips to `error`
//! status (no further retries). The `max_attempts` column on the row is the
//! cap (set at enqueue time; the DAO's `mark_failed` enforces it).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use db::{ConnectionOrTx, Db, DocIndexPayload, QueueTask, QueueTaskDao, QueueTaskType, ReIndexOp};

use crate::error::IngestionError;
use crate::runner::Runner;

/// Maximum tasks processed per poll cycle (one-at-a-time claim, task 1.5):
/// the per-cycle starvation guard — a long queue must not starve the owner
/// thread (no config knob in task 1.4).
const WORKER_BATCH_SIZE: i64 = 100;

/// Cooperative shutdown flag shared between the serve signal task and the
/// inline document-queue worker cycles (fix-serve-signal-shutdown D1).
///
/// The worker runs synchronously on the owner thread (the [`Runner`] is
/// `!Send`) and cannot await any cancellation channel; the flag is the
/// minimal synchronous channel into it. The production signal task calls
/// [`cancel`](Self::cancel) on the first SIGINT/SIGTERM (BEFORE sending on
/// the stop broadcast, so the owner loop's stop arm still fires), and the
/// worker checks [`cancelled`](Self::cancelled) before every claim.
///
/// Cloning is cheap (the [`Arc`] is cloned) and shares the same flag state.
pub struct ShutdownFlag {
    /// The shared atomic state (`true` once shutdown has been requested).
    cancelled: Arc<AtomicBool>,
}

impl ShutdownFlag {
    /// Creates a new, uncancelled flag.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests shutdown (sets the flag). Idempotent; safe from any thread.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been requested.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for ShutdownFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for ShutdownFlag {
    fn clone(&self) -> Self {
        Self {
            cancelled: Arc::clone(&self.cancelled),
        }
    }
}

/// The background consumer of the `queue_tasks` queue (task 1.2).
///
/// Holds references to the database and the runner; every poll cycle is one
/// [`run_once`](Self::run_once) call. The worker is `!Send` (the [`Runner`]
/// is `!Send`), so it must run on the owner thread.
pub struct DocumentWorker<'a> {
    /// The knowledge database handle (shared with the runner and the queue).
    db: &'a Db,
    /// The ingestion runner (the per-document pipeline executor).
    runner: &'a Runner<'a>,
    /// The optional cooperative shutdown flag (fix-serve-signal-shutdown
    /// D2): when present, `run_once` checks it before every claim and stops
    /// claiming once it is set.
    shutdown_flag: Option<ShutdownFlag>,
}

impl<'a> DocumentWorker<'a> {
    /// Wraps the shared database handle and runner reference.
    ///
    /// The worker carries no shutdown flag: `run_once` claims up to the
    /// per-cycle cap without any cancellation check (the pre-change
    /// behavior).
    pub fn new(db: &'a Db, runner: &'a Runner<'a>) -> Self {
        Self {
            db,
            runner,
            shutdown_flag: None,
        }
    }

    /// Wraps the shared database handle, runner reference, and shutdown
    /// flag (fix-serve-signal-shutdown D2).
    ///
    /// `run_once` checks the flag before every claim (including the first):
    /// once set, the claim loop stops and the cycle completes its tail as
    /// usual (GC sweep, per-cycle vector persistence when work happened,
    /// cycle summary). The flag is shared cheaply — the [`Arc`] inside is
    /// cloned, not the state.
    pub fn with_shutdown_flag(db: &'a Db, runner: &'a Runner<'a>, flag: &ShutdownFlag) -> Self {
        Self {
            db,
            runner,
            shutdown_flag: Some(flag.clone()),
        }
    }

    /// One poll cycle: claim due tasks ONE AT A TIME (task 1.5 — at most
    /// one row in `processing` at any instant), process each one, then
    /// sweep orphaned data.
    ///
    /// Cancellation contract (fix-serve-signal-shutdown D2): when the
    /// worker was built with a shutdown flag, the flag is checked at the
    /// top of every claim-loop iteration (including before the first
    /// claim); a set flag breaks the claim loop and the cycle tail runs
    /// uniformly below. A task already in flight when the flag is set runs
    /// to completion and is recorded exactly as usual (`done`, or
    /// backoff/`error` on failure), bounding the shutdown delay to one
    /// task. A worker without a flag checks nothing.
    ///
    /// `now` is the current Unix time in seconds (injected for testability).
    ///
    /// # Errors
    ///
    /// Propagates the first unhandled error from the claim stage. Per-task
    /// failures are recorded in the queue (backoff / error status) and never
    /// abort the cycle. A failed orphan cleanup and a failed post-cycle
    /// vector persistence are both logged and do not propagate.
    pub fn run_once(&self, now: i64) -> Result<(), IngestionError> {
        // One-at-a-time claim (task 1.5): the per-cycle cap stays as the
        // owner-thread starvation guard.
        let mut processed = 0;
        for _ in 0..WORKER_BATCH_SIZE {
            // Cooperative shutdown (fix-serve-signal-shutdown D2): the flag
            // is checked before every claim, including the first; a set
            // flag stops the claim loop (the cycle tail runs uniformly
            // below — no separate cancelled-tail path).
            if self
                .shutdown_flag
                .as_ref()
                .is_some_and(ShutdownFlag::cancelled)
            {
                break;
            }
            let Some(task) = self.claim_one(now)? else {
                break;
            };
            self.process_task(&task, now);
            processed += 1;
        }

        // GC phase: sweep orphaned data after draining the cycle.
        if let Err(err) = self.runner.cleanup_orphaned_data() {
            tracing::warn!(error = %err, "orphan cleanup failed");
        }

        if processed > 0 {
            // Per-cycle vector save (vector-loss-self-heal D1): persist the
            // RAM layer after a work cycle so an unclean shutdown (SIGKILL)
            // loses at most the in-progress batch, not everything since the
            // last flush. A failure is logged, never fatal — the next work
            // cycle retries the save and the startup self-heal repairs any
            // residual loss.
            if let Err(err) = self.runner.persist_vectors() {
                tracing::warn!(error = %err, "post-cycle vector persistence failed");
            }

            // Per-cycle progress: log only when work happened (idle cycles
            // stay quiet).
            self.log_cycle_summary(processed);
        }

        Ok(())
    }

    /// Atomically claim one due task over a pooled connection
    /// ([`QueueTaskDao::claim_one`]); `None` when the queue has no due
    /// pending row.
    fn claim_one(&self, now: i64) -> Result<Option<QueueTask>, IngestionError> {
        self.db
            .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).claim_one(now))
            .map_err(IngestionError::Db)
            .and_then(|result| result.map_err(IngestionError::QueueTask))
    }

    /// Logs the per-cycle queue summary: the processed count plus the queue
    /// size grouped by status (via [`QueueTaskDao::status_counts`]).
    fn log_cycle_summary(&self, processed: usize) {
        let Ok(Ok(counts)) = self
            .db
            .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).status_counts())
        else {
            tracing::warn!("failed to read the queue status counts");
            return;
        };
        let summary = counts
            .iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(processed, queue = %summary, "queue cycle finished");
    }

    /// Processes one claimed task: dispatches by type, then records the
    /// outcome in the queue.
    fn process_task(&self, task: &QueueTask, now: i64) {
        let result = match task.task_type.as_str() {
            "doc:index" => self.process_doc_index(task),
            "doc:delete" => self.runner.delete_document_at(&task.identity).map(|_| ()),
            "entity:link" => self.process_entity_link(task),
            other => {
                tracing::error!(identity = %task.identity, task_type = %other, "unknown task type");
                return;
            }
        };

        match result {
            Ok(()) => self.finish_success(task, now),
            Err(err) => self.finish_failure(task, &err.to_string(), now),
        }
    }

    /// Processes a `doc:index` task: routes by the payload's `ops` set
    /// (vector-loss-self-heal D5): `ReEmbed` without `Full` → the targeted
    /// re-embed of the document's existing chunk rows (no parse, no
    /// re-chunk, no NER, no dedup); otherwise (the `Full` default, which
    /// subsumes `ReEmbed`) → the full per-document pipeline.
    fn process_doc_index(&self, task: &QueueTask) -> Result<(), IngestionError> {
        let payload: DocIndexPayload =
            serde_json::from_str(&task.event).map_err(db::QueueTaskError::Json)?;
        if payload.ops.contains(&ReIndexOp::ReEmbed) && !payload.ops.contains(&ReIndexOp::Full) {
            self.runner.reembed_document(&task.identity)
        } else {
            self.runner.process_document_by_path(&task.identity)
        }
    }

    /// Processes an `entity:link` task: parses the doc_id from the identity
    /// and the entity_ids from the payload, then calls the runner's
    /// incremental linking entry point.
    fn process_entity_link(&self, task: &QueueTask) -> Result<(), IngestionError> {
        use serde::de::Error as _;
        let doc_id: i64 = task.identity.parse().map_err(|_| {
            IngestionError::QueueTask(db::QueueTaskError::Json(serde_json::Error::custom(
                format!("invalid doc_id in entity:link identity: {}", task.identity),
            )))
        })?;
        let payload: db::EntityLinkPayload =
            serde_json::from_str(&task.event).map_err(db::QueueTaskError::Json)?;
        self.runner.link_entities(doc_id, &payload.entity_ids)
    }

    /// Records a successful task: `mark_done` for index/entity-link,
    /// `delete` for delete (the row is a one-shot operation).
    fn finish_success(&self, task: &QueueTask, now: i64) {
        let result = self
            .db
            .with_conn(|conn| {
                let dao = QueueTaskDao::new(ConnectionOrTx::Connection(conn));
                if task.task_type == "doc:delete" {
                    dao.delete(QueueTaskType::DocDelete, &task.identity)
                } else {
                    dao.mark_done(task.id, now)
                }
            })
            .map_err(IngestionError::Db)
            .and_then(|r| r.map_err(IngestionError::QueueTask));
        match result {
            Ok(true) | Ok(false) => {
                tracing::info!(identity = %task.identity, task_type = %task.task_type, "queue task completed");
            }
            Err(err) => {
                tracing::error!(identity = %task.identity, error = %err, "failed to record success");
            }
        }
    }

    /// Records a failed task with exponential backoff
    /// (`30 * 2^(attempts-1)` seconds, `attempts` = the post-failure count).
    /// At the cap, the task flips to `error` status.
    fn finish_failure(&self, task: &QueueTask, err: &str, now: i64) {
        let result = self
            .db
            .with_conn(|conn| {
                QueueTaskDao::new(ConnectionOrTx::Connection(conn)).mark_failed(task.id, err, now)
            })
            .map_err(IngestionError::Db)
            .and_then(|r| r.map_err(IngestionError::QueueTask));
        match result {
            Ok(true) => {
                if task.attempts + 1 >= task.max_attempts {
                    tracing::error!(
                        identity = %task.identity,
                        task_type = %task.task_type,
                        attempts = task.attempts + 1,
                        error = %err,
                        "queue task failed at the retry cap, marking as error"
                    );
                } else {
                    tracing::warn!(
                        identity = %task.identity,
                        task_type = %task.task_type,
                        attempt = task.attempts + 1,
                        error = %err,
                        "queue task attempt failed, retrying with backoff"
                    );
                }
            }
            Ok(false) => {
                tracing::warn!(identity = %task.identity, "no task row (already removed?)");
            }
            Err(e) => {
                tracing::error!(identity = %task.identity, error = %e, "failed to record failure");
            }
        }
    }
}

/// Exponential backoff: `30 * 2^(attempts-1)` seconds (capped at 2^20 to
/// avoid overflow), where `attempts` is the post-failure count.
///
/// The argument is the task's `attempts` field BEFORE this failure is
/// recorded (the 0-indexed previous-failure count), so the exponent is
/// `attempts`:
/// - 0 previous failures → 30s (after attempt 1)
/// - 1 previous failure → 60s (after attempt 2)
/// - 2 previous failures → 120s (after attempt 3)
#[cfg(test)]
fn backoff_seconds(attempts: i32) -> i64 {
    let exponent = (attempts as u32).min(20);
    30i64 * (1i64 << exponent)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use config::DomainConfig;
    use config::ontology::{GlobalConfig, GlobalNerConfig, SourceConfig, SourceType};
    use config::preset::{IngestionConfig, LinkerConfig};
    use db::test_util::in_memory_db;
    use db::{
        ChunkDao, ConnectionOrTx, Db, DocIndexPayload, DocumentDao, EntityDao, QueueTaskDao,
        QueueTaskType, ReIndexOp,
    };
    use embedding::{EmbeddingError, EmbeddingProvider};
    use vectors::{VectorIndex, VectorsError};

    use super::*;
    use crate::error::IngestionError;
    use crate::ner::load_ner_prompts;
    use crate::parsers::tests::TempTree;
    use crate::parsers::walk_matched_files;
    use crate::runner::RunnerParams;
    use crate::sources::Registry;
    use crate::types::{
        Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source,
    };

    /// A minimal source that walks `.txt` files and emits one document per
    /// file (same contract as the runner/job-queue test sources).
    struct TestSource;

    impl TestSource {
        /// Reads one `.txt` file into a test document.
        fn read_file(path: &Path) -> Result<Document, IngestionError> {
            let content = fs::read_to_string(path).map_err(|source| IngestionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(Document {
                source_path: path.to_path_buf(),
                content,
                metadata: DocumentMetadata {
                    source_type: "test".to_owned(),
                    ..Default::default()
                },
            })
        }
    }

    impl Parser for TestSource {
        fn parse(&self, source_path: &Path) -> ParseResult {
            let mut documents = Vec::new();
            let mut errors = Vec::new();
            walk_matched_files(
                source_path,
                |path| path.extension().is_some_and(|ext| ext == "txt"),
                |path| {
                    documents.push(Self::read_file(path)?);
                    Ok(())
                },
                &mut errors,
            );
            ParseResult { documents, errors }
        }

        fn parse_file(&self, path: &Path, _root: &Path) -> Result<Document, IngestionError> {
            Self::read_file(path)
        }

        fn supported_extensions(&self) -> &[&str] {
            &[".txt"]
        }
    }

    impl Chunker for TestSource {
        fn chunk(
            &self,
            content: &str,
            metadata: &DocumentMetadata,
        ) -> Result<Vec<DocumentChunk>, IngestionError> {
            let mut chunks = Vec::new();
            let mut start = 0usize;
            for (seq, line) in content.split_inclusive('\n').enumerate() {
                let end = start + line.len();
                if !line.trim().is_empty() {
                    chunks.push(DocumentChunk {
                        text: line.to_owned(),
                        search_text: line.to_owned(),
                        sequence_num: seq,
                        start_offset: start,
                        end_offset: end,
                        metadata: metadata.extra.clone(),
                    });
                }
                start = end;
            }
            Ok(chunks)
        }
    }

    impl Source for TestSource {}

    /// A deterministic embedding provider: vector `i` is all `(i + 1)`.
    /// `fail_marker` makes it error on any batch containing a text with that
    /// substring (per-document failure path). `probe` (task 1.5) is an
    /// optional observer invoked before every embedding batch — mid-pipeline
    /// queue-state checks run there (the embedding step holds no DB
    /// connection). `embedded_texts` counts the total texts embedded across
    /// all batches (the re-embed routing test asserts the provider saw
    /// exactly the stored chunk count).
    struct MockEmbedding {
        dim: usize,
        fail_marker: Mutex<Option<String>>,
        probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
        embedded_texts: Mutex<usize>,
    }

    impl MockEmbedding {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                fail_marker: Mutex::new(None),
                probe: Mutex::new(None),
                embedded_texts: Mutex::new(0),
            }
        }

        /// Total texts embedded so far (test helper).
        fn embedded_texts(&self) -> usize {
            *self.embedded_texts.lock().unwrap()
        }
    }

    impl EmbeddingProvider for MockEmbedding {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            if let Some(probe) = self.probe.lock().unwrap().as_ref() {
                probe();
            }
            if let Some(marker) = self.fail_marker.lock().unwrap().as_ref()
                && texts.iter().any(|text| text.contains(marker))
            {
                return Err(EmbeddingError::Ort("simulated engine failure".to_owned()));
            }
            *self.embedded_texts.lock().unwrap() += texts.len();
            Ok((0..texts.len())
                .map(|i| vec![(i + 1) as f32; self.dim])
                .collect())
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    /// In-memory [`VectorIndex`] stub: records every row; `build_index`
    /// counts its calls (the per-cycle persistence tests) and can be made
    /// to fail (the persistence-failure path).
    struct MemoryIndex {
        rows: Mutex<Vec<(u32, Vec<f32>)>>,
        build_index_calls: Mutex<usize>,
        fail_build_index: Mutex<bool>,
    }

    impl MemoryIndex {
        fn new() -> Self {
            Self {
                rows: Mutex::new(Vec::new()),
                build_index_calls: Mutex::new(0),
                fail_build_index: Mutex::new(false),
            }
        }

        /// How many times `build_index` has been called (test helper).
        fn build_index_calls(&self) -> usize {
            *self.build_index_calls.lock().unwrap()
        }

        /// Makes `build_index` fail (the persistence-failure path, test
        /// helper).
        fn set_fail_build_index(&self, fail: bool) {
            *self.fail_build_index.lock().unwrap() = fail;
        }

        /// Drops every recorded row (test helper: simulating a lost RAM
        /// layer while the chunk rows remain).
        fn clear(&self) {
            self.rows.lock().unwrap().clear();
        }
    }

    impl VectorIndex for MemoryIndex {
        fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
            self.rows.lock().unwrap().push((chunk_id, vector.to_vec()));
            Ok(())
        }

        fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
            let mut map = self.rows.lock().unwrap();
            for (id, vector) in rows {
                map.push((*id, vector.to_vec()));
            }
            Ok(())
        }

        fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            Ok(Vec::new())
        }

        fn delete_by_chunk_ids(&self, _chunk_ids: &[u32]) -> Result<(), VectorsError> {
            Ok(())
        }

        fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect())
        }

        fn count(&self) -> Result<u64, VectorsError> {
            Ok(self.rows.lock().unwrap().len() as u64)
        }

        fn build_index(&self) -> Result<(), VectorsError> {
            *self.build_index_calls.lock().unwrap() += 1;
            if *self.fail_build_index.lock().unwrap() {
                return Err(VectorsError::Engine("simulated persist failure".to_owned()));
            }
            Ok(())
        }

        fn rebuild(&self, _rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
            Ok(())
        }
    }

    /// Test harness: in-memory DB, one configured source, mock collaborators.
    struct Harness {
        db: Db,
        cfg: IngestionConfig,
        global: GlobalConfig,
        domains: std::collections::HashMap<String, DomainConfig>,
        registry: Registry,
        embed: Arc<MockEmbedding>,
        sink: Arc<MemoryIndex>,
        prompts: crate::ner::NerPrompts,
        linker: LinkerConfig,
    }

    impl Harness {
        fn new() -> Self {
            let mut registry = Registry::new();
            registry
                .register("unstructured", Box::new(TestSource))
                .unwrap();
            Self {
                db: in_memory_db(),
                cfg: IngestionConfig::default(),
                global: GlobalConfig {
                    sources: Vec::new(),
                    cross_domain_links: None,
                    ner: GlobalNerConfig { methods: vec![] },
                    entities: Vec::new(),
                    relations: Vec::new(),
                    extraction: Default::default(),
                },
                domains: std::collections::HashMap::new(),
                registry,
                embed: Arc::new(MockEmbedding::new(4)),
                sink: Arc::new(MemoryIndex::new()),
                prompts: load_ner_prompts("/nonexistent-ner-prompts").unwrap(),
                linker: LinkerConfig::default(),
            }
        }

        /// Registers one enabled unstructured source at `path`.
        fn with_source(&mut self, path: &str) {
            self.global.sources.push(SourceConfig {
                path: path.to_owned(),
                source_type: SourceType::Unstructured,
                disabled: false,
                space: String::new(),
                domains: Vec::new(),
                dataset: String::new(),
            });
        }

        fn runner(&self) -> Runner<'_> {
            Runner::new(RunnerParams {
                db: &self.db,
                ingest_cfg: &self.cfg,
                global: Some(&self.global),
                domains: &self.domains,
                registry: &self.registry,
                embed: self.embed.as_ref(),
                vectors: self.sink.as_ref(),
                prompts: &self.prompts,
                linker_cfg: &self.linker,
                prompts_path: "/nonexistent-prompts",
                llm_cache: None,
            })
        }

        /// All `doc:*` tasks (test helper).
        fn list_doc_tasks(&self) -> Vec<db::QueueTask> {
            self.db
                .with_conn(|conn| {
                    QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None)
                })
                .unwrap()
                .unwrap()
                .into_iter()
                .filter(|t| t.task_type == "doc:index" || t.task_type == "doc:delete")
                .collect()
        }

        /// All `documents` rows (test helper).
        fn list_documents(&self) -> Vec<db::Document> {
            self.db
                .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
                .unwrap()
                .unwrap()
        }

        /// Enqueues a `doc:index` task for `path` (the `Full` op: the full
        /// per-document pipeline).
        fn enqueue_index(&self, path: &str, source: &str) {
            let now = 1_000;
            self.db
                .with_conn(|conn| {
                    QueueTaskDao::new(ConnectionOrTx::Connection(conn)).enqueue(
                        QueueTaskType::DocIndex,
                        path,
                        &DocIndexPayload {
                            source_path: source.to_owned(),
                            content_hash: None,
                            ops: vec![ReIndexOp::Full],
                        },
                        now,
                    )
                })
                .unwrap()
                .unwrap();
        }

        /// Enqueues a `doc:index` task for `path` with the `ReEmbed` op
        /// (vector-loss-self-heal D5: the targeted re-embed of the existing
        /// chunk rows).
        fn enqueue_reembed(&self, path: &str, source: &str) {
            let now = 1_000;
            self.db
                .with_conn(|conn| {
                    QueueTaskDao::new(ConnectionOrTx::Connection(conn)).enqueue(
                        QueueTaskType::DocIndex,
                        path,
                        &DocIndexPayload {
                            source_path: source.to_owned(),
                            content_hash: None,
                            ops: vec![ReIndexOp::ReEmbed],
                        },
                        now,
                    )
                })
                .unwrap()
                .unwrap();
        }

        /// Enqueues a `doc:delete` task for `path`.
        fn enqueue_delete(&self, path: &str) {
            let now = 1_000;
            self.db
                .with_conn(|conn| {
                    QueueTaskDao::new(ConnectionOrTx::Connection(conn)).enqueue(
                        QueueTaskType::DocDelete,
                        path,
                        &db::DocDeletePayload {
                            source_path: String::new(),
                        },
                        now,
                    )
                })
                .unwrap()
                .unwrap();
        }
    }

    // A successful index task creates the document and marks the task done.
    #[test]
    fn run_once_processes_a_successful_index_task() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // Enqueue an index task for the file.
        harness.enqueue_index(&format!("{root}/a.txt"), &root);

        // One poll cycle: the task is claimed, processed, and marked done.
        worker.run_once(2_000).unwrap();

        // The document was created.
        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the index task must create the document");
        assert_eq!(docs[0].original_path, format!("{root}/a.txt"), "{docs:?}");

        // The task is marked done.
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 1, "the task row must still exist");
        assert_eq!(tasks[0].status, "done", "{tasks:?}");
    }

    // A delete task removes the document and the task row.
    #[test]
    fn run_once_processes_a_delete_task() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // First, index the document.
        harness.enqueue_index(&format!("{root}/a.txt"), &root);
        worker.run_once(2_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);

        // Now, enqueue a delete task.
        harness.enqueue_delete(&format!("{root}/a.txt"));

        // One poll cycle: the delete is processed and the task row is removed.
        worker.run_once(3_000).unwrap();

        // The document was removed.
        assert_eq!(
            harness.list_documents().len(),
            0,
            "the delete task must remove the document"
        );
        // The task row was removed.
        let remaining: Vec<_> = harness
            .list_doc_tasks()
            .into_iter()
            .filter(|t| t.identity == format!("{root}/a.txt"))
            .collect();
        // The doc:index row is still there (done), but the doc:delete row is gone.
        assert!(
            remaining.iter().all(|t| t.task_type == "doc:index"),
            "the doc:delete row must be gone, got {remaining:?}"
        );
    }

    // A failing index task gets backoff and eventually error status.
    #[test]
    fn run_once_records_failure_with_backoff_and_error_at_cap() {
        let tree = TempTree::new();
        tree.write("a.txt", "FAIL content\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.cfg.max_retries = 3;
        harness.with_source(&root);
        *harness.embed.fail_marker.lock().unwrap() = Some("FAIL".to_owned());
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // Enqueue an index task.
        harness.enqueue_index(&format!("{root}/a.txt"), &root);

        // Attempt 1: fails, backoff 30s.
        worker.run_once(2_000).unwrap();
        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.attempts, 1, "first failure recorded");
        assert_eq!(task.status, "pending", "below cap: back to pending");
        assert_eq!(task.next_attempt_at, 2_030, "30s backoff");

        // Attempt 2: advance the clock past the backoff, fail again, backoff 60s.
        worker.run_once(2_100).unwrap();
        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.attempts, 2, "second failure recorded");
        assert_eq!(task.status, "pending", "below cap: back to pending");
        assert_eq!(task.next_attempt_at, 2_160, "60s backoff from now=2100");

        // Attempt 3: advance the clock, fail again → error status.
        worker.run_once(2_200).unwrap();
        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.attempts, 3, "third failure recorded");
        assert_eq!(task.status, "error", "at cap: error status");
        assert!(task.last_error.is_some(), "last_error must be set");

        // An error task is not claimable anymore.
        worker.run_once(10_000).unwrap();
        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.status, "error", "error tasks are not re-claimed");
    }

    // End-to-end: a failing document task is retried to the cap
    // (`error`), `reset_retries` re-queues it, and once the failure is
    // cleared the worker processes the document to `done`.
    #[test]
    fn run_once_e2e_failing_doc_retries_reset_reprocesses_to_done() {
        let tree = TempTree::new();
        let file = tree.write("a.txt", "FAIL content\n");
        let root = tree.0.to_string_lossy().into_owned();
        let path = file.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.cfg.max_retries = 3;
        harness.with_source(&root);
        // The pipeline fails on the "FAIL" marker until it is cleared below.
        *harness.embed.fail_marker.lock().unwrap() = Some("FAIL".to_owned());
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // Enqueue the failing index task.
        harness.enqueue_index(&path, &root);

        // Retry up to the cap: three failures drive the task to `error`.
        worker.run_once(2_000).unwrap();
        worker.run_once(2_100).unwrap();
        worker.run_once(2_200).unwrap();

        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.status, "error", "at cap: error status, got {task:?}");
        assert_eq!(
            task.attempts, 3,
            "all three attempts recorded, got {task:?}"
        );
        assert_eq!(
            task.max_attempts, 3,
            "the cap is the configured max, got {task:?}"
        );
        assert!(
            task.last_error.is_some(),
            "last_error must be set, got {task:?}"
        );

        // `reset_retries` (the CLI `queue reset-retries` wraps this) re-queues
        // the error task as pending with attempts 0.
        let reset = harness
            .db
            .with_conn(|conn| {
                QueueTaskDao::new(ConnectionOrTx::Connection(conn)).reset_retries(None, Some(&path))
            })
            .unwrap()
            .unwrap();
        assert!(reset >= 1, "the error task must be reset");

        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(
            task.status, "pending",
            "reset: back to pending, got {task:?}"
        );
        assert_eq!(task.attempts, 0, "reset: attempts cleared, got {task:?}");

        // Clear the failure and run one more cycle: the document is indexed
        // and the task is marked done. `reset_retries` set `next_attempt_at`
        // to the real wall clock, so claim at a time safely past it.
        *harness.embed.fail_marker.lock().unwrap() = None;
        let due: i64 = harness
            .db
            .with_conn(|conn| {
                conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |row| {
                    row.get(0)
                })
            })
            .unwrap()
            .unwrap();
        worker.run_once(due + 1).unwrap();

        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.status, "done", "re-processed to done, got {task:?}");

        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the document must exist, got {docs:?}");
        assert_eq!(docs[0].original_path, path, "{docs:?}");
    }

    // The converge path: a file that was deleted since enqueue → document
    // row removed, task marked done.
    #[test]
    fn run_once_converges_on_deleted_file() {
        let tree = TempTree::new();
        let file = tree.write("a.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // Index the document first.
        harness.enqueue_index(&file.to_string_lossy(), &root);
        worker.run_once(2_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);

        // Delete the file from disk.
        fs::remove_file(&file).unwrap();

        // Enqueue a new index task (simulating a re-enqueue after the file
        // was deleted).
        harness.enqueue_index(&file.to_string_lossy(), &root);

        // The worker converges: removes the document, marks the task done.
        worker.run_once(3_000).unwrap();
        assert_eq!(
            harness.list_documents().len(),
            0,
            "the document must be removed (converge)"
        );
        let tasks: Vec<_> = harness
            .list_doc_tasks()
            .into_iter()
            .filter(|t| t.task_type == "doc:index")
            .collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, "done", "the converge path marks done");
    }

    // GC phase: after draining a batch, orphaned data is swept — the
    // orphaned rows are removed, the live document survives.
    #[test]
    fn run_once_runs_gc_after_the_cycle() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // Enqueue + process a successful index task (one full cycle).
        harness.enqueue_index(&format!("{root}/a.txt"), &root);
        worker.run_once(2_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);

        // Seed one orphan of two kinds: a document row without chunks and an
        // unreferenced entity row.
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

        // A second cycle: the GC sweep at its tail removes the orphans.
        worker.run_once(3_000).unwrap();

        let docs = harness.list_documents();
        assert_eq!(
            docs.len(),
            1,
            "the orphaned document must be swept, the live one kept: {docs:?}"
        );
        assert_eq!(docs[0].original_path, format!("{root}/a.txt"), "{docs:?}");
        let entities: i64 = harness
            .db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(entities, 0, "the unreferenced entity must be swept");
    }

    // A work cycle persists the vector RAM layer (vector-loss-self-heal
    // D1): exactly one `build_index` call after processing a `doc:index`
    // task.
    #[test]
    fn run_once_persists_vectors_after_a_work_cycle() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        harness.enqueue_index(&format!("{root}/a.txt"), &root);
        worker.run_once(2_000).unwrap();

        assert_eq!(
            harness.sink.build_index_calls(),
            1,
            "a work cycle must persist the RAM layer exactly once"
        );
        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the document must be created: {docs:?}");
    }

    // An idle cycle performs no persistence (vector-loss-self-heal D1):
    // the RAM layer changed nothing, so the disk write is skipped.
    #[test]
    fn run_once_does_not_persist_on_an_idle_cycle() {
        let harness = Harness::new();
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        worker.run_once(2_000).unwrap();

        assert_eq!(
            harness.sink.build_index_calls(),
            0,
            "an idle cycle must not persist the RAM layer"
        );
    }

    // A post-cycle persistence failure is non-fatal (vector-loss-self-heal
    // D1): the cycle completes `Ok`, the document is created, and the task
    // is marked done.
    #[test]
    fn run_once_persistence_failure_is_non_fatal() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        harness.sink.set_fail_build_index(true);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        harness.enqueue_index(&format!("{root}/a.txt"), &root);
        let result = worker.run_once(2_000);
        assert!(
            result.is_ok(),
            "a persistence failure must not abort the cycle: {result:?}"
        );

        let docs = harness.list_documents();
        assert_eq!(
            docs.len(),
            1,
            "the document must still be created: {docs:?}"
        );
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(
            tasks[0].status, "done",
            "the task must still be done: {tasks:?}"
        );
    }

    // A `doc:index` task with `ops: [ReEmbed]` routes to the targeted
    // re-embed, not the full pipeline (vector-loss-self-heal D5): the
    // document is unchanged on disk (the full pipeline's content-hash dedup
    // would skip it and embed nothing), yet the re-embed restores the lost
    // vectors from the stored chunk rows.
    #[test]
    fn run_once_routes_reembed_ops_to_the_reembed_path() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);

        // First, index the document through the full pipeline.
        harness.enqueue_index(&format!("{root}/a.txt"), &root);
        worker.run_once(2_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);
        let chunk_count: i64 = harness
            .db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).count())
            .unwrap()
            .unwrap();
        assert!(chunk_count > 0, "the pipeline must have created chunks");
        let chunk_ids = harness.sink.chunk_ids().unwrap();
        assert_eq!(
            chunk_ids.len(),
            chunk_count as usize,
            "every chunk has a vector"
        );

        // Simulate the lost RAM layer: the vectors are gone, the chunk rows
        // and the file on disk are unchanged.
        harness.sink.clear();
        let embedded_before = harness.embed.embedded_texts();

        // The re-embed task: the same identity, the ReEmbed op.
        harness.enqueue_reembed(&format!("{root}/a.txt"), &root);
        worker.run_once(3_000).unwrap();

        // The task is done and the vectors are restored for exactly the
        // stored chunk ids.
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].status, "done", "{tasks:?}");
        let restored = harness.sink.chunk_ids().unwrap();
        assert_eq!(
            restored.len(),
            chunk_count as usize,
            "the lost vectors must be restored"
        );
        for id in &chunk_ids {
            assert!(restored.contains(id), "chunk {id} must be re-embedded");
        }

        // The re-embed path ran (not the full pipeline): the provider saw
        // exactly the stored chunk count in this cycle — the full pipeline's
        // hash-dedup would have skipped the unchanged document and embedded
        // nothing.
        assert_eq!(
            harness.embed.embedded_texts() - embedded_before,
            chunk_count as usize,
            "exactly the stored chunk count is re-embedded (no re-chunk)"
        );
    }

    // Task 1.5 invariant: with N due rows, at most one row is `processing`
    // at any instant during a worker cycle; the rest stay `pending`. The
    // probe observes the queue state from inside the pipeline (the embedding
    // step of each task, which holds no DB connection).
    #[test]
    fn run_once_keeps_at_most_one_row_processing() {
        let tree = TempTree::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            tree.write(name, "hello\n");
        }
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);
        for name in ["a.txt", "b.txt", "c.txt"] {
            harness.enqueue_index(&format!("{root}/{name}"), &root);
        }

        let observations = Arc::new(Mutex::new(Vec::new()));
        let obs = observations.clone();
        let db = harness.db.clone();
        *harness.embed.probe.lock().unwrap() = Some(Box::new(move || {
            let (processing, pending): (i64, i64) = db
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT SUM(CASE WHEN status = 'processing' THEN 1 ELSE 0 END), \
                         SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END) \
                         FROM queue_tasks",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                })
                .unwrap()
                .unwrap();
            obs.lock().unwrap().push((processing, pending));
        }));

        worker.run_once(2_000).unwrap();

        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 3, "one embedding batch per task");
        for (i, (processing, pending)) in observations.iter().enumerate() {
            assert_eq!(*processing, 1, "task {i}: exactly one row in processing");
            assert_eq!(
                *pending,
                3 - i as i64 - 1,
                "task {i}: the rest stay pending"
            );
        }
        let tasks = harness.list_doc_tasks();
        assert!(
            tasks.iter().all(|t| t.status == "done"),
            "all three tasks are done: {tasks:?}"
        );
    }

    // Crash simulation (task 1.5): a claimed row (`processing`) survives a
    // "restart" — the startup recovery path resets it to `pending`
    // (attempts/last_error preserved), and a fresh worker's `run_once`
    // processes it to `done`.
    #[test]
    fn processing_row_survives_a_restart_via_recover_stuck_processing() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        harness.enqueue_index(&format!("{root}/a.txt"), &root);

        // Simulate an unclean shutdown mid-processing: the row is claimed
        // (processing) and never finished.
        let claimed = harness
            .db
            .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).claim_one(2_000))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, "processing");

        // "Restart": the serve startup recovery path (task 1.5).
        let recovered = harness
            .db
            .with_conn(|conn| {
                QueueTaskDao::new(ConnectionOrTx::Connection(conn)).recover_stuck_processing(3_000)
            })
            .unwrap()
            .unwrap();
        assert_eq!(recovered, 1, "the stuck row is recovered");
        let task = harness.list_doc_tasks()[0].clone();
        assert_eq!(task.status, "pending", "recovered to pending, got {task:?}");
        assert_eq!(task.attempts, 0, "attempts are preserved, got {task:?}");

        // A fresh worker's run_once processes the recovered row to done.
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner);
        worker.run_once(4_000).unwrap();

        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].status, "done", "{tasks:?}");
        assert_eq!(harness.list_documents().len(), 1, "the document is indexed");
    }

    // Cooperative shutdown (fix-serve-signal-shutdown 1.1): the flag is set
    // before the cycle → no claims at all, every task stays pending, the
    // cycle completes normally (the uniform tail with `processed == 0`
    // performs no persistence).
    #[test]
    fn run_once_claims_nothing_when_cancelled_before_the_cycle() {
        let tree = TempTree::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            tree.write(name, "hello\n");
        }
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let flag = ShutdownFlag::new();
        let runner = harness.runner();
        let worker = DocumentWorker::with_shutdown_flag(&harness.db, &runner, &flag);
        for name in ["a.txt", "b.txt", "c.txt"] {
            harness.enqueue_index(&format!("{root}/{name}"), &root);
        }

        // The signal arrived before this cycle: cancel, then run.
        flag.cancel();
        worker.run_once(2_000).unwrap();

        // Nothing was claimed or processed.
        assert_eq!(
            harness.list_documents().len(),
            0,
            "no task may be processed after the flag was set"
        );
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 3, "{tasks:?}");
        assert!(
            tasks.iter().all(|t| t.status == "pending"),
            "all tasks stay pending: {tasks:?}"
        );
    }

    // Cooperative shutdown (fix-serve-signal-shutdown 1.1, the field case):
    // the flag is set during task 1's embedding (the probe hook, invoked
    // before every embedding batch) → task 1 runs to completion and is
    // marked done; the claim loop stops; tasks 2-3 stay pending.
    #[test]
    fn run_once_finishes_the_in_flight_task_and_stops_when_cancelled_mid_cycle() {
        let tree = TempTree::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            tree.write(name, "hello\n");
        }
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let flag = ShutdownFlag::new();
        // The probe runs before every embedding batch; cancelling there (the
        // operation is idempotent) cancels during task 1's embedding, before
        // any further claim.
        let probe_flag = flag.clone();
        *harness.embed.probe.lock().unwrap() = Some(Box::new(move || {
            probe_flag.cancel();
        }));
        let runner = harness.runner();
        let worker = DocumentWorker::with_shutdown_flag(&harness.db, &runner, &flag);
        for name in ["a.txt", "b.txt", "c.txt"] {
            harness.enqueue_index(&format!("{root}/{name}"), &root);
        }

        worker.run_once(2_000).unwrap();

        // The in-flight task ran to completion; nothing was claimed after it.
        assert_eq!(
            harness.list_documents().len(),
            1,
            "exactly the in-flight document is indexed"
        );
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 3, "{tasks:?}");
        for t in &tasks {
            if t.identity == format!("{root}/a.txt") {
                assert_eq!(t.status, "done", "the in-flight task completes: {tasks:?}");
            } else {
                assert_eq!(t.status, "pending", "no further claims: {tasks:?}");
            }
        }
    }

    // Cooperative shutdown (fix-serve-signal-shutdown 1.1): an in-flight
    // failure during shutdown is recorded with the usual backoff semantics
    // (the flag changes claim behavior, not failure recording); no further
    // claims.
    #[test]
    fn run_once_records_backoff_for_an_in_flight_failure_during_shutdown() {
        let tree = TempTree::new();
        // Task 1 carries the fail marker; tasks 2-3 would succeed if claimed.
        tree.write("a.txt", "FAIL hello\n");
        tree.write("b.txt", "hello\n");
        tree.write("c.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.cfg.max_retries = 3;
        harness.with_source(&root);
        *harness.embed.fail_marker.lock().unwrap() = Some("FAIL".to_owned());
        let flag = ShutdownFlag::new();
        let probe_flag = flag.clone();
        *harness.embed.probe.lock().unwrap() = Some(Box::new(move || {
            probe_flag.cancel();
        }));
        let runner = harness.runner();
        let worker = DocumentWorker::with_shutdown_flag(&harness.db, &runner, &flag);
        for name in ["a.txt", "b.txt", "c.txt"] {
            harness.enqueue_index(&format!("{root}/{name}"), &root);
        }

        worker.run_once(2_000).unwrap();

        // No document: task 1 failed, tasks 2-3 were never claimed.
        assert_eq!(harness.list_documents().len(), 0, "no document is indexed");
        let tasks = harness.list_doc_tasks();
        assert_eq!(tasks.len(), 3, "{tasks:?}");
        let by_name = |name: &str| -> &db::QueueTask {
            tasks.iter().find(|t| t.identity.ends_with(name)).unwrap()
        };
        let first = by_name("a.txt");
        assert_eq!(
            first.attempts, 1,
            "the in-flight failure is recorded: {tasks:?}"
        );
        assert_eq!(
            first.status, "pending",
            "below the cap: back to pending: {tasks:?}"
        );
        assert_eq!(
            first.next_attempt_at, 2_030,
            "the usual 30s backoff: {tasks:?}"
        );
        assert!(
            first.last_error.is_some(),
            "last_error must be set: {tasks:?}"
        );
        for name in ["b.txt", "c.txt"] {
            let t = by_name(name);
            assert_eq!(t.attempts, 0, "task {name} was never claimed: {tasks:?}");
            assert_eq!(t.status, "pending", "task {name} stays pending: {tasks:?}");
            assert_eq!(
                t.next_attempt_at, 1_000,
                "task {name} is untouched: {tasks:?}"
            );
        }
    }

    // Cooperative shutdown (fix-serve-signal-shutdown 1.1): a cancelled
    // work cycle still runs the uniform tail — the per-cycle vector
    // persistence (vector-loss-self-heal D1) saves the in-flight work,
    // exactly once.
    #[test]
    fn run_once_persists_the_ram_layer_on_a_cancelled_work_cycle() {
        let tree = TempTree::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            tree.write(name, "hello\n");
        }
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let flag = ShutdownFlag::new();
        let probe_flag = flag.clone();
        *harness.embed.probe.lock().unwrap() = Some(Box::new(move || {
            probe_flag.cancel();
        }));
        let runner = harness.runner();
        let worker = DocumentWorker::with_shutdown_flag(&harness.db, &runner, &flag);
        for name in ["a.txt", "b.txt", "c.txt"] {
            harness.enqueue_index(&format!("{root}/{name}"), &root);
        }

        worker.run_once(2_000).unwrap();

        // One task was processed → the uniform tail persists exactly once.
        assert_eq!(
            harness.sink.build_index_calls(),
            1,
            "the cancelled work cycle must run the per-cycle save exactly once"
        );
        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the in-flight document is indexed: {docs:?}");
    }

    // Unit test for the backoff formula.
    #[test]
    fn backoff_seconds_follows_exponential_schedule() {
        assert_eq!(backoff_seconds(0), 30);
        assert_eq!(backoff_seconds(1), 60);
        assert_eq!(backoff_seconds(2), 120);
        assert_eq!(backoff_seconds(3), 240);
        // Capped at 2^20 to avoid overflow.
        assert_eq!(backoff_seconds(20), 30 * (1i64 << 20));
        assert_eq!(backoff_seconds(21), 30 * (1i64 << 20));
    }
}
