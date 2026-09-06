//! Producer surface for the `queue_tasks` queue (event-queue-incremental-
//! linking task 1.2).
//!
//! [`DocumentJobQueue`] is the single producer surface for the persistent
//! event queue (task 1.1, [`db::QueueTaskDao`]): the file watcher (task 1.5),
//! the startup reconcile and the CLI enqueue through it instead of calling
//! the ingestion pipeline directly. The queue is a new Rust construct:
//! ingestion runs as per-file jobs rather than a synchronous whole-source
//! re-ingest, so the durable job state has a single producer surface instead
//! of an ad-hoc re-run.
//!
//! - [`DocumentJobQueue::enqueue_index`] / [`DocumentJobQueue::enqueue_delete`]
//!   are thin idempotent upserts over the DAO (one task per
//!   `(type, identity)`; a re-enqueue resets `attempts` to 0).
//! - [`DocumentJobQueue::reconcile_source`] walks one configured source with
//!   the shared [`walk_matched_files`] (the same `.synignore`-aware walk the
//!   parsers use), computes each matched file's content hash
//!   ([`compute_content_hash`], the pipeline's dedup key) and diffs the
//!   `(path, hash)` set against the `documents` table and the existing
//!   `queue_tasks`: new/changed files are enqueued as `doc:index`, files
//!   known to the DB or the queue but absent on disk are enqueued as
//!   `doc:delete`, unchanged files produce no task.
//!
//! The producer is a directory reader ONLY: it enumerates file paths and
//! hashes and writes `queue_tasks` rows. It never invokes the parser's
//! whole-tree `parse` (which builds `Document`s with full content), never
//! constructs a document and never runs the pipeline — the background
//! worker (task 1.4) is the only consumer, and it re-reads each file itself
//! (design `event-queue-incremental-linking`, Architecture + Correction).
//!
//! Design decisions for the reconcile (prune) behavior:
//!
//! - A source root that is missing or not a directory is an explicit error,
//!   never an "all files deleted" diff: a broken walk must not turn into a
//!   mass delete.
//! - "Absent on disk" is decided per candidate path with
//!   [`std::fs::symlink_metadata`], not solely by walk membership: a file in
//!   a subdirectory the walk could not read stays put.
//! - A stale `pending` delete task is cancelled when its file is back on
//!   disk with an unchanged hash (otherwise the worker would delete a live
//!   document).
//! - Tasks in `processing` are never touched: the worker owns them until it
//!   finishes (a crashed cycle is re-queued by the startup timeout reset,
//!   not by the producer).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use db::{
    ConnectionOrTx, Db, DocDeletePayload, DocIndexPayload, DocumentDao, QueueTaskDao, QueueTaskType,
};

use crate::error::IngestionError;
use crate::ingester::compute_content_hash;
use crate::parsers::walk_matched_files;
use crate::runner::{Runner, is_within};

/// The single producer surface for the `queue_tasks` queue (task 1.2).
///
/// Holds a reference to the knowledge database; every enqueue is an
/// idempotent upsert by `(type, identity)` through [`QueueTaskDao`] (one
/// task per type+identity, `INSERT OR REPLACE` semantics). The producer
/// never runs the ingestion pipeline — the background worker (task 1.4) is
/// the only consumer.
pub struct DocumentJobQueue<'db> {
    /// The knowledge database handle (shared with the runner and the worker).
    db: &'db Db,
}

/// Outcome of one [`DocumentJobQueue::reconcile_source`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileStats {
    /// Files enqueued for (re)indexing (new or changed content hash).
    pub indexed: usize,
    /// Files enqueued for deletion (known to the DB or the queue, absent on
    /// disk).
    pub deleted: usize,
    /// Files present on disk with an unchanged content hash (no task).
    pub unchanged: usize,
    /// Stale `pending` delete tasks cancelled (the file came back on disk
    /// with an unchanged hash before the worker ran).
    pub stale_deletes_cancelled: usize,
}

impl<'db> DocumentJobQueue<'db> {
    /// Wraps the shared knowledge-database handle.
    pub fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// Queue `path` for (re)indexing as a `pending` `doc:index` task due
    /// immediately.
    ///
    /// Idempotent: the row is upserted by `(doc:index, path)`, so a
    /// re-enqueue resets `attempts` to 0 (one task per identity).
    /// `content_hash` is the pipeline's SHA-256 content hash at enqueue time
    /// (the worker re-verifies it).
    pub fn enqueue_index(
        &self,
        path: &str,
        source_path: &str,
        content_hash: Option<&str>,
    ) -> Result<(), IngestionError> {
        let now = now_unix_seconds();
        let payload = DocIndexPayload {
            source_path: source_path.to_owned(),
            content_hash: content_hash.map(str::to_owned),
        };
        self.db.with_conn(|conn| {
            QueueTaskDao::new(ConnectionOrTx::Connection(conn)).enqueue(
                QueueTaskType::DocIndex,
                path,
                &payload,
                now,
            )
        })??;
        Ok(())
    }

    /// Queue `path` for deletion as a `pending` `doc:delete` task.
    ///
    /// Idempotent upsert by `(doc:delete, path)`: a pending `doc:delete` for
    /// the same path is refreshed (the file is gone — indexing it would fail
    /// anyway). A `doc:index` for the same path is a SEPARATE row and is not
    /// touched here.
    pub fn enqueue_delete(&self, path: &str) -> Result<(), IngestionError> {
        let now = now_unix_seconds();
        let payload = DocDeletePayload {
            source_path: String::new(),
        };
        self.db.with_conn(|conn| {
            QueueTaskDao::new(ConnectionOrTx::Connection(conn)).enqueue(
                QueueTaskType::DocDelete,
                path,
                &payload,
                now,
            )
        })??;
        Ok(())
    }

    /// Walks the configured source containing `source_path` and reconciles
    /// the queue with the disk state:
    ///
    /// - new/changed files (no `documents` row, or a different content
    ///   hash) → [`Self::enqueue_index`] with the fresh hash;
    /// - files known to the `documents` table (under this source root) or
    ///   the queue (this source's doc tasks) but absent on disk →
    ///   [`Self::enqueue_delete`];
    /// - unchanged files (same content hash as the `documents` row) → no
    ///   task, and a stale `pending` delete task for them is cancelled.
    ///
    /// The walk reuses the shared [`walk_matched_files`] (same
    /// `.synignore` semantics as the parsers) and the pipeline's
    /// [`compute_content_hash`]; the match predicate accepts the source's
    /// [`supported extensions`](crate::types::Parser::supported_extensions)
    /// case-insensitively, exactly like the per-format parsers. The
    /// producer never invokes the parser's `parse` and never builds a
    /// document — it only enumerates `(path, hash)` pairs.
    ///
    /// # Errors
    ///
    /// [`IngestionError::NoSourceForPath`] when no configured source
    /// contains `source_path`, [`IngestionError::UnknownSourceType`] when
    /// the source's type word has no registered implementation,
    /// [`IngestionError::Io`]/[`IngestionError::NotADirectory`] when the
    /// source root is missing or not a directory (a broken walk must not
    /// turn into a mass delete), or any [`IngestionError::Db`] /
    /// [`IngestionError::QueueTask`] failure.
    pub fn reconcile_source(
        &self,
        runner: &Runner<'_>,
        source_path: &str,
    ) -> Result<ReconcileStats, IngestionError> {
        let src = runner.find_source_for_path(source_path).ok_or_else(|| {
            IngestionError::NoSourceForPath {
                path: source_path.to_owned(),
            }
        })?;
        let source = runner
            .source_for_config(src)
            .ok_or_else(|| IngestionError::UnknownSourceType(src.path.clone()))?;

        // A missing or non-directory root is an explicit error: the walk
        // would enumerate zero files and the diff below would enqueue a
        // delete for every known file (module docs, deliberate deviation).
        let root = Path::new(&src.path);
        let meta = fs::symlink_metadata(root).map_err(|source_err| IngestionError::Io {
            path: root.to_path_buf(),
            source: source_err,
        })?;
        if !meta.is_dir() {
            return Err(IngestionError::NotADirectory {
                path: root.to_path_buf(),
            });
        }

        // The current disk state: one `(path, content hash)` entry per
        // matched file. The walk is best-effort (the shared contract): a
        // per-file read failure is collected in `errors` and the path
        // simply stays out of `on_disk` — the per-path stat in the delete
        // pass below then keeps an existing-but-unreadable file from being
        // deleted.
        let mut on_disk: BTreeMap<String, String> = BTreeMap::new();
        let mut errors: Vec<IngestionError> = Vec::new();
        walk_matched_files(
            root,
            |path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        source
                            .supported_extensions()
                            .iter()
                            .any(|wanted| ext.eq_ignore_ascii_case(&wanted[1..]))
                    })
            },
            |path| {
                let content =
                    fs::read_to_string(path).map_err(|source_err| IngestionError::Io {
                        path: path.to_path_buf(),
                        source: source_err,
                    })?;
                on_disk.insert(
                    path.to_string_lossy().into_owned(),
                    compute_content_hash(&content),
                );
                Ok(())
            },
            &mut errors,
        );

        // The known state: every document row and every doc:* task row
        // (both tables are small on a laptop).
        let documents = self
            .db
            .with_conn(|conn| {
                let exec = ConnectionOrTx::Connection(conn);
                DocumentDao::new(exec).list()
            })
            .map_err(IngestionError::Db)?
            .map_err(IngestionError::Db)?;
        let all_tasks = self
            .db
            .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
            .map_err(IngestionError::Db)?
            .map_err(IngestionError::QueueTask)?;
        let doc_tasks: Vec<db::QueueTask> = all_tasks
            .into_iter()
            .filter(|t| t.task_type == "doc:index" || t.task_type == "doc:delete")
            .collect();
        let documents_by_path: HashMap<&str, &db::Document> = documents
            .iter()
            .map(|doc| (doc.original_path.as_str(), doc))
            .collect();

        let mut stats = ReconcileStats::default();
        let mut deleted: BTreeSet<String> = BTreeSet::new();

        // New / changed / unchanged, per walked file.
        for (path, hash) in &on_disk {
            let unchanged = documents_by_path
                .get(path.as_str())
                .is_some_and(|existing| existing.content_hash.as_deref() == Some(hash.as_str()));
            if unchanged {
                stats.unchanged += 1;
                // A stale pending delete (the file was removed and came
                // back before the worker ran) is cancelled: otherwise the
                // worker would delete a live document (module docs).
                if doc_tasks.iter().any(|t| {
                    t.identity == *path && t.task_type == "doc:delete" && t.status == "pending"
                }) {
                    self.cancel_delete(path)?;
                    stats.stale_deletes_cancelled += 1;
                }
            } else {
                self.enqueue_index(path, &src.path, Some(hash))?;
                stats.indexed += 1;
            }
        }

        // Removed: known paths that are no longer on disk.
        for doc in &documents {
            if is_within(Path::new(&doc.original_path), root) {
                self.enqueue_delete_if_absent(
                    &doc.original_path,
                    &on_disk,
                    &mut deleted,
                    &mut stats,
                )?;
            }
        }
        for task in &doc_tasks {
            // This source's tasks only; `processing` rows are owned by the
            // worker until it finishes (module docs).
            if task.status != "processing" && is_within(Path::new(&task.identity), root) {
                self.enqueue_delete_if_absent(&task.identity, &on_disk, &mut deleted, &mut stats)?;
            }
        }

        Ok(stats)
    }

    /// Enqueues `doc:delete` for `path` when the per-path stat says the file
    /// is gone; deduplicates across the two candidate sets (a path may be
    /// both a document row and a task row).
    fn enqueue_delete_if_absent(
        &self,
        path: &str,
        on_disk: &BTreeMap<String, String>,
        deleted: &mut BTreeSet<String>,
        stats: &mut ReconcileStats,
    ) -> Result<(), IngestionError> {
        if !deleted.insert(path.to_owned()) || on_disk.contains_key(path) {
            return Ok(());
        }
        // Per-path stat: walk membership above already excluded matched
        // files, so a missing stat here means the file is genuinely gone
        // (module docs).
        if fs::symlink_metadata(Path::new(path)).is_err() {
            self.enqueue_delete(path)?;
            stats.deleted += 1;
        }
        Ok(())
    }

    /// Removes a stale `doc:delete` row (the stale-delete cancellation): the
    /// file is back on disk unchanged, so the queued delete is obsolete.
    fn cancel_delete(&self, path: &str) -> Result<(), IngestionError> {
        self.db
            .with_conn(|conn| {
                QueueTaskDao::new(ConnectionOrTx::Connection(conn))
                    .delete(QueueTaskType::DocDelete, path)
            })
            .map_err(IngestionError::Db)?
            .map_err(IngestionError::QueueTask)?;
        Ok(())
    }
}

/// The current Unix time in seconds.
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::path::Path;

    use config::DomainConfig;
    use config::ontology::{GlobalConfig, GlobalNerConfig, SourceConfig, SourceType};
    use config::preset::{IngestionConfig, LinkerConfig};
    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db, DocumentDao, QueueTaskDao};
    use embedding::{EmbeddingError, EmbeddingProvider};
    use vectors::{VectorIndex, VectorsError};

    use super::*;
    use crate::error::IngestionError;
    use crate::ingester::compute_content_hash;
    use crate::ner::load_ner_prompts;
    use crate::parsers::tests::TempTree;
    use crate::runner::RunnerParams;
    use crate::sources::Registry;
    use crate::types::{
        Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source,
    };

    /// A minimal `.txt` source for the producer tests. The producer never
    /// calls `parse` (the corrected task 1.3 walks and hashes directly), so
    /// the parser half is a stub — only `supported_extensions` drives the
    /// walk's match predicate.
    struct TestSource;

    impl Parser for TestSource {
        fn parse(&self, _source_path: &Path) -> ParseResult {
            // Unused by the producer: reconcile_source walks and hashes
            // without building documents (module docs).
            ParseResult::default()
        }

        fn parse_file(&self, _path: &Path, _root: &Path) -> Result<Document, IngestionError> {
            // Unused by the producer (module docs): the stub parses nothing.
            Err(IngestionError::UnsupportedExtension(".txt".to_owned()))
        }

        fn supported_extensions(&self) -> &[&str] {
            &[".txt"]
        }
    }

    impl Chunker for TestSource {
        fn chunk(
            &self,
            _content: &str,
            _metadata: &DocumentMetadata,
        ) -> Result<Vec<DocumentChunk>, IngestionError> {
            Ok(Vec::new())
        }
    }

    impl Source for TestSource {}

    /// A no-op embedding provider (reconcile never embeds).
    struct StubEmbedding;

    impl EmbeddingProvider for StubEmbedding {
        fn generate_embeddings(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(Vec::new())
        }

        fn vector_dim(&self) -> usize {
            4
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    /// A no-op vector index (reconcile never writes vectors).
    struct StubIndex;

    impl VectorIndex for StubIndex {
        fn insert(&self, _chunk_id: u32, _vector: &[f32]) -> Result<(), VectorsError> {
            Ok(())
        }

        fn insert_batch(&self, _rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
            Ok(())
        }

        fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            Ok(Vec::new())
        }

        fn delete_by_chunk_ids(&self, _chunk_ids: &[u32]) -> Result<(), VectorsError> {
            Ok(())
        }

        fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
            Ok(Vec::new())
        }

        fn count(&self) -> Result<u64, VectorsError> {
            Ok(0)
        }

        fn build_index(&self) -> Result<(), VectorsError> {
            Ok(())
        }

        fn rebuild(&self, _rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
            Ok(())
        }
    }

    /// The runner's collaborators (mirrors the runner test harness): an
    /// in-memory knowledge DB, one configured source per test, a `.txt`
    /// registry source and stub embed/index providers (reconcile never
    /// calls them — the runner only needs the handles).
    struct Harness {
        db: Db,
        cfg: IngestionConfig,
        global: GlobalConfig,
        domains: HashMap<String, DomainConfig>,
        registry: Registry,
        embed: StubEmbedding,
        index: StubIndex,
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
                domains: HashMap::new(),
                registry,
                embed: StubEmbedding,
                index: StubIndex,
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
                embed: &self.embed,
                vectors: &self.index,
                prompts: &self.prompts,
                linker_cfg: &self.linker,
                prompts_path: "/nonexistent-prompts",
                llm_cache: None,
            })
        }
    }

    /// All `doc:*` tasks (test helper).
    fn list_doc_tasks(db: &Db) -> Vec<db::QueueTask> {
        db.with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|t| t.task_type == "doc:index" || t.task_type == "doc:delete")
            .collect()
    }

    /// Seeds one `documents` row (test helper).
    fn seed_document(db: &Db, path: &str, content_hash: &str) {
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            DocumentDao::new(ConnectionOrTx::Transaction(&*tx)).create(
                "test",
                path,
                None,
                Some(content_hash),
            )?;
            Ok(())
        })
        .unwrap();
    }

    // enqueue_index twice → one row, attempts 0, latest hash wins.
    #[test]
    fn enqueue_index_twice_is_one_row_with_zero_attempts() {
        let db = in_memory_db();
        let queue = DocumentJobQueue::new(&db);

        queue
            .enqueue_index("/docs/a.md", "/docs", Some("h1"))
            .unwrap();
        queue
            .enqueue_index("/docs/a.md", "/docs", Some("h2"))
            .unwrap();

        let tasks = list_doc_tasks(&db);
        assert_eq!(tasks.len(), 1, "one task per (type, identity)");
        let task = &tasks[0];
        assert_eq!(task.task_type, "doc:index");
        assert_eq!(task.identity, "/docs/a.md");
        assert_eq!(task.status, "pending");
        assert_eq!(task.attempts, 0, "re-enqueue must reset attempts");
        let v: serde_json::Value = serde_json::from_str(&task.event).unwrap();
        assert_eq!(v["content_hash"], "h2", "latest hash wins");
    }

    // reconcile_source: index for new/changed, delete for removed, no task
    // for unchanged.
    #[test]
    fn reconcile_source_enqueues_the_disk_diff() {
        let tree = TempTree::new();
        tree.write("a.txt", "alpha\n"); // unchanged (same hash in the DB)
        tree.write("b.txt", "beta v2\n"); // changed (stale hash in the DB)
        tree.write("c.txt", "gamma\n"); // new (no DB row)
        tree.write("notes.md", "not a .txt file"); // ignored by the walk
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let queue = DocumentJobQueue::new(&harness.db);

        // Seed the documents table: a.txt current, b.txt stale, d.txt
        // indexed but absent on disk.
        seed_document(
            &harness.db,
            &format!("{root}/a.txt"),
            &compute_content_hash("alpha\n"),
        );
        seed_document(&harness.db, &format!("{root}/b.txt"), "sha256:stale");
        seed_document(&harness.db, &format!("{root}/d.txt"), "sha256:gone");
        // A pending index task for e.txt: queued, then the file was removed
        // before the worker ran.
        queue
            .enqueue_index(&format!("{root}/e.txt"), &root, Some("sha256:e"))
            .unwrap();

        let stats = queue.reconcile_source(&runner, &root).unwrap();
        assert_eq!(stats.indexed, 2, "b.txt (changed) + c.txt (new)");
        assert_eq!(stats.deleted, 2, "d.txt (DB only) + e.txt (task only)");
        assert_eq!(stats.unchanged, 1, "a.txt");
        assert_eq!(stats.stale_deletes_cancelled, 0);

        let tasks = list_doc_tasks(&harness.db);

        // a.txt: unchanged → no new task.
        assert!(
            !tasks.iter().any(|t| t.identity == format!("{root}/a.txt")),
            "unchanged files produce no task"
        );

        // b.txt: changed → pending doc:index with the fresh hash.
        let b = tasks
            .iter()
            .find(|t| t.identity == format!("{root}/b.txt"))
            .expect("b.txt must be queued");
        assert_eq!(b.task_type, "doc:index");
        assert_eq!(b.status, "pending");
        let v: serde_json::Value = serde_json::from_str(&b.event).unwrap();
        assert_eq!(v["content_hash"], compute_content_hash("beta v2\n"));

        // c.txt: new → pending doc:index.
        let c = tasks
            .iter()
            .find(|t| t.identity == format!("{root}/c.txt"))
            .expect("c.txt must be queued");
        assert_eq!(c.task_type, "doc:index");
        let v: serde_json::Value = serde_json::from_str(&c.event).unwrap();
        assert_eq!(v["content_hash"], compute_content_hash("gamma\n"));

        // d.txt: DB only → pending doc:delete.
        let d = tasks
            .iter()
            .find(|t| t.identity == format!("{root}/d.txt"))
            .expect("d.txt must be queued");
        assert_eq!(d.task_type, "doc:delete");
        assert_eq!(d.status, "pending");

        // e.txt: the file is absent → a doc:delete is enqueued alongside the
        // stale doc:index (which converges via the worker's process_document
        // path: file absent → remove doc → mark done).
        let e_delete = tasks
            .iter()
            .find(|t| t.identity == format!("{root}/e.txt") && t.task_type == "doc:delete")
            .expect("e.txt must have a doc:delete task");
        assert_eq!(e_delete.status, "pending");

        // Reconcile is a producer: the documents table is untouched.
        let doc_count: i64 = harness
            .db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            })
            .unwrap()
            .unwrap();
        assert_eq!(doc_count, 3, "reconcile must not write documents rows");
    }

    // A stale pending delete is cancelled when the file is back on disk
    // unchanged.
    #[test]
    fn reconcile_source_cancels_a_stale_pending_delete() {
        let tree = TempTree::new();
        tree.write("a.txt", "alpha\n");
        let root = tree.0.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let queue = DocumentJobQueue::new(&harness.db);

        seed_document(
            &harness.db,
            &format!("{root}/a.txt"),
            &compute_content_hash("alpha\n"),
        );
        // The file was removed (delete queued) and came back before the
        // worker ran.
        queue.enqueue_delete(&format!("{root}/a.txt")).unwrap();

        let stats = queue.reconcile_source(&runner, &root).unwrap();
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.deleted, 0);
        assert_eq!(stats.unchanged, 1);
        assert_eq!(stats.stale_deletes_cancelled, 1);

        assert!(
            list_doc_tasks(&harness.db).is_empty(),
            "the stale delete must be cancelled"
        );
    }

    // A path no configured source contains is an explicit error.
    #[test]
    fn reconcile_source_without_a_configured_source_is_an_error() {
        let harness = Harness::new();
        let runner = harness.runner();
        let queue = DocumentJobQueue::new(&harness.db);

        let err = queue.reconcile_source(&runner, "/nowhere").unwrap_err();
        assert!(
            matches!(err, IngestionError::NoSourceForPath { .. }),
            "got {err:?}"
        );
    }

    // A missing source root is an error, never an "all files deleted" diff.
    #[test]
    fn reconcile_source_missing_root_is_not_a_mass_delete() {
        let tree = TempTree::new();
        let missing = tree.0.join("missing");
        let root = missing.to_string_lossy().into_owned();

        let mut harness = Harness::new();
        harness.with_source(&root);
        let runner = harness.runner();
        let queue = DocumentJobQueue::new(&harness.db);

        // A document the source "used to have".
        seed_document(&harness.db, &format!("{root}/a.txt"), "sha256:x");

        let err = queue.reconcile_source(&runner, &root).unwrap_err();
        assert!(matches!(err, IngestionError::Io { .. }), "got {err:?}");
        assert!(
            list_doc_tasks(&harness.db).is_empty(),
            "a broken walk must not enqueue deletes"
        );
    }
}
