//! Background document-job worker (document-jobs-queue task 1.4).
//!
//! [`DocumentWorker`] is the sole consumer of the `document_jobs` queue:
//! it claims due rows, runs the per-document pipeline (index) or removal
//! (delete), records failures with exponential backoff, and sweeps
//! orphaned data after each batch.
//!
//! The worker runs on the serve owner thread (the [`Runner`] is `!Send +
//! !Sync` — it holds `&dyn` references and a `Mutex<()>` that cannot cross
//! thread boundaries). The owner-thread serve loop (document-jobs-queue
//! task 1.6) drives one blocking [`DocumentWorker::run_once`] call per poll
//! tick, interleaved with the shutdown signal via `tokio::select!`.
//!
//! Backoff schedule (design): `30 * 2^(attempts-1)` seconds, i.e. 30s /
//! 60s / 120s for attempts 1→2→3, after which the job flips to `error`
//! status (no further retries).

use db::{ConnectionOrTx, Db, DocumentJobDao};

use crate::error::IngestionError;
use crate::runner::Runner;

/// Maximum jobs claimed per poll cycle (no config knob in task 1.4).
const WORKER_BATCH_SIZE: i64 = 100;

/// Base backoff in seconds (30s * 2^attempts: 30, 60, 120, …).
const BASE_BACKOFF_SECS: i64 = 30;

/// The background consumer of the `document_jobs` queue (task 1.4).
///
/// Holds references to the database and the runner; every poll cycle is one
/// [`run_once`](Self::run_once) call. The worker is `!Send` (the [`Runner`]
/// is `!Send`), so it must run on the owner thread.
pub struct DocumentWorker<'a> {
    /// The knowledge database handle (shared with the runner and the queue).
    db: &'a Db,
    /// The ingestion runner (the per-document pipeline executor).
    runner: &'a Runner<'a>,
    /// Failure cap from `IngestionConfig::max_retries` (default 3).
    max_attempts: i32,
}

impl<'a> DocumentWorker<'a> {
    /// Wraps the shared database handle and runner reference.
    ///
    /// `max_attempts` is the failure cap from `IngestionConfig::max_retries`.
    pub fn new(db: &'a Db, runner: &'a Runner<'a>, max_attempts: i32) -> Self {
        Self {
            db,
            runner,
            max_attempts,
        }
    }

    /// One poll cycle: claim due jobs, process each one, then sweep
    /// orphaned data.
    ///
    /// `now` is the current Unix time in seconds (injected for testability).
    ///
    /// # Errors
    ///
    /// Propagates the first unhandled error from the claim stage. Per-job
    /// failures are recorded in the queue (backoff / error status) and never
    /// abort the cycle. A failed orphan cleanup is logged and does not
    /// propagate.
    pub fn run_once(&self, now: i64) -> Result<(), IngestionError> {
        let jobs = self.db.with_conn(|conn| {
            DocumentJobDao::new(ConnectionOrTx::Connection(conn)).claim_due(now, WORKER_BATCH_SIZE)
        })??;

        for job in &jobs {
            self.process_job(job, now);
        }

        // GC phase: sweep orphaned data after draining the batch.
        if let Err(err) = self.runner.cleanup_orphaned_data() {
            eprintln!("worker: orphan cleanup failed: {err}");
        }

        Ok(())
    }

    /// Processes one claimed job: runs the pipeline (index) or removal
    /// (delete), then records the outcome in the queue.
    fn process_job(&self, job: &db::DocumentJob, now: i64) {
        let result = match job.op.as_str() {
            "index" => self.runner.process_document_by_path(&job.path),
            "delete" => self.runner.delete_document_at(&job.path).map(|_| ()),
            other => {
                eprintln!("worker: unknown op {other:?} for {}", job.path);
                return;
            }
        };

        match result {
            Ok(()) => self.finish_success(job),
            Err(err) => self.finish_failure(job, &err.to_string(), now),
        }
    }

    /// Records a successful job: `mark_done` for index, `mark_deleted_row`
    /// for delete.
    fn finish_success(&self, job: &db::DocumentJob) {
        let result = self.db.with_conn(|conn| {
            let dao = DocumentJobDao::new(ConnectionOrTx::Connection(conn));
            if job.op == "delete" {
                dao.mark_deleted_row(&job.path)
            } else {
                dao.mark_done(&job.path)
            }
        });
        if let Err(err) = result.and_then(|inner| inner.map(|_| ())) {
            eprintln!("worker: failed to record success for {}: {err}", job.path);
        }
    }

    /// Records a failed job with exponential backoff
    /// (`30 * 2^(attempts-1)` seconds, `attempts` = the post-failure count).
    /// At the cap, the job flips to `error` status.
    fn finish_failure(&self, job: &db::DocumentJob, err: &str, now: i64) {
        let backoff = backoff_seconds(job.attempts);
        let result = self.db.with_conn(|conn| {
            DocumentJobDao::new(ConnectionOrTx::Connection(conn)).record_failure(
                &job.path,
                err,
                now,
                backoff,
                self.max_attempts,
            )
        });
        match result {
            Ok(Ok(true)) => {
                if job.attempts + 1 >= self.max_attempts {
                    eprintln!(
                        "worker: {} failed {} times, marking as error: {err}",
                        job.path,
                        job.attempts + 1
                    );
                } else {
                    eprintln!(
                        "worker: {} attempt {} failed (next in {backoff}s): {err}",
                        job.path,
                        job.attempts + 1
                    );
                }
            }
            Ok(Ok(false)) => {
                eprintln!("worker: no job row for {} (already removed?)", job.path);
            }
            Ok(Err(db_err)) => {
                eprintln!(
                    "worker: failed to record failure for {}: {db_err}",
                    job.path
                );
            }
            Err(db_err) => {
                eprintln!(
                    "worker: failed to record failure for {}: {db_err}",
                    job.path
                );
            }
        }
    }
}

/// Exponential backoff: `30 * 2^(attempts-1)` seconds (capped at 2^20 to
/// avoid overflow), where `attempts` is the post-failure count.
///
/// The argument is the job's `attempts` field BEFORE this failure is
/// recorded (the 0-indexed previous-failure count), so the exponent is
/// `attempts`:
/// - 0 previous failures → 30s (after attempt 1)
/// - 1 previous failure → 60s (after attempt 2)
/// - 2 previous failures → 120s (after attempt 3)
fn backoff_seconds(attempts: i32) -> i64 {
    let exponent = (attempts as u32).min(20);
    BASE_BACKOFF_SECS * (1i64 << exponent)
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
    use db::{ConnectionOrTx, Db, DocumentDao, DocumentJobDao, EntityDao};
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
                        doc_id: None,
                        text: line.to_owned(),
                        sequence_num: seq,
                        start_offset: start,
                        end_offset: end,
                        metadata: metadata.clone(),
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

        /// All `document_jobs` rows (test helper).
        fn list_jobs(&self) -> Vec<db::DocumentJob> {
            self.db
                .with_conn(|conn| {
                    DocumentJobDao::new(ConnectionOrTx::Connection(conn)).list(None, None)
                })
                .unwrap()
                .unwrap()
        }

        /// All `documents` rows (test helper).
        fn list_documents(&self) -> Vec<db::Document> {
            self.db
                .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
                .unwrap()
                .unwrap()
        }
    }

    // A successful index job creates the document and marks the job done.
    #[test]
    fn run_once_processes_a_successful_index_job() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello world\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // Enqueue an index job for the file.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &format!("{root}/a.txt"),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        // One poll cycle: the job is claimed, processed, and marked done.
        worker.run_once(1_000).unwrap();

        // The document was created.
        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the index job must create the document");
        assert_eq!(docs[0].original_path, format!("{root}/a.txt"), "{docs:?}");

        // The job is marked done.
        let jobs = harness.list_jobs();
        assert_eq!(jobs.len(), 1, "the job row must still exist");
        assert_eq!(jobs[0].status, "done", "{jobs:?}");
    }

    // A delete job removes the document and the job row.
    #[test]
    fn run_once_processes_a_delete_job() {
        let tree = TempTree::new();
        tree.write("a.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // First, index the document.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &format!("{root}/a.txt"),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        worker.run_once(1_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);

        // Now, enqueue a delete job.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn))
                    .enqueue_delete(&format!("{root}/a.txt"))
            })
            .unwrap()
            .unwrap();

        // One poll cycle: the delete is processed and the job row is removed.
        worker.run_once(2_000).unwrap();

        // The document was removed.
        assert_eq!(
            harness.list_documents().len(),
            0,
            "the delete job must remove the document"
        );
        // The job row was removed (mark_deleted_row).
        assert!(
            harness.list_jobs().is_empty(),
            "the delete job row must be gone"
        );
    }

    // A failing index job gets backoff and eventually error status.
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
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // Enqueue an index job.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &format!("{root}/a.txt"),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        // Attempt 1: fails, backoff 30s.
        worker.run_once(1_000).unwrap();
        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.attempts, 1, "first failure recorded");
        assert_eq!(job.status, "pending", "below cap: back to pending");
        assert_eq!(job.next_attempt_at, 1_030, "30s backoff");

        // Attempt 2: advance the clock past the backoff, fail again, backoff 60s.
        worker.run_once(1_100).unwrap();
        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.attempts, 2, "second failure recorded");
        assert_eq!(job.status, "pending", "below cap: back to pending");
        assert_eq!(job.next_attempt_at, 1_160, "60s backoff from now=1100");

        // Attempt 3: advance the clock, fail again → error status.
        worker.run_once(1_200).unwrap();
        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.attempts, 3, "third failure recorded");
        assert_eq!(job.status, "error", "at cap: error status");
        assert!(job.last_error.is_some(), "last_error must be set");

        // An error job is not claimable anymore.
        worker.run_once(10_000).unwrap();
        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.status, "error", "error jobs are not re-claimed");
    }

    // End-to-end (task 1.8): a failing document job is retried to the cap
    // (`error`), `reset_retries` re-queues it, and once the failure is
    // cleared the worker processes the document to `done`. The failure is
    // injected at the embedding stage (the `MockEmbedding` fail marker) and
    // cleared by setting it back to `None` — the same interior-mutable flip
    // the retry test above uses.
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
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // Enqueue the failing index job.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn))
                    .enqueue_index(&path, &root, None)
            })
            .unwrap()
            .unwrap();

        // Retry up to the cap: three failures drive the job to `error`.
        worker.run_once(1_000).unwrap();
        worker.run_once(1_100).unwrap();
        worker.run_once(1_200).unwrap();

        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.status, "error", "at cap: error status, got {job:?}");
        assert_eq!(job.attempts, 3, "all three attempts recorded, got {job:?}");
        assert_eq!(
            job.max_attempts, 3,
            "the cap is the configured max, got {job:?}"
        );
        assert!(
            job.last_error.is_some(),
            "last_error must be set, got {job:?}"
        );

        // `reset_retries` (the CLI `queue reset-retries` wraps this) re-queues
        // the error job as pending with attempts 0.
        let reset = harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).reset_retries(&path)
            })
            .unwrap()
            .unwrap();
        assert!(reset, "the error job must be reset");

        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.status, "pending", "reset: back to pending, got {job:?}");
        assert_eq!(job.attempts, 0, "reset: attempts cleared, got {job:?}");

        // Clear the failure and run one more cycle: the document is indexed
        // and the job is marked done. `reset_retries` set `next_attempt_at`
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

        let job = harness.list_jobs()[0].clone();
        assert_eq!(job.status, "done", "re-processed to done, got {job:?}");

        let docs = harness.list_documents();
        assert_eq!(docs.len(), 1, "the document must exist, got {docs:?}");
        assert_eq!(docs[0].original_path, path, "{docs:?}");
    }

    // The converge path: a file that was deleted since enqueue → document
    // row removed, job marked done.
    #[test]
    fn run_once_converges_on_deleted_file() {
        let tree = TempTree::new();
        let file = tree.write("a.txt", "hello\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // Index the document first.
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &file.to_string_lossy(),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        worker.run_once(1_000).unwrap();
        assert_eq!(harness.list_documents().len(), 1);

        // Delete the file from disk.
        fs::remove_file(&file).unwrap();

        // Enqueue a new index job (simulating a re-enqueue after the file
        // was deleted).
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &file.to_string_lossy(),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        // The worker converges: removes the document, marks the job done.
        worker.run_once(2_000).unwrap();
        assert_eq!(
            harness.list_documents().len(),
            0,
            "the document must be removed (converge)"
        );
        let jobs = harness.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "done", "the converge path marks done");
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
        let worker = DocumentWorker::new(&harness.db, &runner, harness.cfg.max_retries);

        // Enqueue + process a successful index job (one full cycle).
        harness
            .db
            .with_conn(|conn| {
                DocumentJobDao::new(ConnectionOrTx::Connection(conn)).enqueue_index(
                    &format!("{root}/a.txt"),
                    &root,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        worker.run_once(1_000).unwrap();
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
        worker.run_once(2_000).unwrap();

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
