//! File watcher with debounce (design D5) — the port of the oracle's
//! `setupFileWatcher` (`../synopsis/cmd/app/serve.go`) and
//! `internal/watcher/filewatcher.go`.
//!
//! A [`notify::PollWatcher`] backend feeds changed file paths into a
//! debounced tokio loop; once a quiet period of
//! `config.auto_update.debounce_seconds` has elapsed, the accumulated batch is
//! delivered to the caller through [`Watcher::next_batch`]. The production
//! callback ([`IngestChangeHandler`]) is a producer (document-jobs-queue task
//! 1.5): it enqueues one `document_jobs` row per changed file through the
//! [`DocumentJobQueue`] — `index` (fresh content hash) for a file present on
//! disk, `delete` for a removed one — and, when the graph is enabled, reloads
//! the knowledge graph and hands the fresh index to the injected
//! `on_graph_reload` hook. The background worker runs the ingestion pipeline
//! later; the handler never ingests directly (state flows through the
//! `document_jobs` table only).
//!
//! Architectural notes (functional copy, not a code copy):
//! - **Threading.** The ingestion [`Runner`] borrows the source
//!   [`ingestion::Registry`], whose `Box<dyn Source>` is `!Send + !Sync`, so
//!   the `Runner` is `!Send + !Sync`. A spawned tokio task must be `Send`, so
//!   the debounce loop can only deal in [`PathBuf`]s: it coalesces events into
//!   batches and hands them out over a channel. The *caller* (the serve loop,
//!   task 1.6) invokes the [`ChangeHandler`] with each batch on the thread
//!   that owns the `Runner`. The oracle ran the callback on the watcher
//!   goroutine against a shared runner pointer; the Rust re-architecture keeps
//!   the `!Send` collaborators on their owner thread instead.
//! - The oracle debounced with a `debounce/2` ticker over fsnotify events;
//!   here the debounce lives in [`Debouncer`] (trailing quiet period, same two
//!   timing rules) driven by the tokio loop, with the clock injected for
//!   deterministic unit tests.
//! - The oracle's manual sub-directory re-`Add` on directory-create events is
//!   unnecessary: `PollWatcher` re-walks every watched tree on each poll.
//! - The oracle filtered events to a hardcoded extension list; here the
//!   filter is derived from the registered parsers
//!   ([`ingestion::Registry::supported_extensions`]), so it cannot drift
//!   from what the pipeline actually ingests.
//! - The oracle's `SetGraph` swap on the searcher + MCP server has no Rust
//!   counterpart (the search crate is immutable by design — the CLI rebuilds
//!   the searcher, search design D8). The `on_graph_reload` hook seam stands
//!   in; the serve wiring (task 1.6) consumes it.
//! - The watcher runs entirely on a background task, so it never blocks
//!   startup (acceptance criterion).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::preset::GraphConfig;
use config::{Config, load_global_config};
use db::Db;
use graph::GraphIndex;
use ingestion::ingester::compute_content_hash;
use ingestion::{DocumentJobQueue, Registry, Runner};
use notify::Watcher as _;
use notify::{Event, EventKind, PollWatcher, RecursiveMode};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Polling interval of the notify backend. notify's default is 30 s — too
/// sluggish for interactive re-indexing on a laptop (the oracle's fsnotify
/// backend was instant); 2 s keeps CPU cost negligible while feeling live.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Graceful-shutdown bound (oracle `FileWatcher.Stop`: 5 s).
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors surfaced while building or stopping a [`Watcher`].
#[derive(Debug, Error)]
pub enum WatcherError {
    /// The notify backend failed to start or watch a path.
    #[error("notify backend: {0}")]
    Notify(#[from] notify::Error),
    /// The global ontology could not be loaded.
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    /// A source path could not be resolved.
    #[error("source path: {0}")]
    Io(#[from] std::io::Error),
    /// A configured source path is not an existing directory.
    #[error("source path is not a directory: {path}")]
    SourceNotADirectory {
        /// The configured path that is not an existing directory.
        path: PathBuf,
    },
}

/// Processes one debounced batch of changed paths (design D5 callback).
///
/// Deliberately `!Send + !Sync`: the trait carries no `Send`/`Sync` bound
/// because the production [`IngestChangeHandler`] borrows the `!Send`
/// [`Runner`] and must run on the thread that owns the ingestion
/// collaborators — never inside the watcher's debounce task (a spawned task
/// must be `Send`). The serve wiring (task 1.6) invokes it on that owner
/// thread. Tests substitute the collaborators through this seam (the task's
/// "trait/closure" hook).
pub trait ChangeHandler {
    /// Enqueues a `document_jobs` row per changed file (index for a file
    /// present on disk, delete for a removed one) and — when the graph is
    /// enabled — reloads the graph index.
    fn handle_changes(&self, paths: &[PathBuf]);
}

/// Closure injection (oracle parity: the callback was a plain function).
impl<F> ChangeHandler for F
where
    F: Fn(&[PathBuf]),
{
    fn handle_changes(&self, paths: &[PathBuf]) {
        (self)(paths);
    }
}

/// Production [`ChangeHandler`] (design D5): the oracle `setupFileWatcher`
/// callback body, re-architected as a queue producer (document-jobs-queue
/// task 1.5).
///
/// `on_graph_reload` receives the freshly reloaded graph index after a
/// successful reload; the serve wiring (task 1.6) uses it to rebuild the
/// immutable searcher + MCP server (the Rust re-architecture has no
/// `SetGraph`-style swap — search crate design D8).
pub struct IngestChangeHandler<'a> {
    runner: &'a Runner<'a>,
    db: &'a Db,
    graph_cfg: &'a GraphConfig,
    on_graph_reload: Option<Arc<dyn Fn(Arc<GraphIndex>) + Send + Sync>>,
    /// The `document_jobs` producer: one row per changed file, processed by
    /// the background worker (the handler never ingests directly).
    job_queue: &'a DocumentJobQueue<'a>,
}

impl<'a> IngestChangeHandler<'a> {
    /// Wraps the collaborators of one serve session.
    #[must_use]
    pub fn new(
        runner: &'a Runner<'a>,
        db: &'a Db,
        graph_cfg: &'a GraphConfig,
        on_graph_reload: Option<Arc<dyn Fn(Arc<GraphIndex>) + Send + Sync>>,
        job_queue: &'a DocumentJobQueue<'a>,
    ) -> Self {
        Self {
            runner,
            db,
            graph_cfg,
            on_graph_reload,
            job_queue,
        }
    }
}

impl ChangeHandler for IngestChangeHandler<'_> {
    fn handle_changes(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        tracing::info!(files_changed = paths.len(), "auto-update triggered");

        // Producer (document-jobs-queue task 1.5): one job per changed file,
        // resolved against the configured sources. A file present on disk is
        // queued for (re)indexing with its fresh content hash (the pipeline's
        // dedup key); a removed file is queued for deletion. The background
        // worker runs the pipeline later — no direct ingestion here (state
        // flows through `document_jobs` only). The debouncer already dedupes
        // paths within a batch, so no per-source dedup map is needed.
        for path in paths {
            let path_str = path.to_string_lossy();
            let Some(source) = self.runner.find_source_for_path(&path_str) else {
                tracing::info!(path = %path.display(), "changed file not in any configured source");
                continue;
            };
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    let hash = compute_content_hash(&content);
                    if let Err(err) =
                        self.job_queue
                            .enqueue_index(&path_str, &source.path, Some(&hash))
                    {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "enqueue index job failed"
                        );
                    }
                }
                Err(read_err) if read_err.kind() == std::io::ErrorKind::NotFound => {
                    if let Err(err) = self.job_queue.enqueue_delete(&path_str) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "enqueue delete job failed"
                        );
                    }
                }
                Err(read_err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %read_err,
                        "changed file unreadable; no job enqueued"
                    );
                }
            }
        }

        // Refresh the in-memory knowledge graph so search stays current.
        if self.graph_cfg.enable_graph {
            match GraphIndex::from_db(self.db, self.graph_cfg) {
                Ok(graph) if graph.is_available() => {
                    tracing::info!("knowledge graph reloaded");
                    if let Some(reload) = &self.on_graph_reload {
                        reload(Arc::new(graph));
                    }
                }
                Ok(_) => {
                    // `load_on_startup=false`: the config models the index as
                    // Unavailable — swapping it in would only downgrade the
                    // running server (deviation from the oracle, which would
                    // load anyway).
                }
                Err(err) => tracing::warn!(error = %err, "graph reload failed"),
            }
        }
    }
}

/// Trailing debounce over a batch of changed paths (port of the oracle's
/// `flushPending` timing rules).
///
/// The clock is a [`Duration`] (not an [`std::time::Instant`]) so unit tests
/// can advance it deterministically; production feeds it from
/// `Instant::now().elapsed()`.
struct Debouncer {
    debounce: Duration,
    pending: BTreeSet<PathBuf>,
    last_change: Option<Duration>,
    last_run: Duration,
}

impl Debouncer {
    /// A debouncer with a quiet period of `debounce`.
    #[must_use]
    fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            pending: BTreeSet::new(),
            last_change: None,
            last_run: Duration::ZERO,
        }
    }

    /// Records a change at `now` and slides the quiet window.
    fn note(&mut self, path: PathBuf, now: Duration) {
        self.pending.insert(path);
        self.last_change = Some(now);
    }

    /// Whether the batch is ready to flush at `now`: the quiet period after
    /// the newest change has elapsed AND at least `debounce` has passed since
    /// the last flush (oracle parity; the second rule never binds under
    /// trailing semantics and is kept as defensive parity).
    fn ready(&self, now: Duration) -> bool {
        match self.last_change {
            Some(last) => {
                now.saturating_sub(last) >= self.debounce
                    && now.saturating_sub(self.last_run) >= self.debounce
            }
            None => false,
        }
    }

    /// Consumes the pending batch (empty when nothing is pending or the quiet
    /// period has not elapsed).
    fn flush(&mut self, now: Duration) -> Vec<PathBuf> {
        if !self.ready(now) {
            return Vec::new();
        }
        self.last_run = now;
        self.last_change = None;
        self.pending.iter().cloned().collect()
    }
}

/// Debounced file watcher over the configured source directories (design D5).
///
/// The watcher owns the notify backend, a stop channel and the debounce task.
/// The debounce task coalesces events into batches and delivers each through
/// [`next_batch`]; the caller invokes a [`ChangeHandler`] with the batch.
/// [`stop`] performs a graceful shutdown; dropping the watcher (early-return
/// paths) aborts the task as a safety net.
pub struct Watcher {
    /// Keeps the notify backend (and its polling thread) alive; dropping the
    /// watcher stops the poll loop (oracle `fw.watcher.Close()`). Never read —
    /// retained purely for its drop side-effect.
    #[allow(dead_code)]
    poll: PollWatcher,
    stop_tx: watch::Sender<()>,
    task: JoinHandle<()>,
    batches: mpsc::UnboundedReceiver<Vec<PathBuf>>,
}

impl Watcher {
    /// Starts the watcher over `sources` with a trailing `debounce`.
    ///
    /// `extensions` is the accept list of file extensions, taken from the
    /// registered parsers (e.g.
    /// [`Registry::supported_extensions`](ingestion::Registry::supported_extensions));
    /// it is normalized (leading dot stripped, lowercased) before filtering,
    /// mirroring the parsers' own case-insensitive accepts checks.
    ///
    /// Must be called from within a tokio runtime (the debounce loop is a
    /// spawned task). Every source directory is watched recursively.
    ///
    /// # Errors
    ///
    /// [`WatcherError::Notify`] when the backend cannot start or watch a path.
    pub fn new(
        debounce: Duration,
        sources: Vec<PathBuf>,
        extensions: impl IntoIterator<Item = String>,
    ) -> Result<Self, WatcherError> {
        let wanted = normalize_extensions(extensions);
        let extension_count = wanted.len();
        if extension_count == 0 {
            tracing::warn!(
                "no supported extensions from the parser registry: no file will be re-indexed"
            );
        }
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let mut poll = PollWatcher::new(
            move |res: notify::Result<Event>| {
                let Ok(event) = res else {
                    tracing::warn!("file watcher error: {res:?}");
                    return;
                };
                if !relevant_kind(&event.kind) {
                    return;
                }
                for path in &event.paths {
                    if wanted_extension(path, &wanted) {
                        let _ = events_tx.send(path.clone());
                    }
                }
            },
            notify::Config::default().with_poll_interval(POLL_INTERVAL),
        )?;
        for source in &sources {
            poll.watch(source, RecursiveMode::Recursive)?;
        }
        let (stop_tx, stop_rx) = watch::channel(());
        let task = tokio::spawn(debounce_loop(events_rx, stop_rx, batch_tx, debounce));
        tracing::info!(
            sources = sources.len(),
            extensions = extension_count,
            debounce_ms = debounce.as_millis() as u64,
            "file watcher started"
        );
        Ok(Self {
            poll,
            stop_tx,
            task,
            batches: batch_rx,
        })
    }

    /// Watcher over every enabled source of the global ontology (design D5):
    /// `<workspace_dir>/datasets/<name>/ontology` → `global.xml` → non-disabled
    /// sources, with a trailing `debounce`. The extension accept list is
    /// derived from the parsers registered in `registry`
    /// ([`Registry::supported_extensions`](ingestion::Registry::supported_extensions)).
    ///
    /// # Errors
    ///
    /// [`WatcherError::Config`] when the global ontology cannot be loaded,
    /// [`WatcherError::Io`] / [`WatcherError::SourceNotADirectory`] for a
    /// source path that cannot be resolved or is not a directory,
    /// [`WatcherError::Notify`] when the backend cannot start.
    pub fn from_config(
        debounce: Duration,
        config: &Config,
        registry: &Registry,
    ) -> Result<Self, WatcherError> {
        let sources = watchable_sources(config)?;
        Self::new(debounce, sources, registry.supported_extensions())
    }

    /// Receives the next settled batch of changed paths.
    ///
    /// Returns `None` once the debounce task has finished (after [`stop`] or
    /// a backend drop) and no batch is in flight.
    pub async fn next_batch(&mut self) -> Option<Vec<PathBuf>> {
        self.batches.recv().await
    }

    /// Graceful shutdown (oracle `FileWatcher.Stop`): signals the debounce
    /// loop and waits up to [`STOP_TIMEOUT`] for it to finish (aborting on
    /// timeout). The polling backend keeps running until the watcher is
    /// dropped ([`Drop`]), so the serve wiring drops the watcher after
    /// calling this.
    pub async fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if tokio::time::timeout(STOP_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

impl Drop for Watcher {
    /// Safety net for call sites that drop the watcher without [`stop`]
    /// (early-return paths): the debounce loop is aborted and the polling
    /// thread exits with the backend drop.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The debounce loop (design D5): coalesces the event stream into quiet-period
/// batches and hands each to `batch_tx`.
async fn debounce_loop(
    mut events_rx: mpsc::UnboundedReceiver<PathBuf>,
    mut stop_rx: watch::Receiver<()>,
    batch_tx: mpsc::UnboundedSender<Vec<PathBuf>>,
    debounce: Duration,
) {
    let start = std::time::Instant::now();
    let mut d = Debouncer::new(debounce);
    loop {
        let now = start.elapsed();
        if d.ready(now) {
            let batch = d.flush(now);
            if !batch.is_empty() {
                let _ = batch_tx.send(batch);
            }
            continue;
        }
        // Sleep until the quiet period can possibly end; park forever while
        // idle (nothing pending) so the select waits purely on event/stop.
        let wait = d
            .last_change
            .map(|last| (last + debounce).saturating_sub(now));
        tokio::select! {
            maybe = events_rx.recv() => {
                let Some(path) = maybe else { break };
                d.note(path, start.elapsed());
            }
            _ = stop_rx.changed() => break,
            _ = wait_timer(wait) => {}
        }
    }
}

/// Sleeps `wait`, or parks forever when `None` (idle: no pending batch).
async fn wait_timer(wait: Option<Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending::<()>().await,
    }
}

/// The watch list (oracle `setupFileWatcher` source loop): every non-disabled
/// source of the global ontology, resolved to an absolute path and verified
/// to be an existing directory.
fn watchable_sources(config: &Config) -> Result<Vec<PathBuf>, WatcherError> {
    // The ontology directory is per-dataset: <workspace_dir>/datasets/<name>/ontology.
    let Some(global) =
        load_global_config(config.dataset.ontology_path(&config.paths.workspace_dir))?
    else {
        tracing::info!("no global ontology: nothing to watch");
        return Ok(Vec::new());
    };
    let mut sources = Vec::new();
    for source in &global.sources {
        if source.disabled {
            continue;
        }
        let path = abs_path(&source.path)?;
        if !std::fs::metadata(&path)?.is_dir() {
            return Err(WatcherError::SourceNotADirectory { path });
        }
        tracing::info!(
            path = %path.display(),
            source_type = ?source.source_type,
            "watching source directory"
        );
        sources.push(path);
    }
    Ok(sources)
}

/// Resolves `path` against the current directory and normalizes it lexically
/// (oracle `filepath.Abs`).
fn abs_path(path: &str) -> Result<PathBuf, WatcherError> {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(joined.components().collect())
}

/// Whether an event kind can change ingestable content (oracle parity:
/// create, modify, remove; access/other/any are ignored).
fn relevant_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Normalizes parser-reported extensions (leading dot, e.g. `".md"`) to the
/// lowercase dotless form used by [`wanted_extension`].
fn normalize_extensions(extensions: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    extensions
        .into_iter()
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect()
}

/// Whether the path has an extension one of the registered parsers handles
/// (case-insensitive, matching the parsers' own accepts checks).
fn wanted_extension(path: &Path, wanted: &BTreeSet<String>) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| wanted.contains(&ext.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use config::DomainConfig;
    use config::ontology::{GlobalConfig, GlobalNerConfig, NerMethod, SourceConfig, SourceType};
    use config::preset::{GraphConfig, IngestionConfig, LinkerConfig};
    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, DocumentDao, DocumentJobDao};
    use embedding::{EmbeddingError, EmbeddingProvider};
    use ingestion::{
        DocumentJobQueue, JsonChunker, JsonSource, MarkdownChunker, MarkdownSource,
        MediawikiChunker, MediawikiSource, NerPrompts, Registry, Runner, RunnerParams,
        UnstructuredSource, WebpageSource, ingester::compute_content_hash, load_ner_prompts,
    };
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind};
    use vectors::{VectorIndex, VectorsError};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-watcher-{tag}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// A child directory, created.
        fn sub(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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

    /// In-memory [`VectorIndex`] stub (records rows, no search).
    struct MemIndex {
        rows: Mutex<BTreeMap<u32, Vec<f32>>>,
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

        fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            Ok(Vec::new())
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

    /// Two-collaborator fixture: in-memory DB, fake embed, memory index and a
    /// markdown-source registry over a caller-supplied global ontology.
    struct Fixture {
        db: db::Db,
        cfg: IngestionConfig,
        global: GlobalConfig,
        domains: HashMap<String, DomainConfig>,
        registry: Registry,
        embed: Arc<FakeEmbed>,
        index: MemIndex,
        prompts: NerPrompts,
        linker: LinkerConfig,
    }

    impl Fixture {
        fn new(global: GlobalConfig) -> Self {
            let chunking = IngestionConfig::default().chunking.clone();
            let mut registry = Registry::new();
            registry
                .register(
                    MarkdownSource::SOURCE_TYPE,
                    Box::new(MarkdownSource::new(Box::new(MarkdownChunker::new(
                        chunking.markdown,
                    )))),
                )
                .unwrap();
            Self {
                db: in_memory_db(),
                cfg: IngestionConfig::default(),
                global,
                domains: HashMap::new(),
                registry,
                embed: Arc::new(FakeEmbed { dim: 4 }),
                index: MemIndex {
                    rows: Mutex::new(BTreeMap::new()),
                },
                prompts: load_ner_prompts("/nonexistent-ner-prompts").unwrap(),
                linker: LinkerConfig::default(),
            }
        }

        fn runner(&self) -> Runner<'_> {
            Runner::new(RunnerParams {
                db: &self.db,
                ingest_cfg: &self.cfg,
                global: Some(&self.global),
                domains: &self.domains,
                registry: &self.registry,
                embed: self.embed.as_ref(),
                vectors: &self.index,
                prompts: &self.prompts,
                linker_cfg: &self.linker,
                prompts_path: "/nonexistent-prompts",
                llm_cache: None,
            })
        }
    }

    fn source_config(path: &str, disabled: bool) -> SourceConfig {
        SourceConfig {
            path: path.to_owned(),
            source_type: SourceType::Markdown,
            disabled,
            space: String::new(),
            domains: vec!["default".to_owned()],
            dataset: String::new(),
        }
    }

    fn global_config(sources: Vec<SourceConfig>) -> GlobalConfig {
        GlobalConfig {
            sources,
            cross_domain_links: None,
            ner: GlobalNerConfig {
                methods: vec![NerMethod::Regex],
            },
            entities: Vec::new(),
            relations: Vec::new(),
            extraction: Default::default(),
        }
    }

    fn doc_count(db: &db::Db) -> usize {
        doc_paths(db).len()
    }

    /// All `document_jobs` rows (the producer's output).
    fn job_rows(db: &db::Db) -> Vec<db::DocumentJob> {
        db.with_conn(|conn| DocumentJobDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
            .unwrap()
            .unwrap()
    }

    fn doc_paths(db: &db::Db) -> Vec<String> {
        db.with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|d| d.original_path)
            .collect()
    }

    // --- Debouncer (deterministic clock) -----------------------------------

    #[test]
    fn debounce_requires_quiet_period_after_last_change() {
        let mut d = Debouncer::new(Duration::from_secs(30));
        d.note(PathBuf::from("/a/1.md"), Duration::ZERO);
        assert!(!d.ready(Duration::from_secs(29)), "quiet period not over");
        assert!(d.ready(Duration::from_secs(30)), "quiet period over");
    }

    #[test]
    fn debounce_coalesces_bursts_and_dedupes_paths() {
        let mut d = Debouncer::new(Duration::from_secs(30));
        d.note(PathBuf::from("/a/1.md"), Duration::ZERO);
        d.note(PathBuf::from("/a/1.md"), Duration::from_secs(5)); // same path again
        d.note(PathBuf::from("/b/2.json"), Duration::from_secs(10));
        // The trailing window slides with the newest change (t=10).
        assert!(!d.ready(Duration::from_secs(39)), "window slid to t=10");
        assert!(d.ready(Duration::from_secs(40)));
        let paths = d.flush(Duration::from_secs(40));
        assert_eq!(
            paths,
            vec![PathBuf::from("/a/1.md"), PathBuf::from("/b/2.json")]
        );
        // The flushed batch is consumed.
        assert!(!d.ready(Duration::from_secs(40)));
        assert!(d.flush(Duration::from_secs(100)).is_empty());
    }

    #[test]
    fn debounce_enforces_min_interval_since_last_flush() {
        let mut d = Debouncer::new(Duration::from_secs(30));
        d.note(PathBuf::from("/a/1.md"), Duration::ZERO);
        assert_eq!(d.flush(Duration::from_secs(30)).len(), 1);
        // A change right after the flush: the quiet window ends at t=61 and
        // the last-flush rule (t >= 30 + 30) is satisfied by it.
        d.note(PathBuf::from("/a/2.md"), Duration::from_secs(31));
        assert!(
            !d.ready(Duration::from_secs(60)),
            "quiet window ends at t=61"
        );
        assert!(d.ready(Duration::from_secs(61)));
    }

    // --- Event / extension filter ------------------------------------------

    /// The production registry shape (`bootstrap::build_registry`): all five
    /// source types over the default chunking config.
    fn full_registry() -> Registry {
        let chunking = IngestionConfig::default().chunking.clone();
        let mut registry = Registry::new();
        registry
            .register(
                MarkdownSource::SOURCE_TYPE,
                Box::new(MarkdownSource::new(Box::new(MarkdownChunker::new(
                    chunking.markdown.clone(),
                )))),
            )
            .unwrap();
        registry
            .register(
                JsonSource::SOURCE_TYPE,
                Box::new(JsonSource::new(Box::new(JsonChunker::new(
                    chunking.json.clone(),
                )))),
            )
            .unwrap();
        registry
            .register(
                MediawikiSource::SOURCE_TYPE,
                Box::new(MediawikiSource::new(Box::new(MediawikiChunker::new(
                    chunking.markdown.clone(),
                )))),
            )
            .unwrap();
        registry
            .register(
                WebpageSource::SOURCE_TYPE,
                Box::new(WebpageSource::new(Box::new(MarkdownChunker::new(
                    chunking.markdown.clone(),
                )))),
            )
            .unwrap();
        registry
            .register(
                UnstructuredSource::SOURCE_TYPE,
                Box::new(UnstructuredSource::new(
                    Box::new(MarkdownChunker::new(chunking.markdown)),
                    Box::new(JsonChunker::new(chunking.json)),
                )),
            )
            .unwrap();
        registry
    }

    #[test]
    fn event_filter_keeps_ingestable_changes_only() {
        let relevant = [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            EventKind::Remove(RemoveKind::File),
        ];
        for kind in &relevant {
            assert!(relevant_kind(kind), "{kind:?} must be relevant");
        }
        let ignored = [
            EventKind::Access(AccessKind::Read),
            EventKind::Other,
            EventKind::Any,
        ];
        for kind in &ignored {
            assert!(!relevant_kind(kind), "{kind:?} must be ignored");
        }
    }

    #[test]
    fn extension_filter_derives_from_registered_parsers() {
        // The accept list comes from the parser registry, not a hardcoded
        // list: with the full production registry the union covers every
        // ingestable extension.
        let wanted = normalize_extensions(full_registry().supported_extensions());
        assert!(wanted_extension(Path::new("/a/doc.md"), &wanted));
        assert!(wanted_extension(Path::new("/a/doc.markdown"), &wanted));
        assert!(wanted_extension(Path::new("/a/doc.json"), &wanted));
        assert!(wanted_extension(Path::new("/a/doc.html"), &wanted));
        // Case-insensitive, like the parsers' own accepts checks.
        assert!(wanted_extension(Path::new("/a/DOC.MD"), &wanted));
        assert!(!wanted_extension(Path::new("/a/doc.txt"), &wanted));
        assert!(!wanted_extension(Path::new("/a/nodir"), &wanted));

        // A partial registry filters to exactly what is registered — the
        // watcher tracks the pipeline instead of a parallel list.
        let md_only = Fixture::new(global_config(Vec::new())).registry;
        let md_wanted = normalize_extensions(md_only.supported_extensions());
        assert!(wanted_extension(Path::new("/a/doc.md"), &md_wanted));
        assert!(wanted_extension(Path::new("/a/doc.markdown"), &md_wanted));
        assert!(!wanted_extension(Path::new("/a/doc.json"), &md_wanted));
        assert!(!wanted_extension(Path::new("/a/doc.html"), &md_wanted));

        // An empty registry accepts nothing.
        let none = normalize_extensions(Registry::new().supported_extensions());
        assert!(!wanted_extension(Path::new("/a/doc.md"), &none));
    }

    // --- debounce_loop (real time, short debounce) -------------------------

    #[tokio::test]
    async fn debounce_loop_coalesces_burst_into_one_batch() {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (batch_tx, mut batch_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(());
        let task = tokio::spawn(debounce_loop(
            events_rx,
            stop_rx,
            batch_tx,
            Duration::from_millis(50),
        ));
        // A tight burst: all three events land well inside one window.
        events_tx.send(PathBuf::from("/a/1.md")).unwrap();
        events_tx.send(PathBuf::from("/a/2.md")).unwrap();
        events_tx.send(PathBuf::from("/b/3.json")).unwrap();
        let batch = tokio::time::timeout(Duration::from_secs(5), batch_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            batch,
            vec![
                PathBuf::from("/a/1.md"),
                PathBuf::from("/a/2.md"),
                PathBuf::from("/b/3.json")
            ]
        );
        let _ = stop_tx.send(());
        task.await.unwrap();
        assert!(
            batch_rx.recv().await.is_none(),
            "no further batches after stop"
        );
    }

    #[tokio::test]
    async fn debounce_loop_exits_on_stop_without_events() {
        let (_tx, events_rx) = mpsc::unbounded_channel::<PathBuf>();
        let (_batch_tx, _batch_rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
        let (stop_tx, stop_rx) = watch::channel(());
        let task = tokio::spawn(debounce_loop(
            events_rx,
            stop_rx,
            _batch_tx,
            Duration::from_millis(10),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop_tx.send(()).unwrap();
        task.await.unwrap();
    }

    // --- Watcher lifecycle (new → event → batch → stop) --------------------

    #[tokio::test]
    async fn watcher_lifecycle_new_event_stop() {
        let root = TempDir::new("lifecycle");
        let src = root.sub("src");
        // The directory is empty at watch time (the poll backend baselines
        // it), so the file written afterwards is reported as a Create event
        // on the next walk.
        let mut watcher = Watcher::new(
            Duration::from_millis(50),
            vec![src.clone()],
            vec!["md".to_owned()],
        )
        .unwrap();
        fs::write(src.join("a.md"), "# A\n\nBody.\n").unwrap();
        // The poll backend walks every POLL_INTERVAL (2 s): bound the wait
        // well above one interval.
        let batch = tokio::time::timeout(Duration::from_secs(15), watcher.next_batch())
            .await
            .expect("a batch arrives within the poll-interval bound")
            .expect("the debounce task is still running");
        assert!(
            batch.iter().any(|p| p == &src.join("a.md")),
            "the created file is in the batch: {batch:?}"
        );
        // stop() signals the debounce loop and waits for it to finish.
        watcher.stop().await;
        assert!(watcher.task.is_finished(), "the debounce task has ended");
        // Dropping the watcher (early-return paths) is the no-op safety net.
        drop(watcher);
    }

    // --- IngestChangeHandler (real Runner, fake embed) ---------------------

    #[test]
    fn handler_enqueues_jobs_for_changed_files_and_ignores_foreign_paths() {
        let root = TempDir::new("handler");
        let src_a = root.sub("a");
        let src_b = root.sub("b");
        let foreign = root.sub("foreign");
        fs::write(src_a.join("a.md"), "# Doc A\ncontent a\n").unwrap();
        fs::write(src_b.join("b.md"), "# Doc B\ncontent b\n").unwrap();
        let global = global_config(vec![
            source_config(src_a.to_string_lossy().as_ref(), false),
            source_config(src_b.to_string_lossy().as_ref(), false),
        ]);
        let fixture = Fixture::new(global);
        let runner = fixture.runner();
        let queue = DocumentJobQueue::new(&fixture.db);
        let graph_cfg = GraphConfig {
            enable_graph: false,
            max_depth: 5,
            max_nodes: 100,
            load_on_startup: true,
        };
        let handler = IngestChangeHandler::new(&runner, &fixture.db, &graph_cfg, None, &queue);

        // Two files of source A (one of them never existed) + one of B + a
        // file outside every configured source.
        handler.handle_changes(&[
            src_a.join("a.md"),
            src_a.join("ghost.md"),
            src_b.join("b.md"),
            foreign.join("x.md"),
        ]);

        // The handler is a producer: pending jobs, no documents (the worker
        // ingests later). The foreign file (no configured source) contributes
        // nothing.
        let jobs = job_rows(&fixture.db);
        assert_eq!(jobs.len(), 3, "a.md + ghost.md + b.md enqueued; {jobs:?}");
        let by_path: HashMap<&str, &db::DocumentJob> =
            jobs.iter().map(|job| (job.path.as_str(), job)).collect();

        // a.md: present on disk → pending index with the fresh hash.
        let a = by_path
            .get(src_a.join("a.md").to_string_lossy().as_ref())
            .expect("a.md must be queued");
        assert_eq!(a.op, "index");
        assert_eq!(a.status, "pending");
        assert_eq!(a.source_path, src_a.to_string_lossy().into_owned());
        assert_eq!(
            a.content_hash.as_deref(),
            Some(compute_content_hash("# Doc A\ncontent a\n").as_str())
        );

        // ghost.md: absent on disk → pending delete.
        let ghost = by_path
            .get(src_a.join("ghost.md").to_string_lossy().as_ref())
            .expect("ghost.md must be queued");
        assert_eq!(ghost.op, "delete");
        assert_eq!(ghost.status, "pending");

        // b.md: present on disk → pending index.
        let b = by_path
            .get(src_b.join("b.md").to_string_lossy().as_ref())
            .expect("b.md must be queued");
        assert_eq!(b.op, "index");
        assert_eq!(b.status, "pending");

        assert!(
            doc_count(&fixture.db) == 0,
            "no document until the worker runs"
        );
    }

    #[test]
    fn handler_enqueues_delete_for_a_removed_file() {
        let root = TempDir::new("prune");
        let src_a = root.sub("a");
        let src_b = root.sub("b");
        fs::write(src_a.join("a.md"), "# A\n\nBody text for document A.\n").unwrap();
        fs::write(src_b.join("b.md"), "# B\n\nBody text for document B.\n").unwrap();
        let global = global_config(vec![
            source_config(src_a.to_string_lossy().as_ref(), false),
            source_config(src_b.to_string_lossy().as_ref(), false),
        ]);
        let fixture = Fixture::new(global);
        let runner = fixture.runner();
        let queue = DocumentJobQueue::new(&fixture.db);
        let graph_cfg = GraphConfig {
            enable_graph: false,
            max_depth: 5,
            max_nodes: 100,
            load_on_startup: true,
        };
        let handler = IngestChangeHandler::new(&runner, &fixture.db, &graph_cfg, None, &queue);

        handler.handle_changes(&[src_a.join("a.md"), src_b.join("b.md")]);
        // Two pending index jobs; the documents table is still empty.
        let jobs = job_rows(&fixture.db);
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        assert!(
            jobs.iter()
                .all(|job| job.op == "index" && job.status == "pending"),
            "{jobs:?}"
        );
        assert_eq!(
            doc_count(&fixture.db),
            0,
            "no document until the worker runs"
        );

        fs::remove_file(src_a.join("a.md")).unwrap();
        handler.handle_changes(&[src_a.join("a.md")]);
        // The vanished file's job is replaced by a pending delete (upsert by
        // path); the live file's job survives. The documents table is still
        // unchanged (the worker removes the row later).
        let jobs = job_rows(&fixture.db);
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        let a = jobs
            .iter()
            .find(|job| job.path.ends_with("a/a.md"))
            .expect("a.md job present");
        assert_eq!(a.op, "delete");
        assert_eq!(a.status, "pending");
        let b = jobs
            .iter()
            .find(|job| job.path.ends_with("b/b.md"))
            .expect("b.md job present");
        assert_eq!(b.op, "index");
        assert_eq!(b.status, "pending");
        assert_eq!(
            doc_count(&fixture.db),
            0,
            "documents unchanged until the worker runs"
        );
    }

    #[test]
    fn handler_reloads_graph_when_enabled() {
        let root = TempDir::new("graph-on");
        let src_a = root.sub("a");
        fs::write(src_a.join("a.md"), "# A\n").unwrap();
        let global = global_config(vec![source_config(src_a.to_string_lossy().as_ref(), false)]);
        let fixture = Fixture::new(global);
        let runner = fixture.runner();
        let graph_cfg = GraphConfig {
            enable_graph: true,
            max_depth: 5,
            max_nodes: 100,
            load_on_startup: true,
        };
        let queue = DocumentJobQueue::new(&fixture.db);
        let reloaded: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn Fn(Arc<GraphIndex>) + Send + Sync> = Arc::new({
            let reloaded = reloaded.clone();
            move |g: Arc<GraphIndex>| {
                reloaded.lock().unwrap().push(g.is_available());
            }
        });
        let handler =
            IngestChangeHandler::new(&runner, &fixture.db, &graph_cfg, Some(hook), &queue);
        handler.handle_changes(&[src_a.join("a.md")]);
        // An empty-but-valid graph reloads to Ready.
        assert_eq!(reloaded.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn handler_skips_graph_reload_when_disabled() {
        let root = TempDir::new("graph-off");
        let src_a = root.sub("a");
        fs::write(src_a.join("a.md"), "# A\n").unwrap();
        let global = global_config(vec![source_config(src_a.to_string_lossy().as_ref(), false)]);
        let fixture = Fixture::new(global);
        let runner = fixture.runner();
        let graph_cfg = GraphConfig {
            enable_graph: false,
            max_depth: 5,
            max_nodes: 100,
            load_on_startup: true,
        };
        let queue = DocumentJobQueue::new(&fixture.db);
        let reloaded: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn Fn(Arc<GraphIndex>) + Send + Sync> = Arc::new({
            let reloaded = reloaded.clone();
            move |g: Arc<GraphIndex>| {
                reloaded.lock().unwrap().push(g.is_available());
            }
        });
        let handler =
            IngestChangeHandler::new(&runner, &fixture.db, &graph_cfg, Some(hook), &queue);
        handler.handle_changes(&[src_a.join("a.md")]);
        assert!(reloaded.lock().unwrap().is_empty(), "hook must not fire");
    }

    // --- watchable_sources --------------------------------------------------

    #[test]
    fn watchable_sources_returns_enabled_dirs_only() {
        let root = TempDir::new("sources-ok");
        // The ontology is per-dataset: <workspace_dir>/datasets/edtech/ontology.
        let ontology = root.sub("datasets").join("edtech").join("ontology");
        fs::create_dir_all(&ontology).unwrap();
        let src1 = root.sub("src1");
        let src_disabled = root.sub("srcdisabled");
        fs::write(
            ontology.join("global.xml"),
            format!(
                r#"<global><sources>
<source path="{src1}" type="markdown"/>
<source path="{src_disabled}" type="markdown" disabled="true"/>
</sources></global>"#,
                src1 = src1.display(),
                src_disabled = src_disabled.display(),
            ),
        )
        .unwrap();
        let mut config = Config::default();
        config.paths.workspace_dir = root.0.to_string_lossy().into_owned();
        // No dataset by default (revision 1.1): name the fixture's dataset.
        config.dataset.name = "edtech".to_string();
        let sources = watchable_sources(&config).unwrap();
        assert_eq!(
            sources,
            vec![abs_path(src1.to_string_lossy().as_ref()).unwrap()]
        );
    }

    #[test]
    fn watchable_sources_rejects_non_directory_source() {
        let root = TempDir::new("sources-bad");
        // The ontology is per-dataset: <workspace_dir>/datasets/edtech/ontology.
        let ontology = root.sub("datasets").join("edtech").join("ontology");
        fs::create_dir_all(&ontology).unwrap();
        let file = root.0.join("just-a-file");
        fs::write(&file, "x").unwrap();
        fs::write(
            ontology.join("global.xml"),
            format!(
                r#"<global><sources>
<source path="{file}" type="markdown"/>
</sources></global>"#,
                file = file.display(),
            ),
        )
        .unwrap();
        let mut config = Config::default();
        config.paths.workspace_dir = root.0.to_string_lossy().into_owned();
        // No dataset by default (revision 1.1): name the fixture's dataset.
        config.dataset.name = "edtech".to_string();
        let err = watchable_sources(&config).unwrap_err();
        assert!(
            matches!(err, WatcherError::SourceNotADirectory { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn watchable_sources_empty_without_global() {
        let root = TempDir::new("sources-none");
        // The dataset ontology dir exists but has no global.xml.
        let ontology = root.sub("datasets").join("edtech").join("ontology");
        fs::create_dir_all(&ontology).unwrap();
        let mut config = Config::default();
        config.paths.workspace_dir = root.0.to_string_lossy().into_owned();
        // No dataset by default (revision 1.1): name the fixture's dataset.
        config.dataset.name = "edtech".to_string();
        assert!(watchable_sources(&config).unwrap().is_empty());
    }
}
