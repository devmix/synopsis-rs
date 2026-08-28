//! Load-test subcommand: synthetic data generation, DB fill, and MCP
//! tool benchmarking (design D11).
//!
//! Oracle: `../synopsis/cmd/app/loadtest.go` + `internal/benchmark/`.

pub mod filler;
pub mod generator;
pub mod report;
pub mod runner;

use std::path::Path;
use std::sync::Arc;

use graph::GraphIndex;
use mcp::Server;

use crate::error::CliError;
use crate::serve::bootstrap::{Bootstrap, bootstrap, open_vectors_engine};
use crate::serve::server::{PooledSearcher, recreate_vectors_engine};

use self::filler::{FillOptions, fill};
use self::generator::Generator;
use self::report::{FillSummary, GraphSummary, Report};
use self::runner::Runner;

/// Parsed load-test request from CLI flags.
pub struct LoadTestRequest {
    /// Dataset scale: `small`, `medium` or `large`.
    pub scale: String,
    /// PRNG seed for deterministic data generation.
    pub seed: i64,
    /// Measured iterations per tool case.
    pub iterations: usize,
    /// Write the report as JSON to this path.
    pub json: Option<String>,
    /// Benchmark an existing database without generating data.
    pub no_fill: bool,
}

/// Runs the load-test subcommand.
///
/// `cfg_path` is the resolved configuration file path; `dataset` is the
/// optional `--dataset` override (wins over `config.dataset.name`).
pub fn run_load_test(
    cfg_path: &Path,
    dataset: Option<&str>,
    req: &LoadTestRequest,
) -> Result<(), CliError> {
    // 1. Parse scale.
    let scale = generator::Scale::parse(&req.scale)
        .map_err(|e| CliError::Unsupported(format!("invalid scale: {e}")))?;

    // 2. Bootstrap (config, DB, embedding, vectors).
    let mut boot = bootstrap(cfg_path, dataset)?;
    require_embedding_model(&boot)?;

    // 3. Open the vector engine (dimension mismatch → recreate unless
    //    `--no-fill`, where it is fatal).
    open_vectors_with_recreate(&mut boot, req.no_fill)?;
    let vectors = boot
        .vectors
        .clone()
        .ok_or_else(|| CliError::Unsupported("vector engine not assembled".to_owned()))?;

    // 4. Load the knowledge graph.
    let graph = Arc::new(GraphIndex::from_db(&boot.db, &boot.config.graph)?);

    // 5. Build the MCP server.
    let searcher = Arc::new(PooledSearcher::new(
        boot.db.clone(),
        boot.config.search.clone(),
        boot.config.graph.clone(),
        boot.embed.clone(),
        vectors.clone(),
        graph.clone(),
    ));
    let server = Arc::new(Server::new(
        boot.config.server.name.clone(),
        boot.config.server.version.clone(),
        boot.db.clone(),
        searcher,
        graph.clone(),
    ));

    // 6. Generate dataset + fill DB.
    let mut generator = Generator::new(req.seed);
    let ds = generator.generate(&scale).map_err(CliError::Unsupported)?;

    let mut fill_summary = None;
    if !req.no_fill {
        let batch_size = boot.config.ingestion.batch_size.max(1) as usize;
        let opts = FillOptions::new(batch_size);
        let fill_report = fill(&boot.db, &ds, boot.embed.as_ref(), vectors.as_ref(), &opts)
            .map_err(CliError::Unsupported)?;
        fill_summary = Some(FillSummary {
            duration_ms: fill_report.duration_ms,
            vectors: fill_report.vectors,
            tables: fill_report.tables,
        });
    }

    // 7. Load samples.
    let samples = if req.no_fill {
        runner::load_samples_static()
    } else {
        runner::load_samples_from_db(&boot.db, &ds)
    };

    // 8. Run benchmark.
    let runner = Runner::new(server, samples)
        .with_iterations(req.iterations)
        .with_pages_per_call(3);
    let tool_stats = runner.run();

    // 9. Collect graph summary.
    let graph_summary = graph_summary(&graph);

    // 10. Build and print report.
    let report = Report {
        scale,
        seed: req.seed,
        filled: !req.no_fill,
        iterations: req.iterations,
        pages_per_call: 3,
        fill: fill_summary,
        graph: graph_summary,
        tools: tool_stats,
    };

    report::print(&report);

    // 11. Write JSON if requested.
    if let Some(ref path) = req.json {
        let p = Path::new(path);
        report::write_json(&report, p).map_err(CliError::Unsupported)?;
    }

    Ok(())
}

/// Opens the vector engine with dimension-mismatch handling (design D11).
///
/// When the stored index's dimension disagrees with the configured embedding
/// dimension and `--no-fill` is off, the vector table is dropped and
/// recreated — the fill phase below re-embeds every chunk (the Rust form of
/// the oracle's `rebuildVectorsIfNeeded` → `ReEmbedChunks`). Under
/// `--no-fill` the stored index is the very data being benchmarked, so the
/// mismatch is fatal.
fn open_vectors_with_recreate(boot: &mut Bootstrap, no_fill: bool) -> Result<(), CliError> {
    match open_vectors_engine(boot) {
        Ok(()) => Ok(()),
        Err(_) if boot.dimension_mismatch.is_some() && !no_fill => {
            tracing::info!("vector dimension mismatch; recreating the vector table");
            recreate_vectors_engine(boot)?;
            open_vectors_engine(boot)
        }
        Err(_) if boot.dimension_mismatch.is_some() => Err(CliError::Unsupported(
            "vector dimension mismatch detected under --no-fill: the stored vector index \
             disagrees with the configured embedding dimension. Re-run without --no-fill \
             to rebuild the vectors, or delete the stored vector index"
                .to_string(),
        )),
        Err(err) => Err(err),
    }
}

/// Verifies the embedding model is installed; fails loudly if not.
fn require_embedding_model(boot: &Bootstrap) -> Result<(), CliError> {
    let manager = embedding::model::ModelManager::new(&boot.config.paths.workspace_dir, &boot.onnx);
    let model_name = boot.config.embeddings.local.model_name.as_str();
    if !manager.is_installed(model_name) {
        return Err(CliError::Unsupported(format!(
            "embedding model {model_name:?} is not installed. \
              Run `synopsis model download` first, or place the model in {}",
            boot.config.paths.workspace_dir
        )));
    }
    Ok(())
}

fn graph_summary(graph: &Arc<GraphIndex>) -> GraphSummary {
    match graph.ready() {
        Some(g) => GraphSummary {
            load_ms: 0.0,
            nodes: g.node_count(),
            edges: g.edge_count(),
        },
        None => GraphSummary {
            load_ms: 0.0,
            nodes: 0,
            edges: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use config::{Config, OnnxConfig};
    use db::test_util::in_memory_db;
    use embedding::EmbeddingProvider;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use vectors::{LanceEngine, VectorIndexConfig};

    use crate::serve::bootstrap::open_db;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-loadtest-{tag}-{}-{id}",
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

    /// A 4-dim Bootstrap with a temp-file DB (matching [`FakeEmbed`]). The
    /// dataset is active (design D2): named `edtech` with the directory
    /// present.
    fn test_bootstrap(workspace_dir: &Path) -> Bootstrap {
        let mut config = Config::default();
        config.embeddings.local.model_name = "bge-m3-int8".to_string();
        config.embeddings.local.vector_dim = 4;
        config.paths.workspace_dir = workspace_dir.to_string_lossy().into_owned();
        config.dataset.name = "edtech".to_string();
        config.apply_defaults();
        std::fs::create_dir_all(config.dataset.state_path(&config.paths.workspace_dir))
            .expect("create dataset state dir");
        let db = open_db(workspace_dir.join("knowledge.db").as_path()).expect("open db");
        Bootstrap {
            config,
            global: None,
            domains: std::collections::HashMap::new(),
            db,
            cache: None,
            embed: Arc::new(FakeEmbed::new(4)),
            onnx: OnnxConfig::default(),
            registry: None,
            prompts: None,
            vectors: None,
            dimension_mismatch: None,
        }
    }

    /// Pre-creates a stored ANN index with the given dimension at the
    /// fixture's dataset vectors path (dataset `edtech`).
    fn stored_index(workspace_dir: &Path, dim: usize) {
        let stored = VectorIndexConfig::new(dim, 16, 100, 256, 32, 200).expect("index config");
        let vectors_path = workspace_dir
            .join("datasets")
            .join("edtech")
            .join("state")
            .join("vectors");
        LanceEngine::create(&vectors_path, stored).expect("create stored index");
    }

    /// A no-op embedding provider for tests.
    struct FakeEmbed {
        dim: usize,
        calls: AtomicUsize,
    }

    impl FakeEmbed {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl EmbeddingProvider for FakeEmbed {
        fn generate_embeddings(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, embedding::EmbeddingError> {
            self.calls.fetch_add(texts.len(), Ordering::SeqCst);
            Ok((0..texts.len()).map(|_| vec![0.1f32; self.dim]).collect())
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            "fake"
        }
    }

    #[test]
    fn generator_determinism_across_scales() {
        for scale_name in ["small", "medium"] {
            let scale = generator::Scale::parse(scale_name).unwrap();
            let mut g = Generator::new(42);
            let ds = g.generate(&scale).unwrap();
            assert_eq!(ds.documents.len(), scale.documents);
            assert_eq!(ds.chunks.len(), scale.chunks);
            assert_eq!(ds.entities.len(), scale.entities);
            assert_eq!(ds.facts.len(), scale.facts);
            assert!(!ds.samples.queries.is_empty());
            assert!(!ds.samples.entity_ids.is_empty());
        }
    }

    #[test]
    fn dispatch_all_12_tools_with_in_memory_db() {
        let db = in_memory_db();
        let dim = 4;
        let embed = Arc::new(FakeEmbed::new(dim));
        let vectors = Arc::new(
            vectors::LanceEngine::create(
                std::env::temp_dir().join(format!("lt-test-{}", std::process::id())),
                VectorIndexConfig::new(dim, 16, 100, 1, 1, 10).unwrap(),
            )
            .unwrap(),
        );
        let graph_config = config::preset::GraphConfig::default();
        let graph =
            Arc::new(GraphIndex::from_db(&db, &graph_config).unwrap_or(GraphIndex::Unavailable));
        let searcher = Arc::new(PooledSearcher::new(
            db.clone(),
            config::preset::SearchConfig::default(),
            graph_config.clone(),
            embed.clone(),
            vectors.clone(),
            graph.clone(),
        ));
        let server = Arc::new(Server::new(
            "test".to_owned(),
            "0.1".to_owned(),
            db.clone(),
            searcher,
            graph,
        ));

        let tools = [
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
        for tool in &tools {
            let args = match *tool {
                "search" => Some(serde_json::json!({"query": "test", "top_k": 5})),
                "get_document_context" => Some(serde_json::json!({"document_id": "1"})),
                "get_chunk_by_id" => Some(serde_json::json!({"chunk_id": "1"})),
                "get_fact_by_id" => Some(serde_json::json!({"fact_id": "1"})),
                "get_entity_dossier" => Some(serde_json::json!({"entity_id": "1"})),
                "get_entity_relations" => Some(serde_json::json!({"entity_id": "1"})),
                "get_entity_links" => Some(serde_json::json!({"entity_id": "1"})),
                "search_entities_by_type" => Some(serde_json::json!({"entity_type": "employee"})),
                _ => None,
            };
            let _ = server.dispatch(tool, args.as_ref());
        }
    }

    #[test]
    fn static_queries_fallback() {
        assert_eq!(runner::STATIC_NO_FILL_QUERIES.len(), 16);
    }

    // --- dimension-mismatch handling ------------------------------------------

    /// A stored 8-dim index + no `--no-fill`: the vector table is dropped and
    /// recreated with the configured 4-dim dimension, the mismatch flag is
    /// cleared, and the fresh engine holds no stale vectors.
    #[test]
    fn dimension_mismatch_recreates_vector_table_by_default() {
        let dir = TempDir::new("dim-recreate");
        let workspace_dir = dir.as_ref().join("workspace");
        stored_index(&workspace_dir, 8);

        let mut boot = test_bootstrap(&workspace_dir); // 4-dim config vs 8-dim index
        open_vectors_with_recreate(&mut boot, false).expect("recreate path must succeed");

        assert!(boot.dimension_mismatch.is_none(), "mismatch flag cleared");
        let vectors = boot.vectors.as_deref().expect("engine recreated");
        assert_eq!(vectors.count().expect("count"), 0, "stale vectors dropped");
    }

    /// The same stored 8-dim index + `--no-fill`: the mismatch is fatal,
    /// because the stored index is the data being benchmarked.
    #[test]
    fn dimension_mismatch_is_fatal_under_no_fill() {
        let dir = TempDir::new("dim-fatal");
        let workspace_dir = dir.as_ref().join("workspace");
        stored_index(&workspace_dir, 8);

        let mut boot = test_bootstrap(&workspace_dir);
        let err = open_vectors_with_recreate(&mut boot, true)
            .expect_err("the mismatch must be fatal under --no-fill");
        assert!(
            matches!(
                err,
                CliError::Unsupported(ref msg) if msg.contains("dimension mismatch")
            ),
            "got: {err:?}"
        );
    }

    /// A stored index whose dimension matches the configuration opens
    /// normally, even under `--no-fill` (no recreate, no error).
    #[test]
    fn consistent_index_opens_without_recreate() {
        let dir = TempDir::new("dim-consistent");
        let workspace_dir = dir.as_ref().join("workspace");
        stored_index(&workspace_dir, 4);

        let mut boot = test_bootstrap(&workspace_dir);
        open_vectors_with_recreate(&mut boot, true).expect("consistent index must open");

        assert!(boot.dimension_mismatch.is_none());
        let vectors = boot.vectors.as_deref().expect("engine opened");
        assert_eq!(vectors.count().expect("count"), 0);
    }
}
