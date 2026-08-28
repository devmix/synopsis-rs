//! MCP mount and graceful shutdown (design D4 + D7).
//!
//! Oracle mapping: `../synopsis/cmd/app/serve.go` (`runServe`): bootstrap →
//! port override → dimension-mismatch handling → startup health check →
//! runner → initial sync → scheduler → graph load → searcher → MCP server →
//! file watcher → signal wait → graceful shutdown.
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
//!    runtime from within a runtime"; `vectors::LanceEngine` module docs:
//!    sync contexts and `spawn_blocking` workers only).
//!
//! The serve flow is therefore a *synchronous* function on the main
//! thread:
//!
//! - all Lance-touching work (engine open/create/recreate, initial sync,
//!   watcher-batch re-index, orphan cleanup) runs inline on the main
//!   thread *between* runtime-context entries;
//! - the runtime is entered only for short async bits that require it:
//!   creating the watcher debounce task, creating/starting the scheduler,
//!   binding the listener, and the owner-loop event select (`select!` over
//!   the serve task, the stop signal, the watcher batches, and the
//!   cleanup token — fresh futures per iteration);
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
//! server (design D8 re-architecture of the oracle's `SetGraph` mutation;
//! see [`mcp::Server::set_searcher`] / [`mcp::Server::set_graph`]).
//!
//! # Dimension-mismatch rebuild
//!
//! In the Rust codebase the mismatch surfaces from the ANN engine at open
//! time, so the auto-rebuild drops the stored Lance table and recreates
//! the engine, then forces a full re-ingest (the Rust form of the oracle's
//! `DropVectorTable` + `ReEmbedChunks`).
//!
//! # Shutdown
//!
//! SIGINT/SIGTERM (design D7) are installed as a spawned task (the tokio
//! signal API needs a runtime handle) and feed a broadcast the owner loop
//! selects on. axum stops accepting and drains in-flight requests; the
//! serve task, the scheduler, and the watcher debounce task all stop under
//! one 10 s bound (oracle `shutdownCtx`).
//!
//! # Deviation
//!
//! A serve error exits non-zero; the oracle logged the server error and
//! exited 0 (a bug — a bind failure must not look like success).

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use config::preset::{GraphConfig, SearchConfig};
use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, FactDao};
use embedding::EmbeddingProvider;
use graph::GraphIndex;
use ingestion::{Runner, RunnerParams};
use search::{
    Enricher, GraphExpander, HybridSearcher, LexicalSearcher, Reranker, SearchError, SearchResult,
    Searcher, SemanticSearcher,
};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc};
use vectors::{LanceEngine, VectorIndex, VectorsError};

use crate::error::CliError;
use crate::serve::bootstrap::{self, Bootstrap};
use crate::serve::health::run_health_check;
use crate::serve::ingest;
use crate::serve::scheduler::Scheduler;
use crate::serve::watcher::{ChangeHandler, IngestChangeHandler, Watcher};

/// Graceful-shutdown bound (oracle `shutdownCtx`: 10 s).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// One `serve` invocation: the effective config path plus the per-command
/// flags (clap `serve` subcommand).
pub struct ServeRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// `--db` database path override.
    pub db_path: Option<PathBuf>,
    /// `--no-initial-sync`: skip the full source scan on startup.
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
    /// call this from plain threads — both safe contexts for the Lance
    /// facade (module docs).
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
    let mut boot = bootstrap::bootstrap(&req.cfg_path, req.db_path.as_deref())?;
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
/// runner + dimension-mismatch handling → initial sync → scheduler → graph
/// load → searcher + MCP server → file watcher → bind → owner loop →
/// graceful shutdown under the 10 s bound.
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

    // Startup health check (log-only; the oracle continues on errors).
    if let Err(err) = run_health_check(&boot.db, boot.embed.as_ref(), &boot.config) {
        tracing::warn!(error = %err, "startup health check failed");
    }

    // Collaborator extraction (D4): everything the rest of the flow needs
    // is cloned into owned locals before the ingestion `Runner` is
    // constructed. The `Runner` borrows these locals (not `boot`), so the
    // bootstrap state is no longer pinned by its borrow for the whole
    // owner loop — the Rust form of the oracle's single shared struct.
    let db = boot.db.clone();
    let cache = boot.cache.clone();
    let embed = boot.embed.clone();
    let config = boot.config.clone();
    let global = boot.global.clone();
    let domains = boot.domains.clone();

    // Runner assembly + vector dimension-mismatch handling (D4). The
    // mismatch surfaces from the ANN engine at open time; auto-rebuild
    // recreates the engine and forces a full re-ingest below (the Rust form
    // of the oracle's `rebuildVectorsIfNeeded` → `ReEmbedChunks`). The
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

    // Initial sync: scan all sources so the index is up to date before the
    // server accepts requests. Forced after a vector rebuild (re-embed).
    let initial_sync_due = auto_update.initial_sync && !req.no_initial_sync;
    if initial_sync_due || force_rebuild {
        tracing::info!("initial sync started");
        let stats = ingest::ingest_all(&runner, force_rebuild);
        if !stats.errors.is_empty() {
            tracing::warn!(
                errors = stats.errors.len(),
                "initial sync completed with errors"
            );
        }
        tracing::info!(
            sources = stats.sources_processed,
            documents_created = stats.documents_created,
            documents_updated = stats.documents_updated,
            documents_skipped = stats.documents_skipped,
            "initial sync finished"
        );
    }

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
    // watcher (oracle parity).
    let mut watcher: Option<Watcher> = None;
    let mut handler: Option<IngestChangeHandler<'_>> = None;
    if auto_update.enabled && auto_update.watch_sources {
        let debounce = Duration::from_secs(auto_update.debounce_seconds.max(0) as u64);
        match runtime.block_on(async { Watcher::from_config(debounce, &config, &registry) }) {
            Ok(new_watcher) => {
                let new_handler = IngestChangeHandler::new(&runner, &db, &config.graph, Some(hook));
                watcher = Some(new_watcher);
                handler = Some(new_handler);
            }
            Err(err) => tracing::warn!(
                error = %err,
                "file watcher setup failed; auto-update disabled"
            ),
        }
    }

    // D6: periodic orphan_cleanup. The job body only hands a token to the
    // owner loop (the Runner is `!Send` — scheduler module docs). The spare
    // sender keeps the channel open while the job is inactive.
    let (cleanup_tx, mut cleanup_rx) = mpsc::unbounded_channel::<()>();
    let _cleanup_sender_spare = cleanup_tx.clone();
    let mut scheduler = match runtime.block_on(async {
        Scheduler::new(&config.scheduler, move || {
            let _ = cleanup_tx.send(());
        })
        .await
    }) {
        Ok(scheduler) => {
            if let Err(err) = runtime.block_on(scheduler.start()) {
                tracing::warn!(error = %err, "start scheduler");
            }
            Some(scheduler)
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "create scheduler; orphan_cleanup disabled"
            );
            None
        }
    };

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

    // Owner loop: the watcher callback and the orphan-cleanup job run
    // inline on the main thread (no runtime context — the Lance facade is
    // safe here); the runtime is entered only for the event select. Fresh
    // futures per iteration keep each select self-contained.
    let mut serve_result: Option<io::Result<()>> = None;
    'owner: loop {
        let event = match (watcher.as_mut(), handler.as_ref()) {
            (Some(watcher), Some(_handler)) => runtime.block_on(async {
                tokio::select! {
                    result = &mut serve_handle => OwnerEvent::ServeDone(result),
                    _ = stop.recv() => OwnerEvent::Stop,
                    batch = watcher.next_batch() => OwnerEvent::Batch(batch),
                    _ = cleanup_rx.recv() => OwnerEvent::Cleanup,
                }
            }),
            _ => runtime.block_on(async {
                tokio::select! {
                    result = &mut serve_handle => OwnerEvent::ServeDone(result),
                    _ = stop.recv() => OwnerEvent::Stop,
                    _ = cleanup_rx.recv() => OwnerEvent::Cleanup,
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
            // bounded shutdown below reaps the serve task.
            OwnerEvent::Stop | OwnerEvent::Batch(None) => break 'owner,
            OwnerEvent::Batch(Some(paths)) => {
                // A batch can only arrive from the watcher arm, so the
                // handler exists there.
                if let Some(handler) = handler.as_ref() {
                    handler.handle_changes(&paths);
                }
            }
            OwnerEvent::Cleanup => run_orphan_cleanup(&runner),
        }
    }

    // D7: graceful shutdown under one 10 s bound — the axum drain first
    // (in-flight requests), then the scheduler (waits for a running job),
    // then the watcher debounce task; the polling backend stops when the
    // watcher drops.
    let shutdown_done = async {
        if serve_result.is_none() {
            serve_result = Some(reap_serve(serve_handle.await));
        }
        if let Some(scheduler) = scheduler.as_mut() {
            match scheduler.shutdown().await {
                Ok(()) => tracing::info!("scheduler stopped gracefully"),
                Err(err) => tracing::warn!(error = %err, "scheduler shutdown"),
            }
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
        // The shutdown bound expired before the serve task drained; the
        // task is reaped when the runtime drops at the end of run_serve.
        None => Err(io::Error::other(
            "serve task did not finish within the shutdown bound",
        )),
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
    /// The periodic orphan-cleanup token from the scheduler.
    Cleanup,
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

/// Runs the periodic orphan cleanup inline on the owner loop (the job body
/// can only hand off — the ingestion Runner is `!Send`).
fn run_orphan_cleanup(runner: &Runner<'_>) {
    if let Err(err) = ingest::cleanup_orphaned_data(runner) {
        tracing::warn!(error = %err, "orphan cleanup run failed");
    }
}

/// Drops the stored ANN table and recreates the engine with the configured
/// dimension (the Rust form of the oracle's `DropVectorTable` +
/// `InitVectorTable` inside `ReEmbedChunks`).
///
/// Shared with the `sync` subcommand (design D8), which takes the same
/// recreate path on a dimension mismatch (`--rebuild` resets everything
/// anyway).
pub(crate) fn recreate_vectors_engine(boot: &mut Bootstrap) -> Result<(), CliError> {
    let index_config = bootstrap::vectors_index_config(&boot.config)?;
    let path = Path::new(&boot.config.paths.workspace_dir);
    // The engine stores its table under `<workspace_dir>/vectors.lance` (Lance
    // layout); it must be dropped before the engine can be recreated with
    // the new schema.
    let table_dir = path.join("vectors.lance");
    if table_dir.exists() {
        std::fs::remove_dir_all(&table_dir)?;
    }
    let engine = LanceEngine::create(path, index_config)?;
    tracing::info!(
        path = %path.display(),
        dim = index_config.dim,
        "vector index recreated"
    );
    boot.vectors = Some(Arc::new(engine));
    boot.dimension_mismatch = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use config::{Config, OnnxConfig};
    use db::{ChunkDao, ConnectionOrTx, DocumentDao};
    use embedding::{EmbeddingError, EmbeddingProvider};
    use graph::GraphIndex;
    use search::Searcher;
    use tokio::sync::broadcast;
    use vectors::{VectorIndex, VectorIndexConfig, VectorsError};

    use super::*;
    use crate::serve::bootstrap::{Bootstrap, open_db};

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
    fn test_config(data_dir: &Path) -> Config {
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
                workspace_dir: data_dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.ingestion.ner.disabled = true;
        config.apply_defaults();
        config
    }

    /// A Bootstrap with a fake provider and a temp-file db; no sources
    /// (global `None`) — the task's "no real sources" shape.
    fn test_bootstrap(dir: &TempDir) -> Bootstrap {
        let config = test_config(&dir.as_ref().join("data"));
        let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
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
    /// runtime context — the Lance facade is safe here, mirroring
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
            db_path: None,
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

    // --- serve_with_stop: dimension-mismatch auto-rebuild ----------------------

    /// A stored index with a different dimension + `--auto-rebuild-vectors`:
    /// the engine is recreated with the configured dimension and the serve
    /// completes (the forced re-ingest is a no-op with no sources).
    #[test]
    fn serve_rebuilds_vectors_on_dimension_mismatch() {
        let dir = TempDir::new("dim-rebuild");
        let data_dir = dir.as_ref().join("data");
        // Pre-create the stored index with a different dimension.
        let stored = VectorIndexConfig::new(8, 16, 100, 256, 32, 200).expect("index config");
        LanceEngine::create(&data_dir, stored).expect("create stored index");

        let port = free_port();
        let mut boot = test_bootstrap(&dir); // 4-dim embedding vs 8-dim index
        let req = ServeRequest {
            cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
            db_path: None,
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

    // --- serve_with_stop: mismatch without auto-rebuild is fatal -----------------

    #[test]
    fn serve_mismatch_without_auto_rebuild_is_fatal() {
        let dir = TempDir::new("dim-fatal");
        let data_dir = dir.as_ref().join("data");
        let stored = VectorIndexConfig::new(8, 16, 100, 256, 32, 200).expect("index config");
        LanceEngine::create(&data_dir, stored).expect("create stored index");

        let mut boot = test_bootstrap(&dir);
        let req = ServeRequest {
            cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
            db_path: None,
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
}
