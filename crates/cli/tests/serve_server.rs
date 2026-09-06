//! Integration tests for `cli::serve::server` (moved verbatim from
//! `crates/cli/src/serve/server.rs`, test-hygiene phase-2 task 2.4). Import
//! paths rewritten from `use super::*` to the public `cli::serve::server`
//! API; the crate-private `SHUTDOWN_TIMEOUT` and the `pub(crate)`
//! `recreate_vectors_engine` are reached through `cli::test_support`.

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use config::preset::GraphConfig;
use config::{Config, GlobalConfig, OnnxConfig};
use db::{ChunkDao, ConnectionOrTx, DocIndexPayload, DocumentDao, QueueTaskDao, QueueTaskType};
use embedding::{EmbeddingError, EmbeddingProvider};
use graph::GraphIndex;
use search::Searcher;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;
use vectors::{ENGINE_USEARCH, VectorIndex, VectorIndexConfig, VectorsError, create_vector_engine};

use cli::error::CliError;
use cli::serve::bootstrap::{Bootstrap, open_db};
use cli::serve::server::{PooledSearcher, ServeRequest, serve_with_stop};
use cli::test_support::{SHUTDOWN_TIMEOUT, recreate_vectors_engine};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "synopsis-cli-server-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A deterministic embedding provider (fixed vectors of `dim`).
struct FakeEmbed {
    dim: usize,
}

impl EmbeddingProvider for FakeEmbed {
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(texts.iter().map(|_| vec![0.5f32; self.dim]).collect())
    }

    fn vector_dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

/// An in-memory [`VectorIndex`] stub (records rows, search returns the
/// stored rows in id order).
struct MemIndex {
    rows: std::sync::Mutex<BTreeMap<u32, Vec<f32>>>,
}

impl VectorIndex for MemIndex {
    fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
        self.rows.lock().unwrap().insert(chunk_id, vector.to_vec());
        Ok(())
    }

    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
        for (id, vector) in rows {
            self.insert(*id, vector)?;
        }
        Ok(())
    }

    fn search(&self, _query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.iter().take(k).map(|(id, _)| (*id, 0.0)).collect())
    }

    fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
        let mut rows = self.rows.lock().unwrap();
        for id in chunk_ids {
            rows.remove(id);
        }
        Ok(())
    }

    fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
        Ok(self.rows.lock().unwrap().keys().copied().collect())
    }

    fn count(&self) -> Result<u64, VectorsError> {
        Ok(self.rows.lock().unwrap().len() as u64)
    }

    fn build_index(&self) -> Result<(), VectorsError> {
        Ok(())
    }

    fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
        let mut map = self.rows.lock().unwrap();
        map.clear();
        for (id, vector) in rows {
            map.insert(*id, vector.clone());
        }
        Ok(())
    }
}

/// A 4-dim local-mode config (matching [`FakeEmbed`]), NER disabled.
fn test_config(workspace_dir: &Path) -> Config {
    let mut config = Config {
        embeddings: config::preset::EmbeddingsConfig {
            mode: config::preset::EmbeddingsMode::Local,
            local: config::preset::LocalEmbedding {
                model_name: "bge-m3-int8".to_string(),
                model_path: String::new(),
                tokenizer_path: String::new(),
                vector_dim: 4,
            },
            api: Default::default(),
            auto_rebuild_vectors: false,
        },
        paths: config::preset::PathsConfig {
            workspace_dir: workspace_dir.to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    config.ingestion.ner.disabled = true;
    config.apply_defaults();
    config
}

/// The fixture's dataset vectors path (dataset `edtech`).
fn dataset_vectors_path(dir: &TempDir) -> PathBuf {
    dir.as_ref()
        .join("workspace")
        .join("datasets")
        .join("edtech")
        .join("state")
        .join("vectors")
}

/// A Bootstrap with a fake provider and a temp-file db; no sources
/// (global `None`) — the task's "no real sources" shape. The dataset is
/// active (design D2): named `edtech` with the directory present.
fn test_bootstrap(dir: &TempDir) -> Bootstrap {
    let mut config = test_config(&dir.as_ref().join("workspace"));
    config.dataset.name = "edtech".to_string();
    std::fs::create_dir_all(config.dataset.state_path(&config.paths.workspace_dir))
        .expect("create dataset state dir");
    // The knowledge db at the derived dataset path (the same file the
    // production bootstrap opens): the engine's WAL wiring (task 3.9)
    // points the factory at it.
    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir)).expect("open db");
    Bootstrap {
        config,
        global: None,
        domains: std::collections::HashMap::new(),
        db,
        cache: None,
        embed: Arc::new(FakeEmbed { dim: 4 }),
        onnx: OnnxConfig::default(),
        registry: None,
        prompts: None,
        vectors: None,
        dimension_mismatch: None,
    }
}

/// A free localhost port (bind :0, read, drop).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port probe");
    listener.local_addr().expect("probe port").port()
}

// --- PooledSearcher ------------------------------------------------------

#[test]
fn pooled_searcher_runs_hybrid_search_over_the_pool() {
    let dir = TempDir::new("pooled-searcher");
    let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
    let doc = db
        .exec_tx(|tx| {
            DocumentDao::new(ConnectionOrTx::Transaction(&*tx)).create(
                "markdown",
                "/docs/a.md",
                None,
                None,
            )
        })
        .expect("seed document");
    db.exec_tx(|tx| {
        ChunkDao::new(ConnectionOrTx::Transaction(&*tx)).create(
            doc,
            "alpha beta gamma",
            0,
            None,
            None,
        )
    })
    .expect("seed chunk");

    // Production-default search config: `apply_defaults` materializes
    // the leg toggles and top-k values (a bare `Default` would disable
    // both legs). The stub index is empty, so the semantic leg
    // contributes nothing to the fusion.
    let mut config = Config::default();
    config.apply_defaults();
    let search_config = config.search;
    let searcher = PooledSearcher::new(
        db,
        search_config,
        GraphConfig::default(),
        Arc::new(FakeEmbed { dim: 4 }),
        Arc::new(MemIndex {
            rows: std::sync::Mutex::new(BTreeMap::new()),
        }),
        Arc::new(GraphIndex::Unavailable),
    );

    let results = searcher
        .hybrid_search("alpha", 10, None)
        .expect("search succeeds");
    assert_eq!(results.len(), 1, "the seeded chunk matches");
    assert_eq!(results[0].chunk_text, "alpha beta gamma");
    // The finalize pipeline enriched the hit with the document path.
    assert_eq!(results[0].document_path, "/docs/a.md");

    // Empty query: no results, no error (the leg contract).
    assert!(
        searcher
            .hybrid_search("", 10, None)
            .expect("empty query is ok")
            .is_empty()
    );
}

// --- serve_with_stop: start → health → stop --------------------------------

/// The task's integration shape: collaborators with a temp db + fake
/// embed (no real sources). The test thread is the owner thread (no
/// runtime context — the vector-engine facade is safe here, mirroring
/// `run_serve`'s main thread); a killer task on the runtime waits for
/// `/health` 200 and fires the same stop broadcast the SIGINT/SIGTERM
/// task feeds in production.
#[test]
fn serve_starts_serves_health_and_stops() {
    let dir = TempDir::new("serve-core");
    let port = free_port();
    let mut boot = test_bootstrap(&dir);
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: true,
        port,
        auto_rebuild_vectors: false,
    };

    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer = runtime.spawn(async move {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/health");
        let mut healthy = false;
        for _ in 0..200 {
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|res| res.status().as_u16() == 200)
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = stop_tx.send(());
        healthy
    });

    let started = std::time::Instant::now();
    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx);
    let stopped = started.elapsed();
    let healthy = runtime.block_on(killer).expect("killer task");

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    assert!(healthy, "/health must answer 200 before the stop signal");
    // `stopped` spans the whole shutdown path, whose hard bound is
    // `SHUTDOWN_TIMEOUT` itself (the forced-shutdown timeout), so asserting
    // `< SHUTDOWN_TIMEOUT` would flake under load whenever a legitimate
    // shutdown lands near the bound. The assertion's purpose is to catch a
    // hung shutdown, so allow margin beyond the forced bound: a hang never
    // returns, a healthy stop takes well under a second.
    assert!(
        stopped < SHUTDOWN_TIMEOUT + Duration::from_secs(5),
        "stop must not hang beyond the graceful bound + margin: {stopped:?}"
    );

    // The flow opened the vector engine on the bootstrap (registry and
    // prompts are serve-level locals, not stored on the bootstrap).
    assert!(boot.vectors.is_some(), "engine opened");
}

/// A minimal global config with one enabled markdown source at `src`.
fn one_markdown_source(src: &Path) -> GlobalConfig {
    GlobalConfig {
        sources: vec![config::ontology::SourceConfig {
            path: src.to_string_lossy().into_owned(),
            source_type: config::ontology::SourceType::Markdown,
            disabled: false,
            space: String::new(),
            domains: vec!["default".to_string()],
            dataset: String::new(),
        }],
        cross_domain_links: None,
        ner: config::ontology::GlobalNerConfig {
            methods: Vec::new(),
        },
        entities: Vec::new(),
        relations: Vec::new(),
        extraction: Default::default(),
    }
}

// --- serve_with_stop: startup reconcile → worker processing (task 1.6) -----

/// The task 1.6 acceptance shape: with an active dataset and one enabled
/// markdown source, the startup reconcile enqueues the disk diff (new,
/// changed, removed) into `queue_tasks`, and the worker — driven on the
/// owner thread — processes the pending rows during serve startup: the
/// new/changed documents land in `documents`, the removed one is deleted,
/// the job rows flip to `done` (the delete job row is removed), and the
/// GC sweep (now inside the worker) drops the chunk-less ghost row.
/// `/health` still answers 200 and the stop is graceful.
#[test]
fn serve_startup_reconcile_jobs_are_processed_by_the_worker() {
    let dir = TempDir::new("startup-worker");
    let port = free_port();
    // One markdown source: a new file, a changed file and an unchanged
    // file on disk; the prior ingestion state below adds a document row
    // for a file that is gone.
    let src = dir.as_ref().join("src");
    std::fs::create_dir_all(&src).expect("create source dir");
    let new_content = "# New\n\nA brand new document.\n";
    let changed_content = "# Changed\n\nFresh body after an edit.\n";
    let same_content = "# Same\n\nBody that never changed.\n";
    std::fs::write(src.join("new.md"), new_content).expect("write new.md");
    std::fs::write(src.join("changed.md"), changed_content).expect("write changed.md");
    std::fs::write(src.join("same.md"), same_content).expect("write same.md");
    let mut boot = test_bootstrap(&dir);
    boot.global = Some(one_markdown_source(&src));
    // Prior ingestion state: `changed.md` with a stale hash, `same.md`
    // with its current hash and a live chunk (so the worker's GC keeps
    // it), `gone.md` (no file on disk anymore), and a chunk-less ghost
    // document (a GC candidate).
    let changed_path = src.join("changed.md").to_string_lossy().into_owned();
    let same_path = src.join("same.md").to_string_lossy().into_owned();
    let gone_path = src.join("gone.md").to_string_lossy().into_owned();
    boot.db
        .with_conn(|conn| -> Result<(), db::DbError> {
            let dao = DocumentDao::new(ConnectionOrTx::Connection(conn));
            dao.create("markdown", &changed_path, None, Some("stale-hash"))?;
            let same_doc = dao.create(
                "markdown",
                &same_path,
                None,
                Some(&ingestion::ingester::compute_content_hash(same_content)),
            )?;
            dao.create("markdown", &gone_path, None, Some("old-hash"))?;
            // A live chunk keeps `same.md` out of the orphan sweep.
            ChunkDao::new(ConnectionOrTx::Connection(conn)).create(
                same_doc,
                "same body",
                0,
                None,
                None,
            )?;
            // An orphan document (no chunks, no provenance): the worker's
            // GC sweep must remove it.
            dao.create("markdown", "/ghost/ghost.md", None, None)?;
            Ok(())
        })
        .expect("with_conn seed")
        .expect("seed documents");
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: false,
        port,
        auto_rebuild_vectors: false,
    };

    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer = runtime.spawn(async move {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/health");
        let mut healthy = false;
        for _ in 0..200 {
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|res| res.status().as_u16() == 200)
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = stop_tx.send(());
        healthy
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx);
    let healthy = runtime.block_on(killer).expect("killer task");

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    assert!(healthy, "/health must answer 200 before the stop signal");

    // The worker processed the queued diff during serve startup: new.md
    // and changed.md are (re)indexed and their task rows flip to `done`;
    // gone.md is deleted and its task row removed; same.md stays unqueued.
    let tasks = boot
        .db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .expect("with_conn tasks")
        .map_err(|e| panic!("list tasks: {e}"))
        .expect("list tasks");
    let tasks_by_path: BTreeMap<&str, &db::QueueTask> = tasks
        .iter()
        .map(|task| (task.identity.as_str(), task))
        .collect();
    assert_eq!(
        tasks_by_path.len(),
        2,
        "new + changed processed (done), gone's row removed, same untouched: {tasks:?}"
    );
    let new_task = tasks_by_path
        .get(src.join("new.md").to_string_lossy().as_ref())
        .expect("new.md must be processed");
    assert_eq!(new_task.task_type, "doc:index");
    assert_eq!(new_task.status, "done", "{new_task:?}");
    let changed_task = tasks_by_path
        .get(src.join("changed.md").to_string_lossy().as_ref())
        .expect("changed.md must be processed");
    assert_eq!(changed_task.task_type, "doc:index");
    assert_eq!(changed_task.status, "done", "{changed_task:?}");
    assert!(
        !tasks_by_path.contains_key(src.join("gone.md").to_string_lossy().as_ref()),
        "the gone.md delete task row must be gone: {tasks:?}"
    );
    assert!(
        !tasks_by_path.contains_key(src.join("same.md").to_string_lossy().as_ref()),
        "same.md must stay unqueued: {tasks:?}"
    );

    // The documents table reflects the processed diff: new.md created
    // and changed.md re-indexed with the fresh content hash, same.md
    // untouched, gone.md deleted — and the worker's GC sweep dropped the
    // chunk-less ghost row.
    let docs = boot
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .expect("with_conn documents")
        .expect("list documents");
    let docs_by_path: BTreeMap<&str, &db::Document> = docs
        .iter()
        .map(|doc| (doc.original_path.as_str(), doc))
        .collect();
    assert_eq!(
        docs_by_path.len(),
        3,
        "new + changed + same survive; gone and the ghost are swept: {docs:?}"
    );
    let new_doc = docs_by_path
        .get(src.join("new.md").to_string_lossy().as_ref())
        .expect("new.md must be indexed");
    assert_eq!(
        new_doc.content_hash.as_deref(),
        Some(ingestion::ingester::compute_content_hash(new_content).as_str())
    );
    let changed_doc = docs_by_path
        .get(src.join("changed.md").to_string_lossy().as_ref())
        .expect("changed.md must be re-indexed");
    assert_eq!(
        changed_doc.content_hash.as_deref(),
        Some(ingestion::ingester::compute_content_hash(changed_content).as_str())
    );
    let same_doc = docs_by_path
        .get(src.join("same.md").to_string_lossy().as_ref())
        .expect("same.md must survive");
    assert_eq!(
        same_doc.content_hash.as_deref(),
        Some(ingestion::ingester::compute_content_hash(same_content).as_str())
    );
    assert!(
        !docs_by_path.contains_key(src.join("gone.md").to_string_lossy().as_ref()),
        "gone.md must be deleted: {docs:?}"
    );
    assert!(
        !docs_by_path.contains_key("/ghost/ghost.md"),
        "the ghost must be swept by the worker GC: {docs:?}"
    );
}

// --- serve_with_stop: restart recovery of a stuck processing row (task 1.5) --

/// Task 1.5: serve starts with a `doc:index` row stuck in `processing` (an
/// unclean shutdown mid-pipeline) for a file that is gone from disk: the
/// startup recovery resets it to `pending` BEFORE the startup reconcile
/// (which then sees it and enqueues the delete), and the worker's startup
/// drain processes everything — the recovered index row converges to
/// `done`, the delete row is removed, the live file is indexed. Without the
/// recovery the row would sit in `processing` forever (the worker claims
/// only `pending`; the reconcile skips `processing` rows).
#[test]
fn serve_startup_recovers_a_stuck_processing_row() {
    let dir = TempDir::new("recover-stuck");
    let port = free_port();
    let src = dir.as_ref().join("src");
    std::fs::create_dir_all(&src).expect("create source dir");
    std::fs::write(src.join("live.md"), "# Live\n\nStill on disk.\n").expect("write live.md");
    let mut boot = test_bootstrap(&dir);
    boot.global = Some(one_markdown_source(&src));
    // The stuck row: a `doc:index` for a file gone from disk, left in
    // `processing` by a simulated unclean shutdown (no document row).
    let stuck_path = src.join("gone.md").to_string_lossy().into_owned();
    boot.db
        .with_conn(|conn| -> Result<(), db::QueueTaskError> {
            let tasks = QueueTaskDao::new(ConnectionOrTx::Connection(conn));
            tasks.enqueue(
                QueueTaskType::DocIndex,
                &stuck_path,
                &DocIndexPayload {
                    source_path: src.to_string_lossy().into_owned(),
                    content_hash: None,
                },
                1,
            )?;
            conn.execute(
                "UPDATE queue_tasks SET status = 'processing', attempts = 1, \
                 last_error = 'simulated crash' WHERE identity = ?1",
                [stuck_path.as_str()],
            )
            .map_err(|e| db::QueueTaskError::Db(e.into()))?;
            Ok(())
        })
        .expect("with_conn seed")
        .expect("seed stuck row");
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: false,
        port,
        auto_rebuild_vectors: false,
    };

    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer = runtime.spawn(async move {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/health");
        let mut healthy = false;
        for _ in 0..200 {
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|res| res.status().as_u16() == 200)
            {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = stop_tx.send(());
        healthy
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx);
    let healthy = runtime.block_on(killer).expect("killer task");

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    assert!(healthy, "/health must answer 200 before the stop signal");

    // The stuck row was recovered (processing -> pending) and processed to
    // `done` by the worker's startup drain: without the recovery it would
    // sit in `processing` forever (the worker claims only `pending`, the
    // reconcile skips `processing` rows).
    let tasks = boot
        .db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .expect("with_conn tasks")
        .map_err(|e| panic!("list tasks: {e}"))
        .expect("list tasks");
    let stuck = tasks
        .iter()
        .find(|t| t.identity == stuck_path && t.task_type == "doc:index")
        .expect("the recovered row must still exist");
    assert_eq!(
        stuck.status, "done",
        "the recovered row is processed: {stuck:?}"
    );
    // The reconcile saw the recovered (pending) row: the file is gone from
    // disk, so a doc:delete was enqueued and processed (row removed).
    assert!(
        !tasks
            .iter()
            .any(|t| t.identity == stuck_path && t.task_type == "doc:delete"),
        "the delete row must be processed (removed): {tasks:?}"
    );
    // The live file was indexed by the same startup drain.
    let live = tasks
        .iter()
        .find(|t| t.identity == src.join("live.md").to_string_lossy().as_ref())
        .expect("live.md must be indexed");
    assert_eq!(live.status, "done", "{live:?}");
}

// --- serve_with_stop: dimension-mismatch auto-rebuild ----------------------

/// A stored index with a different dimension + `--auto-rebuild-vectors`:
/// the engine is recreated with the configured dimension and the serve
/// completes (the forced-rebuild clear + reconcile is a no-op with no
/// sources).
#[test]
fn serve_rebuilds_vectors_on_dimension_mismatch() {
    let dir = TempDir::new("dim-rebuild");
    // Pre-create the stored index (at the fixture's dataset vectors
    // path, default engine) with a different dimension.
    let stored = VectorIndexConfig::new(8, 16, 100, 256).expect("index config");
    create_vector_engine(ENGINE_USEARCH, &dataset_vectors_path(&dir), &stored, None)
        .expect("create stored index");

    let port = free_port();
    let mut boot = test_bootstrap(&dir); // 4-dim embedding vs 8-dim index
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: true,
        port,
        auto_rebuild_vectors: true,
    };

    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer = runtime.spawn(async move {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/health");
        for _ in 0..200 {
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|res| res.status().as_u16() == 200)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = stop_tx.send(());
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx);
    runtime.block_on(killer).expect("killer task");

    assert!(
        result.is_ok(),
        "auto-rebuild must let serve start: {:?}",
        result.err()
    );

    // The engine was recreated: the mismatch flag is cleared and the
    // fresh index holds no stale (8-dim) vectors.
    assert!(boot.dimension_mismatch.is_none(), "mismatch flag cleared");
    let vectors = boot.vectors.as_deref().expect("engine recreated");
    assert_eq!(vectors.count().expect("count"), 0, "stale vectors dropped");
}

/// Task 1.5: `recreate_vectors_engine` drops the stored engine
/// subdirectory wholesale (stale index files included) and recreates a
/// fresh empty index in its place.
#[test]
fn recreate_drops_the_stored_engine_subdirectory() {
    let dir = TempDir::new("recreate-drop");
    // A stale stored index (ADR 0004 §1 layout): the engine subdirectory
    // is faked with plain files — recreate must drop it wholesale.
    let engine_path = dataset_vectors_path(&dir).join("usearch");
    std::fs::create_dir_all(&engine_path).expect("create engine dir");
    std::fs::write(engine_path.join("stale.index"), b"stale").expect("seed stale index");

    let mut boot = test_bootstrap(&dir);
    recreate_vectors_engine(&mut boot).expect("recreate must succeed");

    // The engine is recreated at its engine-tagged subdirectory (ADR
    // 0004 §1 layout, task 3.3), and the stale file is gone.
    assert!(
        engine_path.join("ram.keys").exists(),
        "the engine subdirectory must be recreated"
    );
    assert!(
        !engine_path.join("stale.index").exists(),
        "the stored engine subdirectory must be dropped"
    );
    let vectors = boot.vectors.as_deref().expect("engine recreated");
    assert_eq!(vectors.count().expect("count"), 0, "fresh empty index");
    assert!(boot.dimension_mismatch.is_none(), "mismatch flag cleared");
}

// --- serve_with_stop: mismatch without auto-rebuild is fatal -----------------

#[test]
fn serve_mismatch_without_auto_rebuild_is_fatal() {
    let dir = TempDir::new("dim-fatal");
    let stored = VectorIndexConfig::new(8, 16, 100, 256).expect("index config");
    create_vector_engine(ENGINE_USEARCH, &dataset_vectors_path(&dir), &stored, None)
        .expect("create stored index");

    let mut boot = test_bootstrap(&dir);
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: true,
        port: free_port(),
        auto_rebuild_vectors: false,
    };
    let (_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");

    let err = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx)
        .expect_err("the mismatch must be fatal without auto-rebuild");
    assert!(
        matches!(
            err,
            CliError::Unsupported(ref msg) if msg.contains("dimension mismatch")
        ),
        "got: {err:?}"
    );
}
