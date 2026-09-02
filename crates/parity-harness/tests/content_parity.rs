//! Content-parity test (parity-fixture-expansion tasks 1.4/1.5, design D1/D4).
//!
//! Machine-checked **content** parity for the catalog and `search` MCP tools:
//! the product MCP server (`mcp::Server` over rmcp Streamable HTTP, axum on a
//! random loopback port) is booted in-process over the content corpus
//! (`corpus::write_content_corpus`) ingested through the production pipeline
//! (`ingestion` crate, NER disabled), each tool is driven through the harness
//! [`McpClient`] with the exact args the Go fixtures were recorded with
//! (`fixtures/content/README.md`), and every response is compared against its
//! committed Go-oracle fixture with
//! [`content_parity::assert_content_parity`] (strict `json_diff` after
//! `content_parity::normalize`).
//!
//! `search` (task 1.5) is driven in the same single boot as the catalog tools
//! (the corpus ingest is the expensive step; re-running it for a separate
//! test would only duplicate it). `normalize` strips the exact `score` and
//! wall-clock duration, so the comparison is the result count plus the
//! rank-ordered identity (`document_id` / `chunk_id`) of each result — a
//! divergence on identity (not just score) is the intended signal of a real
//! behavior difference and fails the test with the `json_diff` output.
//!
//! # Ingestion order (load-bearing)
//!
//! The fixture's document ids are the Go recording's ingestion order: the Go
//! config listed one markdown source per domain sub-directory in the order
//! `hr`, `product`, `eng` (fixtures/content/README.md). The Rust worker
//! claims due jobs in `(next_attempt_at, path)` order — every enqueued job is
//! due at zero, so reconciling all three sources up-front would drain them in
//! path order (`eng` first) and the ids would diverge from the fixture. The
//! test therefore reconciles and drains ONE source per worker cycle, in the
//! recording order, which reproduces the fixture's id assignment (ids 1-3 are
//! the `hr` documents).
//!
//! # Test-local reduction for `catalog_documents`
//!
//! Two fixture fields are environment- or implementation-defined and are
//! reduced test-locally — applied to BOTH the fixture and the live response
//! before `assert_content_parity` (which still applies the standard
//! normalization: `created_at`/`updated_at` stripping, `domain` sorting, page
//! order by `id`):
//!
//! - `original_path` → file name: the scratch directory differs between the
//!   Go recording (`target/parity-go/documents/…`) and the Rust run (a temp
//!   dir); the file name is the identity signal that survives.
//! - `metadata` → the `domain` key only: the Go metadata bag carries
//!   `file_size`, `modified_at`, `source_file`, `source_type` that the Rust
//!   product moved to dedicated columns (a documented re-architecture); the
//!   persisted extension bag on both sides is the `domain` list.
//!
//! # Graceful skip
//!
//! Same boundary as `parity_test.rs`: if the ONNX runtime library or the
//! bge-small-en-v1.5 model under `workspace/` cannot be loaded in this
//! environment, the test prints a clear message and returns without failing
//! the suite.
//!
//! # Runtime layout
//!
//! The test is a plain `#[test]` (synchronous): the ONNX provider and the
//! usearch engine are sync facades that must not be touched from inside a
//! tokio runtime context, so everything is built on the test thread and a
//! single `Runtime::block_on` serves the MCP server and drives the client
//! (same rationale as `parity_test.rs`).

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
use parity_harness::content_parity::{assert_content_parity, load_fixture};
use parity_harness::corpus::write_content_corpus;
use parity_harness::mcp_client::McpClient;
use rmcp::model::ContentBlock;
use search::{
    Enricher, HybridSearcher, LexicalSearcher, Reranker, SearchError, SearchResult, Searcher,
    SemanticSearcher,
};
use serde_json::{Map, Value};
use vectors::{UsearchEngine, VectorIndex, VectorIndexConfig};

/// The content corpus domains in the Go recording's ingestion order, with the
/// expected file count per domain (fixtures/content/README.md: one markdown
/// source per domain sub-directory, listed `hr`, `product`, `eng`).
///
/// The order is load-bearing (module docs): document ids are the ingestion
/// order, and the fixture's first page (ids 1-3) is the `hr` source.
const SOURCES: [(&str, usize); 3] = [("hr", 3), ("product", 3), ("eng", 2)];

/// Content parity for the catalog tools and `search`: boot the product MCP
/// server over the ingested content corpus, drive `catalog_overview`,
/// `catalog_documents` (first page, `page_size` 3), `catalog_entities` and
/// `search` with the fixture's recording args, and compare each response
/// against its committed Go-oracle fixture after normalization.
#[test]
fn content_parity_matches_go_fixtures() {
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

    // 2. Scratch space + content corpus.
    let scratch = TempDir::new("content-parity");
    let corpus = scratch.join("corpus");
    write_content_corpus(&corpus).expect("write the content corpus");

    // 3. File-backed database + usearch engine (production configuration).
    let db = test_util::temp_file_db();
    let engine = UsearchEngine::create(scratch.join("vectors"), vector_index_config(dim))
        .expect("usearch engine create");
    let vectors: Arc<dyn VectorIndex> = Arc::new(engine);

    // 4. Ingest the corpus through the production pipeline, in the Go
    //    recording's source order (module docs).
    ingest_content_corpus(&db, &provider, &vectors, &corpus);

    // 5. Product MCP server over the pooled hybrid searcher.
    let searcher = Arc::new(PooledSearcher {
        db: db.clone(),
        search_config: search_config(),
        embed: provider.clone(),
        vectors: vectors.clone(),
    });
    let server = Server::new(
        "synopsis-content-parity".to_owned(),
        "0.1.0".to_owned(),
        db.clone(),
        searcher,
        Arc::new(GraphIndex::Unavailable),
    );

    // 6. One runtime: serve + drive, then graceful shutdown.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let responses = runtime
        .block_on(serve_and_drive(server))
        .expect("serve + drive");
    runtime.shutdown_timeout(Duration::from_secs(5));

    // 7. Content parity against the committed Go fixtures
    //    (fixtures/content/README.md).
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/content");

    // `catalog_overview`: recorded with `{}`; no reduction beyond normalize.
    let overview_fixture =
        load_fixture(&fixtures.join("catalog_overview.json")).expect("load the fixture");
    assert_content_parity(&overview_fixture, &responses.overview, "catalog_overview");

    // `catalog_documents`: recorded with `{"page_size": 3}`; the test-local
    // reduction (module docs) applies to both sides.
    let documents_fixture =
        load_fixture(&fixtures.join("catalog_documents.json")).expect("load the fixture");
    assert_content_parity(
        &reduce_catalog_documents(documents_fixture),
        &reduce_catalog_documents(responses.documents),
        "catalog_documents",
    );

    // `catalog_entities`: recorded with `{}` (NER disabled → empty listing).
    let entities_fixture =
        load_fixture(&fixtures.join("catalog_entities.json")).expect("load the fixture");
    assert_content_parity(&entities_fixture, &responses.entities, "catalog_entities");

    // `search`: recorded with `{"query": "Atlas dashboard builder", "top_k":
    // 5}`; `normalize` strips the exact scores and the wall-clock duration, so
    // the comparison is the result count plus the rank-ordered identity
    // (`document_id` / `chunk_id`) of each result (module docs).
    let search_fixture = load_fixture(&fixtures.join("search.json")).expect("load the fixture");
    assert_content_parity(&search_fixture, &responses.search, "search");
}

/// The four tool payloads (oracle-shaped JSON).
struct Responses {
    /// The `catalog_overview` payload.
    overview: Value,
    /// The `catalog_documents` payload (first page, `page_size` 3).
    documents: Value,
    /// The `catalog_entities` payload.
    entities: Value,
    /// The `search` payload (recorded with `{"query": "Atlas dashboard
    /// builder", "top_k": 5}`).
    search: Value,
}

/// Bind the product router to a random loopback port, drive the catalog tools
/// and `search` through the harness client with the fixture's recording args,
/// then shut the server down gracefully.
async fn serve_and_drive(server: Server) -> Result<Responses, String> {
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

    // The exact args the fixtures were recorded with (fixtures/content/README.md).
    let overview = client
        .call_tool("catalog_overview", Map::new())
        .await
        .map_err(|err| format!("catalog_overview: {err}"))?;
    let mut document_args = Map::new();
    document_args.insert("page_size".to_owned(), Value::from(3));
    let documents = client
        .call_tool("catalog_documents", document_args)
        .await
        .map_err(|err| format!("catalog_documents: {err}"))?;
    let entities = client
        .call_tool("catalog_entities", Map::new())
        .await
        .map_err(|err| format!("catalog_entities: {err}"))?;
    let mut search_args = Map::new();
    search_args.insert(
        "query".to_owned(),
        Value::String("Atlas dashboard builder".to_owned()),
    );
    search_args.insert("top_k".to_owned(), Value::from(5));
    let search = client
        .call_tool("search", search_args)
        .await
        .map_err(|err| format!("search: {err}"))?;

    let responses = Responses {
        overview: payload(&overview),
        documents: payload(&documents),
        entities: payload(&entities),
        search: payload(&search),
    };

    client
        .close()
        .await
        .map_err(|err| format!("close: {err}"))?;
    let _ = shutdown_tx.send(());
    serve
        .await
        .map_err(|err| format!("serve task join: {err}"))?
        .map_err(|err| format!("axum serve: {err}"))?;

    Ok(responses)
}

/// The tool's oracle-shaped JSON payload: the first text content block of the
/// `tools/call` result, parsed.
fn payload(result: &rmcp::model::CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("the tools answer with a JSON text block");
    serde_json::from_str(&text.text).expect("the catalog payload is valid JSON")
}

/// Test-local reduction for `catalog_documents` beyond
/// `content_parity::normalize` (module docs): `original_path` → file name
/// (the scratch directory is environment-defined) and `metadata` → the
/// `domain` key only (the Go bag's `file_size`/`modified_at`/`source_file`/
/// `source_type` fields live in dedicated columns in the Rust
/// re-architecture and are not part of the persisted extension bag). Applied
/// to BOTH the fixture and the live response.
fn reduce_catalog_documents(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    let Some(Value::Array(docs_ref)) = obj.get_mut("documents") else {
        return Value::Object(obj);
    };
    let docs = std::mem::take(docs_ref);
    *docs_ref = docs
        .into_iter()
        .map(|doc| {
            let Value::Object(mut entry) = doc else {
                return doc;
            };
            if let Some(path) = entry.get("original_path").and_then(Value::as_str) {
                let file_name = Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_owned());
                entry.insert("original_path".to_owned(), Value::String(file_name));
            }
            if let Some(Value::Object(metadata)) = entry.get_mut("metadata") {
                let domain = metadata
                    .get("domain")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                *metadata = Map::from_iter([("domain".to_owned(), domain)]);
            }
            Value::Object(entry)
        })
        .collect();
    Value::Object(obj)
}

/// Ingest the content corpus through the production pipeline (markdown source
/// → hybrid chunker → embeddings → vector index), one source per worker cycle
/// in the Go recording's order (module docs), and assert the pipeline's own
/// bookkeeping.
fn ingest_content_corpus(
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
        sources: SOURCES
            .iter()
            .map(|(domain, _)| SourceConfig {
                path: corpus.join(*domain).to_string_lossy().into_owned(),
                source_type: SourceType::Markdown,
                disabled: false,
                space: String::new(),
                domains: vec![(*domain).to_owned()],
                dataset: String::new(),
            })
            .collect(),
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

    // The queue is the only processing path: one reconcile + one worker cycle
    // per source, in the recording order. Draining all three sources at once
    // would index them in path order (`eng` first) and the document ids would
    // diverge from the fixture (module docs).
    let queue = DocumentJobQueue::new(db);
    let worker = DocumentWorker::new(db, &runner, ingest_cfg.max_retries);
    for (index, (domain, expected)) in SOURCES.iter().enumerate() {
        let source = &global.sources[index];
        let reconcile = queue
            .reconcile_source(&runner, &source.path)
            .expect("reconcile the source");
        assert_eq!(
            reconcile.indexed, *expected,
            "{domain}: the expected corpus files must be queued"
        );
        worker.run_once(1_000).expect("worker cycle");
    }

    let docs = db
        .with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .count(&DocumentFilter::default())
                .ok()
        })
        .expect("db connection")
        .expect("document count");
    assert_eq!(docs, 8, "all eight corpus documents ingested");
}

/// Search handle for the test server (mirrors the production
/// `cli::serve::server::PooledSearcher` and `tests/parity_test.rs`, minus the
/// graph expander): every call checks out a pooled connection and assembles a
/// fresh (cheap) hybrid searcher on it. Synchronous by contract — the mcp
/// dispatch boundary hops tool calls to `spawn_blocking` before invoking it.
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

/// ADR 0004 HNSW parameters for the test dimension (the index is never
/// persisted: the corpus stays in the RAM layer, the same state the
/// production `serve` flow runs in until an explicit index build).
fn vector_index_config(dim: usize) -> VectorIndexConfig {
    VectorIndexConfig::new(dim, 16, 100, 200).expect("ADR 0004 parameters validate")
}

/// Hybrid search configuration for the test (both legs enabled; explicit
/// values — `SearchConfig::default()` disables both legs). Same values the
/// Go recording config used (fixtures/content/README.md).
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
