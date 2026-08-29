//! p50/p95 latency parity test (task 1.3, design D4).
//!
//! Machine-checked latency parity for the 12 frozen MCP tools: boots the
//! product MCP server (`mcp::Server` over rmcp Streamable HTTP, axum on a
//! random loopback port) in-process, over a small corpus ingested through
//! the `ingestion` crate, drives every tool through the harness
//! [`McpClient`], and asserts the search-tool latency gates against the Go
//! oracle baseline:
//!
//! ```text
//! p50(search) <= 2 x Go baseline p50
//! p95(search) <= 5 x Go baseline p95
//! ```
//!
//! # Go baseline and its assumption
//!
//! The Go benchmark (design.md D4, recorded with `../synopsis/bin/synopsis`
//! over the 270-chunk knowledge.db) measured the `search` tool at
//! p50 ≈ 106.8 ms and p95 ≈ 108.9 ms. The exact benchmark cannot be
//! re-run from Rust — the legacy Go database is bindingly off-limits to the
//! Rust product (it never opens or migrates `knowledge.db`) — so the
//! documented baseline values are the gate reference.
//!
//! The comparison stays like-for-like: the corpus is embedded with
//! **bge-small-en-v1.5 (384-dim)**, the same model the Go oracle used for
//! knowledge.db and the `workspace/configs/onnx.yaml` registry default,
//! loaded from the pre-installed copy under the repo `workspace/`
//! directory (provenance: `workspace/README.md`). The product default
//! (bge-m3-int8, 1024-dim, 2.3 GB)
//! is deliberately not used: it would make the test several times heavier
//! than the product startup path without adding parity signal.
//!
//! # Graceful skip
//!
//! If the ONNX runtime library or model cannot be loaded in this
//! environment (missing `workspace/` artifacts, unsupported platform), the
//! test prints a clear message and returns without failing the suite.
//!
//! # Unindexed Lance table
//!
//! The corpus is far smaller than the ADR 0003 IVF partition count (256),
//! so `build_index` is deliberately NOT called: lancedb falls back to flat
//! search — the same state the production `serve` flow runs in until an
//! explicit index build (only the loadtest filler builds one).
//!
//! # Runtime layout
//!
//! The test is a plain `#[test]` (synchronous). The ONNX provider and the
//! Lance engine are sync facades over their own runtimes and must not be
//! touched from inside a tokio runtime context, so everything is built on
//! the test thread; a single `Runtime::block_on` then serves the MCP
//! server and drives the client. Tool dispatch hops to `spawn_blocking`
//! inside the server, so the searcher runs in a safe synchronous context.

// Test code: unwrap/expect are intentional (test-infra failures panic with
// a message rather than plumbing Results through helpers).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use config::ontology::{GlobalConfig, GlobalNerConfig, SourceConfig, SourceType};
use config::preset::{
    ChunkingStrategy, IngestionConfig, LinkerConfig, LocalEmbedding, MarkdownChunkerConfig,
    SearchConfig,
};
use db::test_util;
use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, DocumentFilter};
use embedding::{EmbeddingProvider, new_onnx_provider};
use graph::GraphIndex;
use ingestion::worker::DocumentWorker;
use ingestion::{
    DocumentJobQueue, MarkdownChunker, MarkdownSource, Registry, Runner, RunnerParams,
    load_ner_prompts,
};
use mcp::Server;
use parity_harness::mcp_client::{McpClient, TimingStats};
use search::{
    Enricher, HybridSearcher, LexicalSearcher, Reranker, SearchError, SearchResult, Searcher,
    SemanticSearcher,
};
use vectors::{LanceEngine, VectorIndex, VectorIndexConfig};

/// Go oracle `search` baseline p50 in ms (design.md D4, knowledge.db).
const GO_SEARCH_P50_MS: u64 = 107;
/// Go oracle `search` baseline p95 in ms (design.md D4, knowledge.db).
const GO_SEARCH_P95_MS: u64 = 109;
/// Steady-state `search` samples (warm-up excluded via `reset_stats`).
const SEARCH_SAMPLES: usize = 21;

/// The 12 frozen MCP tool names (`mcp-contract`).
const FROZEN_TOOLS: [&str; 12] = [
    "search",
    "catalog_overview",
    "catalog_documents",
    "catalog_entities",
    "search_entities_by_type",
    "search_facts",
    "get_document_context",
    "get_chunk_by_id",
    "get_fact_by_id",
    "get_entity_dossier",
    "get_entity_relations",
    "get_entity_links",
];

/// Tools that MUST succeed on the ingested corpus (documents and chunks
/// present; entities and facts absent by construction — NER is disabled).
const EXPECTED_SUCCESS: [&str; 8] = [
    "search",
    "catalog_overview",
    "catalog_documents",
    "catalog_entities",
    "search_entities_by_type",
    "search_facts",
    "get_document_context",
    "get_chunk_by_id",
];

/// Tools that MUST return a tool-level error on the ingested corpus (no
/// fact with id 1, no entity with id 1, graph disabled).
const EXPECTED_TOOL_ERROR: [&str; 4] = [
    "get_fact_by_id",
    "get_entity_dossier",
    "get_entity_relations",
    "get_entity_links",
];

/// p50/p95 latency parity: boot the product MCP server over an ingested
/// corpus, drive all 12 frozen tools, and gate the search-tool latency
/// against the Go oracle baseline.
#[test]
fn mcp_tool_latency_meets_go_gates() {
    let repo_root = repo_root();

    // 1. Embedding provider — the graceful-skip boundary.
    let provider = match build_provider(&repo_root) {
        Some(provider) => provider,
        None => {
            eprintln!(
                "SKIP: the ONNX runtime or the bge-small-en-v1.5 model under \
                 workspace/ is not usable in this environment; see workspace/README.md"
            );
            return;
        }
    };
    let dim = provider.vector_dim();

    // 2. Scratch space + corpus.
    let scratch = TempDir::new("mcp-latency");
    let corpus = scratch.join("corpus");
    write_corpus(&corpus);

    // 3. File-backed database + Lance engine (production configuration).
    let db = test_util::temp_file_db();
    let engine = LanceEngine::create(scratch.join("vectors"), vector_index_config(dim))
        .expect("lance engine create");
    let vectors: Arc<dyn VectorIndex> = Arc::new(engine);

    // 4. Ingest the corpus through the production pipeline (sync facade).
    ingest_corpus(&db, &provider, &vectors, &corpus);

    // 5. Ids the corpus produced (for the per-id tools).
    let (doc_id, chunk_id) = first_doc_and_chunk(&db);

    // 6. Product MCP server over the pooled hybrid searcher.
    let searcher = Arc::new(PooledSearcher {
        db: db.clone(),
        search_config: search_config(),
        embed: provider.clone(),
        vectors: vectors.clone(),
    });
    let server = Server::new(
        "synopsis-parity".to_owned(),
        "0.1.0".to_owned(),
        db.clone(),
        searcher,
        Arc::new(GraphIndex::Unavailable),
    );

    // 7. One runtime: serve + drive, then graceful shutdown.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let report = runtime
        .block_on(async move { serve_and_drive(server, doc_id, chunk_id).await })
        .expect("serve + drive");
    runtime.shutdown_timeout(Duration::from_secs(5));

    // 8. Assertions: full tool coverage + search latency gates.
    assert_tool_coverage(&report.stats);
    assert_search_gates(&report.stats);
    assert!(
        report.search_total_count > 0,
        "search returned no results for the ingested corpus"
    );

    eprintln!("parity latency report:\n{report}");
}

/// Measured phase outcome.
struct Report {
    /// Per-operation timing samples (measured phase only).
    stats: TimingStats,
    /// `total_count` of the first measured `search` call.
    search_total_count: usize,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for tool in FROZEN_TOOLS {
            let op = format!("tools/call:{tool}");
            match (
                self.stats.p50(&op),
                self.stats.p95(&op),
                self.stats.avg(&op),
            ) {
                (Some(p50), Some(p95), Some(avg)) => writeln!(
                    f,
                    "  {tool:<24} n={:>2}  p50={p50:?}  p95={p95:?}  avg={avg:?}",
                    self.stats.count(&op)
                ),
                _ => writeln!(f, "  {tool:<24} (no samples)"),
            }?;
        }
        Ok(())
    }
}

/// Bind the product router to a random loopback port, drive the 12 tools
/// through the harness client (warm-up, reset, measured phase), then shut
/// the server down gracefully.
async fn serve_and_drive(server: Server, doc_id: i64, chunk_id: i64) -> Result<Report, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("bind loopback: {err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("local addr: {err}"))?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = tokio::spawn(async move {
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let url = format!("http://{addr}/mcp");
    let mut client = McpClient::connect(&url)
        .await
        .map_err(|err| format!("connect: {err}"))?;

    // Full 12-tool coverage of the frozen contract (order-insensitive).
    let tools = client
        .list_tools()
        .await
        .map_err(|err| format!("tools/list: {err}"))?;
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    let mut expected = FROZEN_TOOLS.to_vec();
    expected.sort_unstable();
    if names != expected {
        return Err(format!("tools/list mismatch: {names:?}"));
    }

    // Warm-up: one call per tool (connection warm-up, ONNX session warm-up,
    // lazy init). Results irrelevant — excluded via reset_stats.
    for name in FROZEN_TOOLS {
        let _ = client
            .call_tool(name, tool_args(name, doc_id, chunk_id))
            .await;
    }
    client.reset_stats();

    // Measured phase: every tool once, plus extra search samples.
    let mut search_total_count = 0usize;
    for name in FROZEN_TOOLS {
        let outcome = client
            .call_tool(name, tool_args(name, doc_id, chunk_id))
            .await;
        match name {
            "search" => {
                let result = outcome.map_err(|err| format!("search: {err}"))?;
                search_total_count = parse_total_count(&result)?;
            }
            name if EXPECTED_SUCCESS.contains(&name) => {
                outcome.map_err(|err| format!("{name}: {err}"))?;
            }
            name if EXPECTED_TOOL_ERROR.contains(&name) && outcome.is_ok() => {
                return Err(format!("{name}: expected a tool error, got success"));
            }
            _ => {}
        }
    }
    for _ in 1..SEARCH_SAMPLES {
        client
            .call_tool("search", tool_args("search", doc_id, chunk_id))
            .await
            .map_err(|err| format!("steady-state search: {err}"))?;
    }

    let stats = client.stats().clone();
    client
        .close()
        .await
        .map_err(|err| format!("close: {err}"))?;
    let _ = shutdown_tx.send(());
    serve
        .await
        .map_err(|err| format!("serve task join: {err}"))?
        .map_err(|err| format!("axum serve: {err}"))?;

    Ok(Report {
        stats,
        search_total_count,
    })
}

/// Per-tool arguments for the frozen schema.
fn tool_args(name: &str, doc_id: i64, chunk_id: i64) -> serde_json::Map<String, serde_json::Value> {
    let mut args = serde_json::Map::new();
    let mut insert = |key: &str, value: serde_json::Value| {
        args.insert(key.to_owned(), value);
    };
    match name {
        "search" => {
            insert("query", serde_json::json!("vacation policy"));
            insert("top_k", serde_json::json!(5));
        }
        "search_entities_by_type" => insert("entity_type", serde_json::json!("person")),
        "get_document_context" => insert("document_id", serde_json::json!(doc_id.to_string())),
        "get_chunk_by_id" => insert("chunk_id", serde_json::json!(chunk_id.to_string())),
        "get_fact_by_id" => insert("fact_id", serde_json::json!("1")),
        "get_entity_dossier" | "get_entity_relations" | "get_entity_links" => {
            insert("entity_id", serde_json::json!("1"));
        }
        _ => {}
    }
    args
}

/// The `total_count` field of the `search` tool's oracle-shaped payload.
fn parse_total_count(result: &rmcp::model::CallToolResult) -> Result<usize, String> {
    let text = result
        .content
        .iter()
        .find_map(rmcp::model::ContentBlock::as_text)
        .ok_or_else(|| "search returned no text content".to_owned())?;
    let value: serde_json::Value =
        serde_json::from_str(&text.text).map_err(|err| format!("search payload: {err}"))?;
    let total = value
        .get("total_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "search payload has no total_count".to_owned())?;
    usize::try_from(total).map_err(|err| format!("total_count: {err}"))
}

/// Every frozen tool has at least one measured sample, and `search` has
/// exactly the steady-state sample count.
fn assert_tool_coverage(stats: &TimingStats) {
    for tool in FROZEN_TOOLS {
        let op = format!("tools/call:{tool}");
        assert!(stats.count(&op) >= 1, "no measured samples for `{tool}`");
    }
    assert_eq!(
        stats.count("tools/call:search"),
        SEARCH_SAMPLES,
        "search sample count"
    );
}

/// The design D4 gates: p50 within 2x and p95 within 5x of the Go baseline.
fn assert_search_gates(stats: &TimingStats) {
    let op = "tools/call:search";
    let p50 = stats.p50(op).expect("search p50 recorded");
    let p95 = stats.p95(op).expect("search p95 recorded");
    let p50_gate = Duration::from_millis(2 * GO_SEARCH_P50_MS);
    let p95_gate = Duration::from_millis(5 * GO_SEARCH_P95_MS);
    assert!(
        p50 <= p50_gate,
        "search p50 {p50:?} exceeds gate {p50_gate:?} (Go baseline p50 {GO_SEARCH_P50_MS} ms)"
    );
    assert!(
        p95 <= p95_gate,
        "search p95 {p95:?} exceeds gate {p95_gate:?} (Go baseline p95 {GO_SEARCH_P95_MS} ms)"
    );
}

/// Build the ONNX embedding provider from the pre-installed artifacts under
/// the repo `workspace/` directory, or `None` when they are not usable here.
///
/// Uses the explicit `model_path` override (oracle `NewONNXProvider`
/// semantics): the file is used as-is — no registry lookup, no download, no
/// manifest mutation — keeping the test fully offline.
fn build_provider(repo_root: &Path) -> Option<Arc<dyn EmbeddingProvider>> {
    let onnx_path = repo_root
        .join("workspace")
        .join("configs")
        .join("onnx.yaml");
    let onnx_cfg = config::load_onnx_config(&onnx_path).ok()?;
    let model_path = repo_root
        .join("workspace")
        .join("models")
        .join("bge-small-en-v1.5")
        .join("model.onnx");
    if !model_path.is_file() {
        return None;
    }
    let cfg = LocalEmbedding {
        model_name: "bge-small-en-v1.5".to_owned(),
        model_path: model_path.to_string_lossy().into_owned(),
        vector_dim: 384,
        ..Default::default()
    };
    new_onnx_provider(&cfg, repo_root.join("workspace"), &onnx_cfg).ok()
}

/// ADR 0003 ANN parameters for the test dimension (the index itself is
/// never built — see the module docs).
fn vector_index_config(dim: usize) -> VectorIndexConfig {
    VectorIndexConfig::new(dim, 16, 100, 256, 32, 200).expect("ADR 0003 parameters validate")
}

/// Hybrid search configuration for the test (both legs enabled; explicit
/// values — `SearchConfig::default()` disables both legs).
fn search_config() -> SearchConfig {
    SearchConfig {
        rrf_k: 20,
        lexical_top_k: 20,
        semantic_top_k: 20,
        final_top_k: 10,
        enable_lexical: true,
        enable_semantic: true,
        timeout_ms: 10_000,
        deprecated_boost: 0.0,
        official_boost: 0.0,
        recent_boost: 0.0,
        recent_days: 0,
        authority_boost: HashMap::new(),
    }
}

/// Two small markdown documents with clearly distinct topics (one per
/// file), so the `search` tool has real lexical and semantic signal.
fn write_corpus(corpus: &Path) {
    std::fs::create_dir_all(corpus).expect("create corpus dir");
    std::fs::write(
        corpus.join("hr-policy.md"),
        "# Vacation and Leave Policy\n\nEmployees are entitled to twenty days of paid vacation \
         per calendar year. Vacation requests must be submitted at least two weeks in advance \
         through the internal portal. Unused vacation days carry over up to five days.\n\n\
         ## Sick Leave\n\nSick leave is unlimited and requires a medical certificate for \
         absences longer than three consecutive days.\n",
    )
    .expect("write hr-policy.md");
    std::fs::write(
        corpus.join("atlas-release.md"),
        "# Atlas Product Release Notes\n\nAtlas 3.0 introduces a new dashboard builder with \
         drag-and-drop widgets. The release adds real-time collaboration, offline mode, and a \
         redesigned reporting engine.\n\n## Migration Guide\n\nExisting workspaces migrate \
         automatically. Custom integrations using the legacy REST API v1 must be updated to API \
         v2 before the deprecation date.\n",
    )
    .expect("write atlas-release.md");
}

/// Ingest the corpus through the production pipeline (markdown source →
/// hybrid chunker → embeddings → vector index) and assert the pipeline's
/// own bookkeeping.
fn ingest_corpus(
    db: &db::Db,
    provider: &Arc<dyn EmbeddingProvider>,
    vectors: &Arc<dyn VectorIndex>,
    corpus: &Path,
) {
    let markdown = MarkdownChunkerConfig {
        strategy: ChunkingStrategy::Hybrid,
        max_chunk_size: 8192,
        overlap_size: 100,
        min_section_size: 500,
    };
    let mut ingest_cfg = IngestionConfig::default();
    ingest_cfg.chunking.markdown = markdown.clone();
    ingest_cfg.ner.disabled = true;

    let global = GlobalConfig {
        sources: vec![SourceConfig {
            path: corpus.to_string_lossy().into_owned(),
            source_type: SourceType::Markdown,
            disabled: false,
            space: String::new(),
            domains: vec!["default".to_owned()],
            dataset: String::new(),
        }],
        cross_domain_links: None,
        ner: GlobalNerConfig {
            methods: Vec::new(),
        },
        entities: Vec::new(),
        relations: Vec::new(),
        extraction: Default::default(),
    };
    let domains = HashMap::new();
    let mut registry = Registry::new();
    registry
        .register(
            MarkdownSource::SOURCE_TYPE,
            Box::new(MarkdownSource::new(Box::new(MarkdownChunker::new(
                markdown,
            )))),
        )
        .expect("register markdown source");
    let prompts = load_ner_prompts("/nonexistent-ner-prompts").expect("embedded NER prompts");
    let linker_cfg = LinkerConfig::default();

    let runner = Runner::new(RunnerParams {
        db,
        ingest_cfg: &ingest_cfg,
        global: Some(&global),
        domains: &domains,
        registry: &registry,
        embed: provider.as_ref(),
        vectors: vectors.as_ref(),
        prompts: &prompts,
        linker_cfg: &linker_cfg,
        prompts_path: "/nonexistent-prompts",
        llm_cache: None,
    });

    // The queue is the only processing path: reconcile the corpus source
    // (two new files → two index jobs) and let one worker cycle process
    // them.
    let queue = DocumentJobQueue::new(db);
    let reconcile = queue
        .reconcile_source(&runner, &global.sources[0].path)
        .expect("reconcile the corpus source");
    assert_eq!(reconcile.indexed, 2, "two corpus files queued");
    let worker = DocumentWorker::new(db, &runner, ingest_cfg.max_retries);
    worker.run_once(1_000).expect("worker cycle");
    let done_jobs: i64 = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM document_jobs WHERE status = 'done'",
                [],
                |row| row.get(0),
            )
        })
        .expect("db connection")
        .expect("job count");
    assert_eq!(done_jobs, 2, "two corpus documents ingested");

    let (docs, chunks) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec)
                .count(&DocumentFilter::default())
                .ok()?;
            let chunks = ChunkDao::new(exec).count().ok()?;
            Some((docs, chunks))
        })
        .expect("db connection")
        .expect("chunk/document counts");
    let stored = vectors.count().expect("vector count");
    assert_eq!(docs, 2, "two documents in the database");
    assert!(chunks > 0, "chunks produced");
    assert_eq!(stored as i64, chunks, "every chunk has a stored vector");
}

/// The first ingested document and its first chunk (deterministic: SQLite
/// rowid order of the ingestion order).
fn first_doc_and_chunk(db: &db::Db) -> (i64, i64) {
    db.with_conn(|conn| {
        let exec = ConnectionOrTx::Connection(conn);
        let doc = DocumentDao::new(exec).list().ok()?.into_iter().next()?;
        let chunk = ChunkDao::new(exec)
            .list_by_doc_id(doc.id)
            .ok()?
            .into_iter()
            .next()?;
        Some((doc.id, chunk.id))
    })
    .expect("db connection")
    .expect("corpus ingested a document and a chunk")
}

/// Search handle for the test server (mirrors the production
/// `cli::serve::server::PooledSearcher`, minus the graph expander): every
/// call checks out a pooled connection and assembles a fresh (cheap) hybrid
/// searcher on it. Synchronous by contract — the mcp dispatch boundary hops
/// tool calls to `spawn_blocking` before invoking it.
struct PooledSearcher {
    db: db::Db,
    search_config: SearchConfig,
    embed: Arc<dyn EmbeddingProvider>,
    vectors: Arc<dyn VectorIndex>,
}

impl PooledSearcher {
    fn run<T>(
        &self,
        f: impl FnOnce(&HybridSearcher<'_>) -> Result<T, SearchError>,
    ) -> Result<T, SearchError> {
        self.db.with_conn(|conn| {
            let chunks = ChunkDao::new(ConnectionOrTx::Connection(conn));
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let chunk_entities = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            let searcher = HybridSearcher::new(
                self.search_config.clone(),
                LexicalSearcher::new(&chunks),
                SemanticSearcher::new(&chunks, &documents, &*self.embed, &*self.vectors),
                Enricher::new(&documents, &chunk_entities),
                Reranker::new(Some(&self.search_config)),
                None,
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

/// Resolve the repo root from the crate manifest location (robust against
/// the test process working directory).
fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("crates/parity-harness lives two levels below the repo root")
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "synopsis-parity-{prefix}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
