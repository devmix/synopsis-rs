//! MCP mount and graceful shutdown (design D4 + D7).
//!
//! Flow: bootstrap → port override → dimension-mismatch handling → startup
//! health check → runner → initial sync → document worker → graph load →
//! searcher → MCP server → file watcher → signal wait → graceful shutdown.
//!
//! # Owner-thread architecture (verified runtime-context constraint)
//!
//! Two hard constraints shape this module (both verified empirically
//! against tokio 1.53):
//!
//! 1. The ingestion [`ingestion::Runner`] is `!Send` — it borrows the
//!    source registry, whose `Box<dyn Source>` objects carry no Send/Sync
//!    bounds — so the ingestion work must stay on the thread that created
//!    the runner: the main thread of [`run_serve`].
//! 2. The vectors engine and the embedding provider are sync facades over
//!    their own runtimes and must never be called from inside a tokio
//!    runtime context (a nested `block_on` panics with "Cannot start a
//!    runtime from within a runtime"; `vectors::UsearchEngine` module docs:
//!    sync methods, `spawn_blocking` workers only).
//!
//! The serve flow is therefore a *synchronous* function on the main
//! thread:
//!
//! - all vector-engine work (engine open/create/recreate, and the document
//!   worker's per-job pipeline + GC sweep) runs inline on the main thread
//!   *between* runtime-context entries (the watcher batch, the startup
//!   reconcile and the forced-rebuild clear + reconcile only enqueue
//!   `queue_tasks` rows — no vector-engine work);
//! - the runtime is entered only for short async bits that require it:
//!   creating the watcher debounce task, spawning the worker's sweep-tick
//!   timer, binding the listener, and the owner-loop event select
//!   (`select!` over the serve task, the stop signal, the watcher batches,
//!   the sweep tick, and the retry token — fresh futures per iteration);
//! - the axum HTTP listener runs as a spawned task with
//!   `with_graceful_shutdown` on the stop signal; the MCP tool dispatch is
//!   hopped to `spawn_blocking` at the mcp crate boundary (`call_tool`,
//!   the same precedent as `GET /health`), so the search path stays out
//!   of async contexts too.
//!
//! # Searcher lifetime
//!
//! The search crate's [`HybridSearcher`] is immutable and borrows a pooled
//! connection per unit of work (db design D11 — a raw connection can never
//! be held). [`PooledSearcher`] implements the object-safe [`Searcher`]
//! contract for the server's lifetime: every call checks out a pooled
//! connection and assembles a fresh (cheap) hybrid searcher on it. After a
//! graph reload the serve loop constructs a fresh handle (with the
//! reloaded index) and swaps it — plus the graph handle — into the MCP
//! server (design D8: an explicit handle swap instead of an in-place
//! mutation; see [`mcp::Server::set_searcher`] / [`mcp::Server::set_graph`]).
//!
//! # Dimension-mismatch rebuild
//!
//! In the Rust codebase the mismatch surfaces from the ANN engine at open
//! time, so the auto-rebuild drops the stored vector table and recreates
//! the engine, then clears the knowledge DB tables in place and re-enqueues
//! every source through the startup reconcile — clear-then-queue; the
//! worker re-embeds every file.
//!
//! # Shutdown
//!
//! SIGINT/SIGTERM (design D7) are installed as a spawned task (the tokio
//! signal API needs a runtime handle) and feed a broadcast the owner loop
//! selects on. axum stops accepting and drains in-flight requests; the
//! serve task and the watcher debounce task stop under one 10 s bound.
//! The document worker runs inline on the owner thread, so it stops with
//! the loop — no separate abort.
//!
//! # Exit behavior
//!
//! A serve error exits non-zero: a bind failure must not look like success.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use config::GlobalConfig;
use config::preset::{GraphConfig, SearchConfig};
use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, FactDao, QueueTaskDao};
use embedding::EmbeddingProvider;
use graph::GraphIndex;
use ingestion::worker::DocumentWorker;
use ingestion::{DocumentJobQueue, Runner, RunnerParams};
use search::{
    Enricher, GraphExpander, HybridSearcher, LexicalSearcher, Reranker, SearchError, SearchResult,
    Searcher, SemanticSearcher,
};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc};
use vectors::{ENGINE_USEARCH, VectorIndex, VectorsError, create_vector_engine};

use crate::db::clear_dataset_tables;
use crate::error::CliError;
use crate::serve::bootstrap::{self, Bootstrap};
use crate::serve::health::run_health_check;
use crate::serve::watcher::{ChangeHandler, IngestChangeHandler, Watcher};

/// Graceful-shutdown bound (10 s). `pub(crate)` so the integration tests
/// can bound their shutdown-timing assertions against it (re-exported
/// through [`crate::test_support`]).
pub(crate) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// One `serve` invocation: the effective config path plus the per-command
/// flags (clap `serve` subcommand).
pub struct ServeRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// `--dataset` dataset name override (wins over `config.dataset.name`).
    pub dataset: Option<String>,
    /// `--no-initial-sync`: skip the startup reconcile (the `queue_tasks`
    /// enqueue) on startup.
    pub no_initial_sync: bool,
    /// `--port` override; `0` keeps the `server.port` config value.
    pub port: u16,
    /// `--auto-rebuild-vectors` (OR-ed with the config flag).
    pub auto_rebuild_vectors: bool,
}

/// Search handle for the MCP server (design D4).
///
/// See the module docs for the lifetime design: one pooled connection per
/// call, a fresh immutable [`HybridSearcher`] per call. After a graph
/// reload the serve loop constructs a fresh handle (with the reloaded
/// index) and swaps it into [`mcp::Server::set_searcher`] — there is no
/// in-place mutation (search design D8).
pub struct PooledSearcher {
    db: db::Db,
    search_config: SearchConfig,
    graph_config: GraphConfig,
    embed: Arc<dyn EmbeddingProvider>,
    vectors: Arc<dyn VectorIndex>,
    graph: Arc<GraphIndex>,
}

impl PooledSearcher {
    /// Bind the handle to one serve session's collaborators.
    #[must_use]
    pub fn new(
        db: db::Db,
        search_config: SearchConfig,
        graph_config: GraphConfig,
        embed: Arc<dyn EmbeddingProvider>,
        vectors: Arc<dyn VectorIndex>,
        graph: Arc<GraphIndex>,
    ) -> Self {
        Self {
            db,
            search_config,
            graph_config,
            embed,
            vectors,
            graph,
        }
    }

    /// The active graph index (a cheap `Arc` clone).
    #[must_use]
    pub fn graph(&self) -> Arc<GraphIndex> {
        self.graph.clone()
    }

    /// Runs `f` on a per-call hybrid searcher assembled over a checked-out
    /// pooled connection (design D5 expander rule: present only when the
    /// graph index is Ready — `Unavailable` models the disabled config).
    ///
    /// Runs synchronously on the calling thread: the mcp dispatch boundary
    /// (`call_tool`) hops tool calls to `spawn_blocking`, and the tests
    /// call this from plain threads — both safe contexts for the
    /// vector-engine facade (module docs).
    fn run<T>(
        &self,
        f: impl FnOnce(&HybridSearcher<'_>) -> Result<T, SearchError>,
    ) -> Result<T, SearchError> {
        self.db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let chunk_entities = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            let expander = self
                .graph
                .ready()
                .map(|graph| GraphExpander::new(graph, &facts, &self.graph_config));
            let searcher = HybridSearcher::new(
                self.search_config.clone(),
                LexicalSearcher::new(&chunks),
                SemanticSearcher::new(&chunks, &documents, &*self.embed, &*self.vectors),
                Enricher::new(&documents, &chunk_entities),
                Reranker::new(Some(&self.search_config)),
                expander,
            );
            f(&searcher)
        })?
    }
}

impl Searcher for PooledSearcher {
    fn hybrid_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.run(|searcher| searcher.hybrid_search(query, top_k, domain))
    }

    fn lexical_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.run(|searcher| searcher.lexical_search(query, top_k, domain))
    }

    fn semantic_search(
        &self,
        query: &str,
        top_k: i32,
        domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.run(|searcher| searcher.semantic_search(query, top_k, domain))
    }
}

/// Production shutdown trigger (design D7): resolves on the first SIGINT or
/// SIGTERM (`ctrl_c` is the SIGINT receiver itself). If the SIGTERM handler
/// cannot be installed, degrades to SIGINT only.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => tracing::info!("received SIGINT, stopping..."),
                    _ = sigterm.recv() => tracing::info!("received SIGTERM, stopping..."),
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "install SIGTERM handler; falling back to SIGINT only"
                );
                let _ = ctrl_c.await;
                tracing::info!("received SIGINT, stopping...");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
        tracing::info!("received SIGINT, stopping...");
    }
}

/// The `serve` subcommand body (design D4/D7): bootstrap, then the
/// owner-thread serve flow with the production SIGINT/SIGTERM trigger.
///
/// Builds its own tokio runtime (the binary `main` is synchronous).
pub fn run_serve(req: &ServeRequest) -> ExitCode {
    let Ok(runtime) = Runtime::new() else {
        eprintln!("error: failed to build the tokio runtime");
        return ExitCode::FAILURE;
    };
    match serve(&runtime, req) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "serve failed");
            err.exit_code()
        }
    }
}

/// Bootstrap + owner-thread serve flow with the production SIGINT/SIGTERM
/// trigger. The signal is installed as a spawned task (the tokio signal
/// API needs a runtime handle) and feeds the stop broadcast the owner loop
/// selects on.
pub fn serve(runtime: &Runtime, req: &ServeRequest) -> Result<(), CliError> {
    let mut boot = bootstrap::bootstrap(&req.cfg_path, req.dataset.as_deref())?;
    let (signal_tx, mut stop) = broadcast::channel::<()>(1);
    runtime.spawn(async move {
        shutdown_signal().await;
        let _ = signal_tx.send(());
    });
    serve_with_stop(runtime, &mut boot, req, &mut stop)
}

/// The serve flow (design D4) over an already-bootstrapped application
/// state, with the stop trigger injected — the seam the integration tests
/// drive with stubs (temp db, fake embed provider, no real sources) and a
/// broadcast sent from a killer task instead of SIGINT/SIGTERM.
///
/// Synchronous by design (module docs): port override → health check →
/// runner + dimension-mismatch handling → initial sync → worker startup
/// drain → graph load → searcher + MCP server → file watcher → sweep-tick
/// spawn → bind → owner loop → graceful shutdown under the 10 s bound.
pub fn serve_with_stop(
    runtime: &Runtime,
    boot: &mut Bootstrap,
    req: &ServeRequest,
    stop: &mut broadcast::Receiver<()>,
) -> Result<(), CliError> {
    // D4: port override (CLI flag > config).
    if req.port > 0 {
        boot.config.server.port = i32::from(req.port);
    }
    let auto_rebuild = req.auto_rebuild_vectors || boot.config.embeddings.auto_rebuild_vectors;
    // `apply_defaults` (bootstrap) materializes an absent section.
    let auto_update = boot.config.auto_update.clone().unwrap_or_default();

    // Startup health check (log-only; serve continues on errors).
    if let Err(err) = run_health_check(&boot.db, boot.embed.as_ref(), &boot.config) {
        tracing::warn!(error = %err, "startup health check failed");
    }

    // Collaborator extraction (D4): everything the rest of the flow needs
    // is cloned into owned locals before the ingestion `Runner` is
    // constructed. The `Runner` borrows these locals (not `boot`), so the
    // bootstrap state is no longer pinned by its borrow for the whole
    // owner loop.
    let db = boot.db.clone();
    let cache = boot.cache.clone();
    let embed = boot.embed.clone();
    let config = boot.config.clone();
    let global = boot.global.clone();
    let domains = boot.domains.clone();

    // Runner assembly + vector dimension-mismatch handling (D4). The
    // mismatch surfaces from the ANN engine at open time; auto-rebuild
    // recreates the engine and forces a clear + re-enqueue below. The
    // engine is opened before the runner so its `Arc` can be captured
    // into a local while `boot` is still borrowable.
    let mut force_rebuild = false;
    let vectors: Arc<dyn VectorIndex> = loop {
        match bootstrap::open_vectors_engine(boot) {
            Ok(()) => {
                break match boot.vectors.clone() {
                    Some(vectors) => vectors,
                    // Unreachable: the engine is opened above.
                    None => {
                        return Err(CliError::Unsupported(
                            "vector engine not assembled".to_string(),
                        ));
                    }
                };
            }
            Err(CliError::Vectors(VectorsError::DimensionMismatch { .. })) if auto_rebuild => {
                force_rebuild = true;
                recreate_vectors_engine(boot)?;
            }
            Err(CliError::Vectors(VectorsError::DimensionMismatch { .. })) => {
                return Err(CliError::Unsupported(
                    "vector dimension mismatch detected; run with --auto-rebuild-vectors \
                     (or set embeddings.auto_rebuild_vectors in config) to rebuild the \
                     vectors automatically, or delete the stored vector index"
                        .to_string(),
                ));
            }
            Err(err) => return Err(err),
        }
    };
    let registry = bootstrap::build_registry(&config.ingestion.chunking)
        .map_err(|err| CliError::Unsupported(format!("source registry: {err}")))?;
    let prompts = ingestion::load_ner_prompts(&config.paths.prompts_path).map_err(|err| {
        CliError::Config(config::ConfigError::Validation {
            message: format!("NER prompts: {err}"),
        })
    })?;
    // The runner borrows the locals (same wiring as `build_runner`), so it
    // outlives any use of `boot` without pinning it.
    let runner = Runner::new(RunnerParams {
        db: &db,
        ingest_cfg: &config.ingestion,
        global: global.as_ref(),
        domains: &domains,
        registry: &registry,
        embed: embed.as_ref(),
        vectors: vectors.as_ref(),
        prompts: &prompts,
        linker_cfg: &config.linker,
        prompts_path: &config.paths.prompts_path,
        llm_cache: cache,
    });

    // The `queue_tasks` producer (event-queue-incremental-linking task 1.2):
    // the startup reconcile and the watcher enqueue through it; the
    // background worker is the sole consumer.
    let job_queue = DocumentJobQueue::new(&db);

    // Initial sync (event-queue-incremental-linking task 1.2): startup is a
    // producer — reconcile every enabled source against the disk and enqueue
    // the diff into `queue_tasks`; the background worker runs the pipeline
    // before the index is up to date. No direct ingestion at startup.
    // No-data semantics (design D2): without an active dataset there is
    // nothing to reconcile — the server simply starts with an empty index.
    let initial_sync_due =
        bootstrap::has_active_dataset(&config) && auto_update.initial_sync && !req.no_initial_sync;
    // Restart recovery (event-queue-incremental-linking task 1.5): rows
    // left in `processing` by an unclean shutdown (crash, SIGKILL, power
    // loss) are never claimed again (the worker claims only `pending`) and
    // the startup reconcile skips them — reset them to `pending` BEFORE the
    // reconcile so the recovered rows are visible to both (the worker's
    // startup drain below then processes them).
    let recovered = db
        .with_conn(|conn| {
            QueueTaskDao::new(ConnectionOrTx::Connection(conn))
                .recover_stuck_processing(now_unix_seconds())
        })
        .map_err(CliError::Db)?
        .map_err(|e| CliError::Unsupported(format!("queue task: {e}")))?;
    if recovered > 0 {
        tracing::info!(
            recovered,
            "recovered stuck queue tasks (processing -> pending)"
        );
    }
    if force_rebuild {
        // The vector engine was recreated: the stored vectors are gone, and
        // the producer's content-hash diff cannot force re-embedding of
        // unchanged documents. The recovery is clear-then-queue: clear the
        // knowledge DB tables IN PLACE (the `Db` handle is still open and
        // borrowed by the runner/job queue/worker — deleting the state
        // directory would orphan the pooled connections), then run the same
        // startup reconcile so the worker re-embeds every source file.
        tracing::info!("forced rebuild started (clear dataset tables + queue reconcile)");
        clear_dataset_tables(&db)?;
        let (enqueued_indexed, enqueued_deleted, failed_sources) =
            reconcile_enabled_sources(&job_queue, &runner, &global, "forced rebuild");
        tracing::info!(
            enqueued_indexed,
            enqueued_deleted,
            failed_sources,
            "forced rebuild finished (jobs enqueued; the worker processes them)"
        );
    } else if initial_sync_due {
        tracing::info!("initial sync started (queue reconcile)");
        let (enqueued_indexed, enqueued_deleted, failed_sources) =
            reconcile_enabled_sources(&job_queue, &runner, &global, "startup");
        tracing::info!(
            enqueued_indexed,
            enqueued_deleted,
            failed_sources,
            "initial sync finished (jobs enqueued; the worker processes them)"
        );
    }

    // Startup vector self-heal (vector-loss-self-heal D2): chunk rows whose
    // vector was lost with the RAM layer (an unclean shutdown, SIGKILL) are
    // re-embedded by the ordinary pipeline — one `doc:index` per affected
    // document, processed by the startup drain below. A failure is a
    // warning without aborting startup (the next startup retries).
    let healed = match runner.heal_missing_vectors(now_unix_seconds()) {
        Ok(healed) => healed,
        Err(err) => {
            tracing::warn!(error = %err, "startup vector self-heal failed");
            0
        }
    };
    if healed > 0 {
        tracing::info!(
            healed,
            "re-queued documents with missing vectors for re-embedding"
        );
    }

    // The event-queue worker (event-queue-incremental-linking task 1.2):
    // the sole consumer of the queue. The Runner is `!Send`, so the worker
    // runs on this owner thread: one immediate drain right here (the startup
    // reconcile's diff is processed before the index serves traffic), then
    // the owner-loop select! keeps it ticking — the periodic sweep (below)
    // and an on-demand cycle after a watcher batch enqueues new work.
    let worker = DocumentWorker::new(&db, &runner);
    if let Err(err) = worker.run_once(now_unix_seconds()) {
        tracing::warn!(error = %err, "worker startup cycle failed");
    }

    // The periodic retry sweep (document-jobs-queue 1.6): a spawned timer
    // hands a tick token to the owner loop every poll interval; the worker
    // cycle itself runs inline (the Runner is `!Send`). The `retry_failed`
    // config (task 1.2) gates the sweep: disabled → no timer, and the
    // sender kept alive below keeps the channel open so the select arm
    // stays pending instead of hot-looping on a closed channel.
    let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<()>();
    let poll_interval =
        Duration::from_secs(auto_update.retry_failed.poll_interval_seconds.max(1) as u64);
    if auto_update.retry_failed.enabled {
        let tick_tx = tick_tx.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(poll_interval);
            // The first tick is immediate; the startup drain above already
            // ran a cycle, so the first scheduled one is a full interval out.
            interval.tick().await;
            loop {
                interval.tick().await;
                if tick_tx.send(()).is_err() {
                    break;
                }
            }
        });
    }

    // On-demand worker wake-up: a settled watcher batch enqueues new jobs
    // (the change handler is a producer); the token triggers a worker cycle
    // now instead of waiting for the next sweep tick.
    let (retry_tx, mut retry_rx) = mpsc::unbounded_channel::<()>();

    // Knowledge graph (D4): `Unavailable` models the disabled config —
    // `GraphIndex::from_db` is a no-op for those flags.
    let graph = Arc::new(GraphIndex::from_db(&db, &config.graph)?);

    // Searcher + MCP server (D4). The hook server shares the hot-swap locks
    // with the router's copy (the rmcp factory clones it per session).
    let search_config = config.search.clone();
    let graph_config = config.graph.clone();
    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(PooledSearcher::new(
        db.clone(),
        search_config.clone(),
        graph_config.clone(),
        embed.clone(),
        vectors.clone(),
        graph.clone(),
    ));
    let server = mcp::Server::new(
        config.server.name.clone(),
        config.server.version.clone(),
        db.clone(),
        searcher,
        graph.clone(),
    );
    let hook_server = server.clone();
    let hook: Arc<dyn Fn(Arc<GraphIndex>) + Send + Sync> = Arc::new({
        let db = db.clone();
        let search_config = search_config.clone();
        let graph_config = graph_config.clone();
        let embed = embed.clone();
        let vectors = vectors.clone();
        move |reloaded: Arc<GraphIndex>| {
            // The searcher is immutable (design D8): rebuild and swap.
            hook_server.set_graph(reloaded.clone());
            hook_server.set_searcher(Arc::new(PooledSearcher::new(
                db.clone(),
                search_config.clone(),
                graph_config.clone(),
                embed.clone(),
                vectors.clone(),
                reloaded,
            )));
            tracing::info!("searcher and MCP handles rebuilt after graph reload");
        }
    });
    let router = server.router();

    // D5: file watcher over every enabled source. `from_config` spawns the
    // debounce task, so it runs inside a brief runtime-context entry; the
    // change handler itself runs on the owner thread (the `!Send` runner
    // seam, watcher module docs). Setup failure is a warning without the
    // watcher.
    let mut watcher: Option<Watcher> = None;
    let mut handler: Option<IngestChangeHandler<'_>> = None;
    if auto_update.enabled && auto_update.watch_sources {
        let debounce = Duration::from_secs(auto_update.debounce_seconds.max(0) as u64);
        match runtime.block_on(async { Watcher::from_config(debounce, &config, &registry) }) {
            Ok(new_watcher) => {
                let new_handler =
                    IngestChangeHandler::new(&runner, &db, &config.graph, Some(hook), &job_queue);
                watcher = Some(new_watcher);
                handler = Some(new_handler);
            }
            Err(err) => tracing::warn!(
                error = %err,
                "file watcher setup failed; auto-update disabled"
            ),
        }
    }

    // Bind the listener and start serving (D4/D7). The axum serve task owns
    // a second stop receiver: the signal stops accepting and drains
    // in-flight requests.
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = runtime.block_on(TcpListener::bind(&addr))?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "MCP server listening (Streamable HTTP)");
    let mut stop_for_serve = stop.resubscribe();
    let mut serve_handle = runtime.spawn(async move {
        let shutdown = async move {
            let _ = stop_for_serve.recv().await;
        };
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
    });

    // Owner loop: the watcher callback and the document worker cycle run
    // inline on the main thread (no runtime context — the vector-engine
    // facade is safe here); the runtime is entered only for the event
    // select. Fresh
    // futures per iteration keep each select self-contained.
    let mut serve_result: Option<io::Result<()>> = None;
    'owner: loop {
        let event = match (watcher.as_mut(), handler.as_ref()) {
            (Some(watcher), Some(_handler)) => runtime.block_on(async {
                tokio::select! {
                    result = &mut serve_handle => OwnerEvent::ServeDone(result),
                    _ = stop.recv() => OwnerEvent::Stop,
                    batch = watcher.next_batch() => OwnerEvent::Batch(batch),
                    _ = retry_rx.recv() => OwnerEvent::RetryBatch,
                    _ = tick_rx.recv() => OwnerEvent::Tick,
                }
            }),
            _ => runtime.block_on(async {
                tokio::select! {
                    result = &mut serve_handle => OwnerEvent::ServeDone(result),
                    _ = stop.recv() => OwnerEvent::Stop,
                    _ = retry_rx.recv() => OwnerEvent::RetryBatch,
                    _ = tick_rx.recv() => OwnerEvent::Tick,
                }
            }),
        };
        match event {
            OwnerEvent::ServeDone(result) => {
                serve_result = Some(reap_serve(result));
                break 'owner;
            }
            // The stop signal fired (axum is draining in-flight requests)
            // or the watcher debounce task ended: leave the loop; the
            // bounded shutdown below reaps the serve task. The worker runs
            // inline on this thread, so leaving the loop stops it — no
            // separate abort.
            OwnerEvent::Stop | OwnerEvent::Batch(None) => break 'owner,
            OwnerEvent::Batch(Some(paths)) => {
                // A batch can only arrive from the watcher arm, so the
                // handler exists there.
                if let Some(handler) = handler.as_ref() {
                    handler.handle_changes(&paths);
                    // The handler enqueued jobs: run a worker cycle now
                    // instead of waiting for the next sweep tick.
                    let _ = retry_tx.send(());
                }
            }
            // The periodic sweep tick or an on-demand retry: one worker
            // cycle (claim due jobs + GC) inline on the owner thread.
            OwnerEvent::RetryBatch | OwnerEvent::Tick => run_worker_cycle(&worker),
        }
    }

    // ADR 0004 §9 (usearch-wal-persistence task 3.9): the shutdown save
    // point — persist the vector engine's in-memory state (the usearch
    // RAM layer snapshot) before exit. Best-effort: a failure warns (the
    // cascade protocol tolerates the unsaved RAM window) and never fails
    // the serve.
    if let Err(err) = vectors.build_index() {
        tracing::warn!(error = %err, "vector index shutdown save failed");
    }

    // D7: graceful shutdown under one 10 s bound — the axum drain first
    // (in-flight requests), then the watcher debounce task; the polling
    // backend stops when the watcher drops. The worker and its sweep-tick
    // task stop with the loop / the runtime — no separate abort.
    let shutdown_done = async {
        if serve_result.is_none() {
            serve_result = Some(reap_serve(serve_handle.await));
        }
        if let Some(watcher) = watcher.as_mut() {
            watcher.stop().await;
        }
    };
    // `tokio::time::timeout` builds its `Sleep` at construction time (needs a
    // runtime context), so it is created inside the `block_on` entry.
    match runtime.block_on(async { tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown_done).await }) {
        Ok(()) => tracing::info!("MCP server stopped gracefully"),
        Err(_) => tracing::warn!("forced shutdown after timeout"),
    }

    let serve_result = match serve_result {
        Some(result) => result,
        // A forced shutdown (the bound expired before the serve task drained) is
        // not a failure: the process is about to exit, so the in-flight axum
        // drain is abandoned — that is the point of the forced bound. The warn!
        // above already logged it; returning Ok keeps the exit code clean (a
        // forced stop met the user's intent: the server stopped). The serve task
        // is reaped when the runtime drops at the end of run_serve.
        None => Ok(()),
    };
    if let Err(err) = &serve_result {
        tracing::error!(error = %err, "MCP server error");
    }
    serve_result.map_err(CliError::Io)
}

/// One owner-loop event (module docs: the runtime is entered only for the
/// select that produces it).
enum OwnerEvent {
    /// The axum serve task finished (signal-driven drain done or error);
    /// carries the `JoinHandle` outcome (an axum I/O result, or a worker
    /// failure — normalized by [`reap_serve`]).
    ServeDone(std::result::Result<io::Result<()>, tokio::task::JoinError>),
    /// The stop signal fired (or its channel closed).
    Stop,
    /// A settled watcher batch; `None` once the debounce task ended.
    Batch(Option<Vec<PathBuf>>),
    /// The periodic retry-sweep tick from the spawned timer task.
    Tick,
    /// An on-demand worker cycle (a watcher batch enqueued new jobs).
    RetryBatch,
}

/// Folds the serve task's `JoinHandle` outcome into an I/O result: an axum
/// result passes through, a worker failure (panic) becomes an I/O error so
/// the serve is reported as failed rather than silently lost.
fn reap_serve(raw: std::result::Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
    match raw {
        Ok(result) => result,
        Err(join_err) => Err(io::Error::other(format!("serve task failed: {join_err}"))),
    }
}

/// Runs one event-queue worker cycle inline on the owner thread (the
/// ingestion Runner is `!Send` — the worker module docs): claim due
/// `queue_tasks`, process each one, sweep orphaned data.
fn run_worker_cycle(worker: &DocumentWorker<'_>) {
    if let Err(err) = worker.run_once(now_unix_seconds()) {
        tracing::warn!(error = %err, "event queue worker cycle failed");
    }
}

/// Reconciles every enabled source against the disk through the job queue
/// and returns the accumulated counters: files enqueued for (re)indexing,
/// files enqueued for deletion, and the number of sources whose reconcile
/// failed (each logged with the `context` name). Both the forced-rebuild
/// recovery and the startup reconcile share this loop so their behavior
/// cannot drift (remove-direct-ingest task 1.2).
fn reconcile_enabled_sources(
    job_queue: &DocumentJobQueue<'_>,
    runner: &Runner<'_>,
    global: &Option<GlobalConfig>,
    context: &str,
) -> (usize, usize, usize) {
    let mut enqueued_indexed = 0usize;
    let mut enqueued_deleted = 0usize;
    let mut failed_sources = 0usize;
    if let Some(global) = global {
        for src in global.sources.iter().filter(|src| !src.disabled) {
            match job_queue.reconcile_source(runner, &src.path) {
                Ok(stats) => {
                    enqueued_indexed += stats.indexed;
                    enqueued_deleted += stats.deleted;
                }
                Err(err) => {
                    failed_sources += 1;
                    tracing::warn!(
                        source = %src.path,
                        error = %err,
                        "{context} reconcile failed"
                    );
                }
            }
        }
    }
    (enqueued_indexed, enqueued_deleted, failed_sources)
}

/// Current Unix time in seconds (the worker's claim/backoff clock).
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Drops the stored ANN table and recreates the engine with the configured
/// dimension.
pub(crate) fn recreate_vectors_engine(boot: &mut Bootstrap) -> Result<(), CliError> {
    let index_config = bootstrap::vectors_index_config(&boot.config)?;
    // The ANN index is per-dataset and per-engine:
    // <workspace_dir>/datasets/<name>/state/vectors/<engine> (task 1.5).
    // Only the active engine's subdirectory is dropped.
    let engine_name = boot.config.vectors_config().engine;
    let name = engine_name.as_deref().unwrap_or(ENGINE_USEARCH);
    let base = boot
        .config
        .dataset
        .vectors_path(&boot.config.paths.workspace_dir);
    let engine_path = boot
        .config
        .dataset
        .vectors_engine_path(&boot.config.paths.workspace_dir, name);
    if engine_path.exists() {
        std::fs::remove_dir_all(&engine_path)?;
    }
    // The WAL database (usearch-wal-persistence task 3.9, ADR 0004 §3):
    // the knowledge.db path — the recreated engine journals into the same
    // `usearch_vectors_log` table (the rebuild clears it).
    let wal_db = boot
        .config
        .dataset
        .db_path(&boot.config.paths.workspace_dir);
    // The factory performs the (now guaranteed) create at the engine-tagged
    // path and dispatches to the compiled engine.
    let engine = create_vector_engine(name, &base, &index_config, Some(wal_db.as_path()))?;
    tracing::info!(
        path = %engine_path.display(),
        dim = index_config.dim,
        "vector index recreated"
    );
    boot.vectors = Some(engine);
    boot.dimension_mismatch = None;
    Ok(())
}
