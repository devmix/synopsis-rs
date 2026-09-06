//! Background event-queue worker (event-queue-incremental-linking task 1.2).
//!
//! [`DocumentWorker`] is the sole consumer of the `queue_tasks` queue: it
//! claims due rows, dispatches by [`QueueTaskType`] (index / delete /
//! entity-link), records failures with exponential backoff, and sweeps
//! orphaned data after each batch.
//!
//! The worker runs on the serve owner thread (the [`Runner`] is `!Send +
//! !Sync` — it holds `&dyn` references and a `Mutex<()>` that cannot cross
//! thread boundaries). The owner-thread serve loop drives one blocking
//! [`DocumentWorker::run_once`] call per poll tick, interleaved with the
//! shutdown signal via `tokio::select!`.
//!
//! Backoff schedule (design): `30 * 2^(attempts-1)` seconds, i.e. 30s /
//! 60s / 120s for attempts 1→2→3, after which the task flips to `error`
//! status (no further retries). The `max_attempts` column on the row is the
//! cap (set at enqueue time; the DAO's `mark_failed` enforces it).

use db::{ConnectionOrTx, Db, QueueTask, QueueTaskDao, QueueTaskType};

use crate::error::IngestionError;
use crate::runner::Runner;

/// Maximum tasks claimed per poll cycle (no config knob in task 1.4).
const WORKER_BATCH_SIZE: i64 = 100;

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
}

impl<'a> DocumentWorker<'a> {
    /// Wraps the shared database handle and runner reference.
    pub fn new(db: &'a Db, runner: &'a Runner<'a>) -> Self {
        Self { db, runner }
    }

    /// One poll cycle: claim due tasks, process each one, then sweep
    /// orphaned data.
    ///
    /// `now` is the current Unix time in seconds (injected for testability).
    ///
    /// # Errors
    ///
    /// Propagates the first unhandled error from the claim stage. Per-task
    /// failures are recorded in the queue (backoff / error status) and never
    /// abort the cycle. A failed orphan cleanup is logged and does not
    /// propagate.
    pub fn run_once(&self, now: i64) -> Result<(), IngestionError> {
        let tasks = self
            .db
            .with_conn(|conn| {
                QueueTaskDao::new(ConnectionOrTx::Connection(conn))
                    .claim_due(now, WORKER_BATCH_SIZE)
            })
            .map_err(IngestionError::Db)?
            .map_err(IngestionError::QueueTask)?;

        for task in &tasks {
            self.process_task(task, now);
        }

        // GC phase: sweep orphaned data after draining the batch.
        if let Err(err) = self.runner.cleanup_orphaned_data() {
            tracing::warn!(error = %err, "orphan cleanup failed");
        }

        // Per-cycle progress: log only when work happened (idle cycles stay
        // quiet).
        if !tasks.is_empty() {
            self.log_cycle_summary(tasks.len());
        }

        Ok(())
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
            "doc:index" => self.runner.process_document_by_path(&task.identity),
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
        ConnectionOrTx, Db, DocIndexPayload, DocumentDao, EntityDao, QueueTaskDao, QueueTaskType,
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
    /// substring (per-document failure path).
    struct MockEmbedding {
        dim: usize,
        fail_marker: Mutex<Option<String>>,
    }

    impl MockEmbedding {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                fail_marker: Mutex::new(None),
            }
        }
    }

    impl EmbeddingProvider for MockEmbedding {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            if let Some(marker) = self.fail_marker.lock().unwrap().as_ref()
                && texts.iter().any(|text| text.contains(marker))
            {
                return Err(EmbeddingError::Ort("simulated engine failure".to_owned()));
            }
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

    /// In-memory [`VectorIndex`] stub: records every row.
    struct MemoryIndex {
        rows: Mutex<Vec<(u32, Vec<f32>)>>,
    }

    impl MemoryIndex {
        fn new() -> Self {
            Self {
                rows: Mutex::new(Vec::new()),
            }
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

        /// Enqueues a `doc:index` task for `path`.
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
