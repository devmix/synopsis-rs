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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use config::onnx::{ModelInfo, OnnxModelsConfig};
use config::preset::GraphConfig;
use config::{Config, GlobalConfig, OnnxConfig};
use db::{
    ChunkDao, ConnectionOrTx, DocIndexPayload, DocumentDao, QueueTaskDao, QueueTaskType, ReIndexOp,
};
use embedding::{EmbeddingError, EmbeddingProvider};
use graph::GraphIndex;
use ingestion::ShutdownFlag;
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

/// A 4-dim embedding provider that simulates the field case
/// (fix-serve-signal-shutdown 1.2): on the first embedding batch only
/// (guarded by `first`) it (a) sends the "entered" oneshot token,
/// (b) cancels the shutdown flag and sends the stop broadcast — the
/// production signal task's order — and (c) blocks on the release channel
/// (simulating the slow LLM call) before returning the fixed vectors.
struct GateEmbed {
    /// The cooperative shutdown flag (cancelled on the first batch, before
    /// the broadcast send).
    flag: ShutdownFlag,
    /// The stop broadcast sender (fed on the first batch).
    stop_tx: broadcast::Sender<()>,
    /// The "entered" token: the test's release task awaits the receiving
    /// end. `Option` because `oneshot::Sender::send` consumes the sender
    /// (it is taken on the first batch); the `Mutex` keeps the provider
    /// `Sync`.
    entered_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// The release channel: the first batch blocks here (the slow LLM call)
    /// until the test sends the token.
    release_rx: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    /// First-batch guard (atomic: the provider is `Sync`).
    first: AtomicBool,
}

impl EmbeddingProvider for GateEmbed {
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        // First embedding batch only (the guard is an atomic swap).
        if self.first.swap(false, Ordering::SeqCst) {
            // (a) the in-flight task has entered the embedding step
            // (synchronous send — fine from the owner thread).
            let entered_tx = self.entered_tx.lock().unwrap().take().unwrap();
            let _ = entered_tx.send(());
            // (b) the production signal task's order: cancel the flag
            // FIRST, then the broadcast (fix-serve-signal-shutdown D1).
            self.flag.cancel();
            let _ = self.stop_tx.send(());
            // (c) the slow LLM call: block until the test releases.
            self.release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(texts.iter().map(|_| vec![0.5f32; 4]).collect())
    }

    fn vector_dim(&self) -> usize {
        4
    }

    fn name(&self) -> &'static str {
        "gate"
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

/// The `onnx.yaml` registry fixture for the 4-dim fake provider
/// (registry-as-model-source-of-truth D4): `models.default` selects the
/// entry, whose `vector_dim` is the index dimension.
fn test_onnx() -> OnnxConfig {
    OnnxConfig {
        runtime: Default::default(),
        models: OnnxModelsConfig {
            default: "bge-m3-int8".to_string(),
            entries: vec![ModelInfo {
                name: "bge-m3-int8".to_string(),
                vector_dim: 4,
                ..Default::default()
            }],
        },
    }
}

/// A Bootstrap with `embed` as the provider and a temp-file db; no sources
/// (global `None`) — the task's "no real sources" shape. The dataset is
/// active (design D2): named `edtech` with the directory present.
fn test_bootstrap_with_embed(dir: &TempDir, embed: Arc<dyn EmbeddingProvider>) -> Bootstrap {
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
        embed,
        onnx: test_onnx(),
        registry: None,
        prompts: None,
        vectors: None,
        dimension_mismatch: None,
    }
}

/// A Bootstrap with a fake 4-dim provider and a temp-file db (the default
/// [`test_bootstrap_with_embed`] shape).
fn test_bootstrap(dir: &TempDir) -> Bootstrap {
    test_bootstrap_with_embed(dir, Arc::new(FakeEmbed { dim: 4 }))
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

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer_flag = shutdown_flag.clone();
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
        // The production signal task's order: cancel the flag first, then
        // the broadcast (fix-serve-signal-shutdown D1).
        killer_flag.cancel();
        let _ = stop_tx.send(());
        healthy
    });

    let started = std::time::Instant::now();
    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
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
///
/// The fixture's prior state also carries the vector-loss-self-heal D2
/// shape: `same.md`'s chunk row has no vector (the engine is fresh — the
/// RAM layer an unclean shutdown would lose), so the startup self-heal
/// re-queues it as `doc:index` with the `ReEmbed` op; the startup drain
/// routes it to the targeted re-embed (vector-loss-self-heal D5 — no
/// re-parse / re-chunk, the pipeline's hash-dedup is never reached),
/// restores the vector, and processes the row to `done`. After the drain,
/// every live chunk row has a vector.
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
    // with its current hash and a live chunk WITHOUT a vector (the engine
    // is fresh — the missing-vector state the startup self-heal detects
    // and re-queues, and the chunk keeps `same.md` out of the orphan
    // sweep), `gone.md` (no file on disk anymore), and a chunk-less ghost
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

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer_flag = shutdown_flag.clone();
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
        // The production signal task's order: cancel the flag first, then
        // the broadcast (fix-serve-signal-shutdown D1).
        killer_flag.cancel();
        let _ = stop_tx.send(());
        healthy
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
    let healthy = runtime.block_on(killer).expect("killer task");

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    assert!(healthy, "/health must answer 200 before the stop signal");

    // The worker processed the queued diff during serve startup: new.md
    // and changed.md are (re)indexed and their task rows flip to `done`;
    // gone.md is deleted and its task row removed; same.md is unchanged
    // per the content-hash reconcile (the reconcile does not re-queue it)
    // but its chunk row has no vector, so the startup self-heal
    // (vector-loss-self-heal D2) re-queues it with the `ReEmbed` op and
    // the startup drain routes it to the targeted re-embed (D5),
    // restoring the vector and processing the row to `done`.
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
        3,
        "new + changed + same processed (done), gone's row removed: {tasks:?}"
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
    // The self-heal's re-queue of same.md: a `doc:index` row whose payload
    // carries no content hash (the reconcile's rows carry one — the worker
    // re-reads and re-hashes the file instead).
    let same_task = tasks_by_path
        .get(src.join("same.md").to_string_lossy().as_ref())
        .expect("same.md must be re-queued by the startup self-heal");
    assert_eq!(same_task.task_type, "doc:index");
    assert_eq!(same_task.status, "done", "{same_task:?}");
    let same_payload: DocIndexPayload =
        serde_json::from_str(&same_task.event).expect("same.md payload parses");
    assert_eq!(
        same_payload.content_hash, None,
        "the self-heal's payload carries no content hash: {same_task:?}"
    );

    // The self-heal's payload requested the targeted re-embed (design D5):
    // the full pipeline's content-hash dedup would have skipped the
    // unchanged document and left the vector missing.
    assert_eq!(
        same_payload.ops,
        vec![ReIndexOp::ReEmbed],
        "the self-heal's payload must carry the ReEmbed op: {same_task:?}"
    );

    // End-to-end (vector-loss-self-heal D5): after the startup drain every
    // live chunk row has a vector in the engine — the fresh engine's RAM
    // layer (the state an unclean shutdown would lose) holds the
    // re-embedded same.md chunk alongside the fresh new.md / changed.md
    // chunks.
    let vectors = boot.vectors.as_deref().expect("engine opened");
    let indexed: std::collections::HashSet<u32> = vectors
        .chunk_ids()
        .expect("chunk ids")
        .into_iter()
        .collect();
    let chunk_rows = boot
        .db
        .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_id_doc_id())
        .expect("with_conn chunks")
        .expect("list chunk rows");
    assert!(
        !chunk_rows.is_empty(),
        "the live documents must have chunk rows"
    );
    let missing: Vec<u32> = chunk_rows
        .iter()
        .map(|(id, _)| *id as u32)
        .filter(|id| !indexed.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "every live chunk must have a vector after the startup drain: {missing:?}"
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
                    ops: vec![ReIndexOp::Full],
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

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer_flag = shutdown_flag.clone();
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
        // The production signal task's order: cancel the flag first, then
        // the broadcast (fix-serve-signal-shutdown D1).
        killer_flag.cancel();
        let _ = stop_tx.send(());
        healthy
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
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

// --- serve_with_stop: stop during the startup drain (task 1.2) -------------

/// The field bug's exact scenario (fix-serve-signal-shutdown 1.2): SIGINT
/// arrives DURING the startup drain — while the worker is processing the
/// in-flight task (the embedding step, here a slow LLM call). The
/// production signal task's behavior is mirrored inside the embedding
/// provider ([`GateEmbed`]): the shutdown flag is cancelled, then the stop
/// broadcast is sent. The drain must stop after the in-flight task (it
/// completes and is recorded `done`), the two unclaimed tasks stay
/// `pending` (they survive to the next startup), and the serve flow
/// proceeds to the bounded shutdown and returns `Ok` (no hang).
///
/// Note on timing (Rev 1): the stop broadcast is sent BEFORE the axum
/// serve task's receiver resubscribes (the resubscribe happens after the
/// drain), and a broadcast receiver only sees messages sent after it
/// subscribed — so the resubscribed receiver never sees the stop message.
/// The serve task's shutdown future therefore checks the flag first: the
/// flag was cancelled before the broadcast send, so it resolves immediately
/// and axum drains at once (the owner loop's ORIGINAL receiver, subscribed
/// at channel creation, still gets the buffered message on its first
/// `recv()`). The whole shutdown takes the graceful path — well under
/// `SHUTDOWN_TIMEOUT` — and the assertion below pins that fast path (a
/// regression to the forced path would wait the full bound here).
#[test]
fn serve_stop_cancels_the_startup_drain_mid_task() {
    let dir = TempDir::new("drain-stop");
    let port = free_port();
    // One markdown source with three files: the startup reconcile enqueues
    // three `doc:index` rows for the startup drain.
    let src = dir.as_ref().join("src");
    std::fs::create_dir_all(&src).expect("create source dir");
    for (i, name) in ["one.md", "two.md", "three.md"].iter().enumerate() {
        std::fs::write(src.join(name), format!("# Doc {i}\n\nBody {i}.\n")).expect("write md");
    }

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let mut boot = test_bootstrap_with_embed(
        &dir,
        Arc::new(GateEmbed {
            flag: shutdown_flag.clone(),
            stop_tx,
            entered_tx: std::sync::Mutex::new(Some(entered_tx)),
            release_rx: std::sync::Mutex::new(release_rx),
            first: AtomicBool::new(true),
        }),
    );
    boot.global = Some(one_markdown_source(&src));
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: false,
        port,
        auto_rebuild_vectors: false,
    };

    let runtime = Runtime::new().expect("test runtime");
    // The "slow LLM" release: once the in-flight task has entered the
    // embedding step (and fired the stop, as the production signal task
    // would), let it finish.
    let releaser = runtime.spawn(async move {
        let _ = entered_rx.await;
        let _ = release_tx.send(());
    });

    // The test thread is the owner thread (no runtime context — the
    // vector-engine facade is safe here, mirroring `run_serve`'s main
    // thread).
    let started = std::time::Instant::now();
    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
    let stopped = started.elapsed();
    runtime.block_on(releaser).expect("releaser task");

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    // The graceful fast path (Rev 1): the serve task's shutdown future
    // resolves via the flag check (its resubscribed receiver never sees
    // the pre-subscription broadcast), so axum drains at once and the
    // whole shutdown lands well under the forced bound — a regression to
    // the forced path would wait the full `SHUTDOWN_TIMEOUT` here.
    assert!(
        stopped < SHUTDOWN_TIMEOUT,
        "stop must take the graceful path (well under the bound): {stopped:?}"
    );

    // The drain stopped after the in-flight task: exactly one task is
    // `done`, the other two rows are still `pending` (unclaimed — they
    // survive to the next startup).
    let tasks = boot
        .db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .expect("with_conn tasks")
        .map_err(|e| panic!("list tasks: {e}"))
        .expect("list tasks");
    assert_eq!(tasks.len(), 3, "the three enqueued rows: {tasks:?}");
    let done: Vec<_> = tasks.iter().filter(|t| t.status == "done").collect();
    let pending: Vec<_> = tasks.iter().filter(|t| t.status == "pending").collect();
    assert_eq!(
        done.len(),
        1,
        "exactly the in-flight task is done: {tasks:?}"
    );
    assert_eq!(
        pending.len(),
        2,
        "the unclaimed tasks stay pending: {tasks:?}"
    );

    // Exactly one document is indexed — the in-flight task's.
    let docs = boot
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .expect("with_conn documents")
        .expect("list documents");
    assert_eq!(docs.len(), 1, "exactly one document is indexed: {docs:?}");
    assert_eq!(
        docs[0].original_path, done[0].identity,
        "the indexed document is the in-flight task's: {docs:?}"
    );
}

// --- serve_with_stop: graceful stop ends the SSE session (task 1.4) --------

/// The field scenario (fix-serve-signal-shutdown 1.4 / design D5): a
/// connected legacy SSE client must not hold the graceful stop open — the
/// server ends its own SSE streams when the stop fires (the serve task's
/// shutdown future calls `close_all_sessions`), so the axum drain completes
/// promptly. `serve_with_stop` (temp db, fake embed, no sources) runs on a
/// free port; a spawned task (a) waits for `/health` 200, (b) opens
/// `GET /sse` (the handler registers the session before answering) and
/// reads the `endpoint` frame from the body, (c) cancels the shutdown flag
/// and sends the stop broadcast (the production signal task's order).
/// The test asserts: `serve_with_stop` returns `Ok`, the stop took the
/// graceful path (well under `SHUTDOWN_TIMEOUT` — a regression that left
/// the stream open would wait out the full 10 s bound), and the SSE body
/// ended (the client saw the endpoint frame and then a clean EOF).
#[test]
fn serve_graceful_stop_ends_the_sse_session() {
    let dir = TempDir::new("sse-stop");
    let port = free_port();
    let mut boot = test_bootstrap(&dir);
    let req = ServeRequest {
        cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
        dataset: None,
        no_initial_sync: true,
        port,
        auto_rebuild_vectors: false,
    };

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let client_flag = shutdown_flag.clone();
    // The connected legacy SSE client: wait for `/health` 200, open
    // `GET /sse`, then fire the stop in the production signal task's order
    // (cancel the flag FIRST, then the broadcast — fix-serve-signal-shutdown
    // D1). The body read runs to EOF: the server ends the stream on the
    // graceful stop, so it completes with the endpoint frame in hand.
    let client = runtime.spawn(async move {
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
        let response = client
            .get(format!("http://127.0.0.1:{port}/sse"))
            .send()
            .await;
        let sse_ok = response
            .as_ref()
            .is_ok_and(|res| res.status().as_u16() == 200);
        // The production signal task's order: cancel the flag first, then
        // the broadcast (fix-serve-signal-shutdown D1).
        client_flag.cancel();
        let _ = stop_tx.send(());
        // The body read completes at EOF: the server ends the stream on
        // the graceful stop (a regression that left it open would hold
        // this read — and the axum drain — until the forced bound).
        let body = match response {
            Ok(response) => response.text().await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        (healthy, sse_ok, body)
    });

    let started = std::time::Instant::now();
    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
    let stopped = started.elapsed();

    assert!(
        result.is_ok(),
        "serve_with_stop must succeed: {:?}",
        result.err()
    );
    // The graceful path (design D5): the server ended its own SSE stream
    // when the stop resolved, so the axum drain completed promptly — a
    // regression (stream left open) would wait out the full bound here.
    assert!(
        stopped < SHUTDOWN_TIMEOUT,
        "stop must take the graceful path (well under the bound): {stopped:?}"
    );

    let (healthy, sse_ok, body) = runtime.block_on(client).expect("sse client task");
    assert!(healthy, "/health must answer 200 before the stop signal");
    assert!(
        sse_ok,
        "GET /sse must answer 200 (the session is registered)"
    );
    // The client saw the endpoint frame (confirming the session was
    // registered) and then a clean EOF.
    assert!(
        body.contains("event: endpoint"),
        "the endpoint frame must arrive before the EOF: {body}"
    );
    assert!(
        body.contains("sessionId="),
        "the endpoint frame carries the session URL: {body}"
    );
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

    let shutdown_flag = ShutdownFlag::new();
    let (stop_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");
    let killer_flag = shutdown_flag.clone();
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
        // The production signal task's order: cancel the flag first, then
        // the broadcast (fix-serve-signal-shutdown D1).
        killer_flag.cancel();
        let _ = stop_tx.send(());
    });

    let result = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag);
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
    let shutdown_flag = ShutdownFlag::new();
    let (_tx, mut stop_rx) = broadcast::channel::<()>(1);
    let runtime = Runtime::new().expect("test runtime");

    let err = serve_with_stop(&runtime, &mut boot, &req, &mut stop_rx, &shutdown_flag)
        .expect_err("the mismatch must be fatal without auto-rebuild");
    assert!(
        matches!(
            err,
            CliError::Unsupported(ref msg) if msg.contains("dimension mismatch")
        ),
        "got: {err:?}"
    );
}
