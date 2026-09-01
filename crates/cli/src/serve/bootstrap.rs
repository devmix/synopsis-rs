//! Bootstrap assembly (design D3): config → domains → onnx.yaml → db →
//! model → provider → cache.
//!
//! Oracle mapping: `../synopsis/cmd/app/cmd.go` (`bootstrap`, `openDatabase`,
//! `ensureEmbeddingModel`, `openCacheStore`) plus
//! `internal/domain/domain_registry.go` (`DiscoveryWithLogger`).
//!
//! Architectural note on the dimension mismatch: in the Go oracle the
//! mismatch surfaces from the vec0 SQLite virtual table at migrate time
//! (`database.IsDimensionMismatchError`). In this codebase vectors live in
//! the ANN index, not SQLite — the squashed DDL migrations can never produce
//! a mismatch, and the check surfaces from the vector engine's `open` (the
//! [`create_vector_engine`] factory) as [`VectorsError::DimensionMismatch`].
//! The [`Bootstrap::dimension_mismatch`] flag carries the non-fatal signal
//! to the serve wiring (tasks 1.6/1.7); [`build_runner`] sets it when the
//! stored index disagrees with the configured dimension.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use config::preset::{ChunkingConfig, EmbeddingsMode};
use config::{
    Config, ConfigError, DomainConfig, GlobalConfig, OnnxConfig, load, load_domain_config,
    load_global_config, load_onnx_config,
};
use db::Db;
use embedding::{EmbeddingProvider, ModelManager, new_onnx_provider};
use ingestion::{
    IngestionError, JsonChunker, JsonSource, MarkdownChunker, MarkdownSource, MediawikiChunker,
    MediawikiSource, NerPrompts, Registry, Runner, RunnerParams, UnstructuredSource, WebpageSource,
    load_ner_prompts,
};
use vectors::{VectorIndex, VectorIndexConfig, VectorsError, create_vector_engine};

use crate::error::CliError;

/// Default local embedding model name (oracle `ensureEmbeddingModel` fallback).
const DEFAULT_MODEL_NAME: &str = "bge-m3-int8";

/// Vector dimension mismatch between the configuration and the stored index.
///
/// The Rust analogue of the oracle's `database.DimensionMismatchError`
/// (`ConfigDim` / `DBDim`): `expected` is what the configuration declares
/// (what new vectors will have), `actual` is what the stored index has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DimensionMismatch {
    /// Dimensionality the configuration declares.
    pub expected: usize,
    /// Dimensionality of the stored index.
    pub actual: usize,
}

impl DimensionMismatch {
    /// Extracts the mismatch from a vectors error; `None` for every other
    /// variant.
    ///
    /// The Rust analogue of the oracle's `database.IsDimensionMismatchError`:
    /// the mismatch surfaces from the vector engine's `open` as
    /// [`VectorsError::DimensionMismatch`], never from the DDL migrations.
    #[must_use]
    pub fn from_vectors_error(err: &VectorsError) -> Option<Self> {
        match err {
            VectorsError::DimensionMismatch { expected, actual } => Some(Self {
                expected: *expected,
                actual: *actual,
            }),
            _ => None,
        }
    }
}

/// Assembled application state of the `serve` subcommand (design D3).
pub struct Bootstrap {
    /// Effective configuration (defaults applied, validated).
    pub config: Config,
    /// Global ontology (`global.xml`); `None` when the section is absent or
    /// empty (no configured sources, no cross-domain linking), or when there
    /// is no active dataset (design D2 no-data semantics).
    pub global: Option<GlobalConfig>,
    /// Domain ontologies by name (discovered from `domains/*.xml`).
    pub domains: HashMap<String, DomainConfig>,
    /// Main SQLite database (migrated; `PRAGMA user_version` is the sole
    /// schema authority).
    pub db: Db,
    /// Separate LLM-NER cache database; `None` when it could not be opened
    /// (nil-on-failure port of the oracle's `openCacheStore`).
    pub cache: Option<Db>,
    /// Ready embedding provider.
    pub embed: Arc<dyn EmbeddingProvider>,
    /// External ONNX registry (`onnx.yaml`).
    pub onnx: OnnxConfig,
    /// Source-type registry (oracle `NewRunner` bookkeeping); `None` until
    /// [`build_runner`] assembles it.
    pub registry: Option<Registry>,
    /// NER prompt templates (loaded from `paths.prompts_path` with the
    /// embedded fallback); `None` until [`build_runner`] loads them.
    pub prompts: Option<NerPrompts>,
    /// Opened vector-index engine (design D5); `None` until [`build_runner`]
    /// opens it (created on first run).
    pub vectors: Option<Arc<dyn VectorIndex>>,
    /// Vector dimension mismatch detected when opening the ANN index; `None`
    /// while consistent. Set by [`build_runner`] from the vector engine's
    /// `open` via [`DimensionMismatch::from_vectors_error`].
    pub dimension_mismatch: Option<DimensionMismatch>,
}

/// Whether an active dataset is configured (design D2): a non-empty
/// `dataset.name` AND an existing dataset directory
/// `<workspace_dir>/datasets/<name>`. Without an active dataset the
/// application runs with no data (no ontology loaded, nothing to ingest).
#[must_use]
pub fn has_active_dataset(config: &Config) -> bool {
    if config.dataset.name.is_empty() {
        return false;
    }
    let root = Path::new(config.paths.workspace_dir.as_str())
        .join("datasets")
        .join(config.dataset.name.as_str());
    root.is_dir()
}

/// Assembles the shared application state (design D3).
///
/// `dataset_override` is the `--dataset` flag value; when present it wins
/// over `config.dataset.name` before any path resolution. The knowledge-DB
/// path is derived, not configurable (revision 1.1):
/// `<workspace_dir>/datasets/<name>/state/db/knowledge.db`. The assembly
/// order mirrors the oracle: config → dataset gate → domain discovery →
/// onnx.yaml → main database → model provisioning → provider → cache
/// database.
///
/// No-data semantics (design D2): without an active dataset
/// ([`has_active_dataset`]) the ontology load is skipped, the server runs
/// with no data, and a warning is logged — never an error.
///
/// # Errors
///
/// [`CliError::Config`] for config/domain/ontology failures,
/// [`CliError::Db`] for the main database, [`CliError::Embedding`] for model
/// provisioning and provider construction, [`CliError::Unsupported`] when the
/// configured embeddings mode is not available in this build.
pub fn bootstrap(cfg_path: &Path, dataset_override: Option<&str>) -> Result<Bootstrap, CliError> {
    // 1. Config: load → defaults → validate.
    let mut config = load(cfg_path)?;
    config.apply_defaults();
    config.validate()?;

    // 1b. `--dataset` CLI flag wins over the config value.
    if let Some(name) = dataset_override {
        config.dataset.name = name.to_string();
    }

    // 2. Embeddings mode gate: this build ships the local ONNX provider only.
    //    (The oracle supports an api provider; see the change report.)
    match &config.embeddings.mode {
        EmbeddingsMode::Local => {}
        EmbeddingsMode::Api => {
            return Err(CliError::Unsupported(
                "embeddings.mode \"api\" is not supported by this build; use the local ONNX \
                 provider"
                    .to_string(),
            ));
        }
        // `validate()` rejects unknown modes; the guard keeps the match
        // exhaustive if the enum grows.
        EmbeddingsMode::Unknown(mode) => {
            return Err(CliError::Config(ConfigError::Validation {
                message: format!("unknown embeddings mode \"{mode}\""),
            }));
        }
    }

    // 3. Dataset gate (design D2) + domain discovery (port of
    //    domain.DiscoveryWithLogger). The ontology directory is per-dataset:
    //    <workspace_dir>/datasets/<name>/ontology. Without an active dataset
    //    there is no ontology to load and nothing to ingest: run with no
    //    data (a warning, never an error).
    let (global, domains) = if has_active_dataset(&config) {
        discover_domains(config.dataset.ontology_path(&config.paths.workspace_dir))?
    } else {
        tracing::warn!(
            dataset = %config.dataset.name,
            "no dataset configured, running with no data"
        );
        (None, HashMap::new())
    };

    // 4. External ONNX registry.
    let onnx = load_onnx_config(&config.paths.onnx_config)?;

    // 5. Knowledge database: the path is derived from workspace_dir +
    //    dataset.name (not configurable, revision 1.1).
    let knowledge_db = config.dataset.db_path(&config.paths.workspace_dir);

    // 6. Main database + migrations.
    let db = open_db(&knowledge_db)?;

    // 7. Model provisioning + provider.
    ensure_model(&config, &onnx)?;
    let embed = new_onnx_provider(&config.embeddings.local, &config.paths.workspace_dir, &onnx)?;

    // 8. Cache database (nil-on-failure).
    let cache = open_cache(&config.cache_db_path());

    tracing::info!(
        config = %cfg_path.display(),
        db = %knowledge_db.display(),
        "configuration loaded"
    );
    Ok(Bootstrap {
        config,
        global,
        domains,
        db,
        cache,
        embed,
        onnx,
        registry: None,
        prompts: None,
        vectors: None,
        dimension_mismatch: None,
    })
}

/// Opens the main KNOWLEDGE database (creating the file and parent
/// directories if absent) and applies the knowledge migrations —
/// `Db::open_knowledge` runs them internally, `PRAGMA user_version` being
/// the sole schema authority.
///
/// All failures are fatal: the squashed DDL migrations contain no vector
/// dimension, so the oracle's non-fatal `IsDimensionMismatchError` branch has
/// no counterpart at this layer (see [`Bootstrap::dimension_mismatch`]).
///
/// # Errors
///
/// [`CliError::Db`] when the file cannot be created or the migrations fail.
pub fn open_db(path: &Path) -> Result<Db, CliError> {
    let db = Db::open_knowledge(path)?;
    tracing::info!(path = %path.display(), "database ready");
    Ok(db)
}

/// Provisions the configured local embedding model, auto-downloading it on
/// first use (oracle `ensureEmbeddingModel`).
///
/// No-op in api mode; skips the download when an explicit `model_path` is
/// set (legacy mode). Deliberately does NOT mutate the config: the Rust
/// provider factory resolves the registry model itself (with the dimension
/// cross-check against `onnx.yaml`), so the oracle's
/// `cfg.Embeddings.Local.ModelPath = modelPath` mutation has no counterpart.
///
/// # Errors
///
/// [`CliError::Embedding`] when the model is not in the registry or the
/// download / cache write fails.
pub fn ensure_model(config: &Config, onnx: &OnnxConfig) -> Result<(), CliError> {
    if !matches!(config.embeddings.mode, EmbeddingsMode::Local) {
        return Ok(());
    }
    let local = &config.embeddings.local;
    if !local.model_path.is_empty() {
        tracing::info!(path = %local.model_path, "using explicit model path (skipping auto-download)");
        return Ok(());
    }
    let model_name = if local.model_name.trim().is_empty() {
        DEFAULT_MODEL_NAME
    } else {
        local.model_name.as_str()
    };
    let manager = ModelManager::new(&config.paths.workspace_dir, onnx);
    let model_path = manager.ensure_model(model_name)?;
    tracing::info!(name = model_name, path = %model_path.display(), "model ensured");
    Ok(())
}

/// Opens the separate cache database (cache schema ONLY — never the
/// knowledge schema; `Db::open_cache` applies the cache migrations, task
/// 1.9, storage-layout-restructure). Returns `None` (not an error) when it
/// cannot be opened — the application continues without caching (nil-on-
/// failure port of the oracle's `openCacheStore`).
pub fn open_cache(path: &Path) -> Option<Db> {
    match Db::open_cache(path) {
        Ok(db) => {
            tracing::info!(path = %path.display(), "cache database opened");
            Some(db)
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "open cache database (continuing without cache)"
            );
            None
        }
    }
}

/// Discovers the global ontology and the per-domain ontologies under
/// `<ontology_dir>/domains/*.xml` (port of `domain.DiscoveryWithLogger`).
///
/// File-presence semantics follow the oracle: an empty directory name or a
/// missing `global.xml` / `domains/` directory yields an empty result, not
/// an error. A domain name that appears in more than one file is a
/// validation error (oracle `Register`).
///
/// # Errors
///
/// [`CliError::Config`] for I/O, XML, validation or regex failures of the
/// ontology files, and for a duplicate domain name.
pub fn discover_domains(
    ontology_dir: impl AsRef<Path>,
) -> Result<(Option<GlobalConfig>, HashMap<String, DomainConfig>), CliError> {
    let dir = ontology_dir.as_ref();
    let global = load_global_config(dir)?;
    let mut domains: HashMap<String, DomainConfig> = HashMap::new();

    if dir.as_os_str().is_empty() {
        return Ok((global, domains));
    }
    let domain_dir = dir.join("domains");
    let entries = match std::fs::read_dir(&domain_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((global, domains)),
        Err(err) => {
            return Err(ConfigError::Io {
                path: domain_dir.to_string_lossy().into_owned(),
                source: err,
            }
            .into());
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| ConfigError::Io {
            path: domain_dir.to_string_lossy().into_owned(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| ConfigError::Io {
            path: domain_dir.to_string_lossy().into_owned(),
            source,
        })?;
        if file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        // Oracle parity: `filepath.Match("*.xml", name)` — case-sensitive.
        if path.extension().is_none_or(|ext| ext != "xml") {
            continue;
        }
        let cfg = load_domain_config(&path)?;
        if domains.contains_key(&cfg.name) {
            return Err(ConfigError::Validation {
                message: format!("domain {:?} already registered", cfg.name),
            }
            .into());
        }
        domains.insert(cfg.name.clone(), cfg);
    }
    Ok((global, domains))
}

/// Builds the source-type registry from the chunking config (oracle
/// `NewRunner` registry construction): every supported format gets its
/// parser + chunker pair, keyed by the `global.xml` `type` word.
///
/// Public because the serve wiring (task 1.6) builds the registry as an
/// owned local before constructing the `Runner` (the runner borrows the
/// local, keeping the bootstrap state unpinned).
///
/// # Errors
///
/// [`IngestionError::AlreadyRegistered`] on an internal registration
/// conflict (programmer error — the registry is built from a fixed source
/// list).
pub fn build_registry(chunking: &ChunkingConfig) -> Result<Registry, IngestionError> {
    let md = chunking.markdown.clone();
    let json = chunking.json.clone();
    let mut registry = Registry::new();
    registry.register(
        MarkdownSource::SOURCE_TYPE,
        Box::new(MarkdownSource::new(Box::new(MarkdownChunker::new(
            md.clone(),
        )))),
    )?;
    registry.register(
        JsonSource::SOURCE_TYPE,
        Box::new(JsonSource::new(Box::new(JsonChunker::new(json.clone())))),
    )?;
    registry.register(
        MediawikiSource::SOURCE_TYPE,
        Box::new(MediawikiSource::new(Box::new(MediawikiChunker::new(
            md.clone(),
        )))),
    )?;
    registry.register(
        WebpageSource::SOURCE_TYPE,
        Box::new(WebpageSource::new(Box::new(MarkdownChunker::new(
            md.clone(),
        )))),
    )?;
    registry.register(
        UnstructuredSource::SOURCE_TYPE,
        Box::new(UnstructuredSource::new(
            Box::new(MarkdownChunker::new(md)),
            Box::new(JsonChunker::new(json)),
        )),
    )?;
    Ok(registry)
}

/// Maps the effective config to the ANN index configuration (design D5):
/// the embedding dimension is authoritative (the engine stores the model's
/// vectors, so a `vectors.dim` that disagreed would only fail at insert
/// time), the ANN tuning fields come from the `vectors:` section.
///
/// Shared with the serve wiring (task 1.6), which recreates the engine with
/// this config on a dimension-mismatch auto-rebuild.
pub fn vectors_index_config(config: &Config) -> Result<VectorIndexConfig, VectorsError> {
    let tuning = config.vectors_config();
    let dim = i32::max(config.vector_dim(), 0) as usize;
    let mut index_config =
        VectorIndexConfig::new(dim, tuning.m, tuning.ef_construction, tuning.ef_search)?;
    // The scalar quantization is a usearch-engine parameter; the config
    // default is "bf16".
    index_config = index_config.with_quantization(tuning.quantization);
    // The usearch tuning section (usearch-wal-persistence task 3.9, ADR
    // 0004 §10): mapped field by field (dependency direction D7: config
    // cannot depend on vectors). An absent section stays `None` (the
    // engine resolves [`vectors::UsearchConfig::default`]).
    if let Some(usearch) = &tuning.usearch {
        index_config.usearch = Some(vectors::UsearchConfig {
            max_segment_vectors: usearch.max_segment_vectors,
            compaction_stale_threshold: usearch.compaction_stale_threshold,
            search_threads: usearch.search_threads,
        });
    }
    Ok(index_config)
}

/// Opens the vector-index engine (design D5), creating it on first run:
/// the [`create_vector_engine`] factory resolves the `vectors.engine`
/// selection (add-usearch-ann-engine) and performs the
/// `open` → `NotFound` → `create` cascade. The engine is stored on the
/// bootstrap so the runner and the later search wiring share one instance.
///
/// A dimension mismatch between the configured embedding dimension and the
/// stored index is recorded on [`Bootstrap::dimension_mismatch`] (the
/// non-fatal signal the serve/sync wiring acts on) and also returned.
///
/// Idempotent: a second call reuses the stored engine. The serve wiring
/// (task 1.6) calls this directly before building the `Runner` so it can
/// capture the engine's `Arc` into an owned local.
///
/// # Errors
///
/// [`CliError::Vectors`] when the index cannot be opened/created (including
/// an unknown `vectors.engine` value) or its stored dimension disagrees
/// with the configuration.
pub fn open_vectors_engine(boot: &mut Bootstrap) -> Result<(), CliError> {
    let index_config = vectors_index_config(&boot.config)?;
    // The ANN index is per-dataset and per-engine:
    // <workspace_dir>/datasets/<name>/state/vectors/<engine> (task 1.5).
    // The factory resolves the engine subdirectory from the name.
    let path = boot
        .config
        .dataset
        .vectors_path(&boot.config.paths.workspace_dir);
    // Runtime engine selection (add-usearch-ann-engine, design.md): the
    // `vectors.engine` field ("usearch"; absent → the default engine). The
    // factory performs the open → NotFound → create cascade.
    let engine_name = boot.config.vectors_config().engine;
    // The WAL database (usearch-wal-persistence task 3.9, ADR 0004 §3):
    // the knowledge.db path (the same file [`bootstrap`] opened in step 6)
    // — the engine journals its mutations to the `usearch_vectors_log`
    // table there (migrations 3+4).
    let wal_db = boot
        .config
        .dataset
        .db_path(&boot.config.paths.workspace_dir);
    let engine = match create_vector_engine(
        engine_name.as_deref().unwrap_or(""),
        &path,
        &index_config,
        Some(wal_db.as_path()),
    ) {
        Ok(engine) => engine,
        Err(err) => {
            if let Some(mismatch) = DimensionMismatch::from_vectors_error(&err) {
                tracing::error!(
                    expected = mismatch.expected,
                    actual = mismatch.actual,
                    "vector dimension mismatch"
                );
                boot.dimension_mismatch = Some(mismatch);
            }
            return Err(CliError::Vectors(err));
        }
    };
    tracing::info!(
        path = %boot
            .config
            .dataset
            .vectors_engine_path(&boot.config.paths.workspace_dir, engine_name.as_deref().unwrap_or("usearch"))
            .display(),
        dim = index_config.dim,
        "vector index ready"
    );
    boot.vectors = Some(engine);
    Ok(())
}

/// Assembles the ingestion [`Runner`] from the bootstrap (design D3/D4):
/// opens the vector engine, builds the source registry, loads the NER
/// prompts, and fills in the 11-field [`RunnerParams`] (domain discovery was
/// already done by [`bootstrap`]).
///
/// Idempotent: the collaborators are stored on the bootstrap and reused on
/// later calls — a second call must not re-open the engine.
///
/// # Errors
///
/// [`CliError::Vectors`] when the engine cannot be opened or the stored
/// index disagrees with the configured dimension (also recorded on
/// [`Bootstrap::dimension_mismatch`]), [`CliError::Config`] when the NER
/// prompt templates fail to load, [`CliError::Unsupported`] for an internal
/// registry registration conflict.
pub fn build_runner(boot: &mut Bootstrap) -> Result<Runner<'_>, CliError> {
    if boot.vectors.is_none() {
        open_vectors_engine(boot)?;
    }
    if boot.registry.is_none() {
        let registry = build_registry(&boot.config.ingestion.chunking)
            .map_err(|err| CliError::Unsupported(format!("source registry: {err}")))?;
        boot.registry = Some(registry);
    }
    if boot.prompts.is_none() {
        let prompts = load_ner_prompts(&boot.config.paths.prompts_path).map_err(|err| {
            CliError::Config(ConfigError::Validation {
                message: format!("NER prompts: {err}"),
            })
        })?;
        boot.prompts = Some(prompts);
    }

    let config = &boot.config;
    let vectors = match boot.vectors.as_deref() {
        Some(vectors) => vectors,
        None => {
            // Unreachable: opened above. The explicit error keeps the match
            // exhaustive without an `expect`.
            return Err(CliError::Vectors(VectorsError::NotFound(
                "<vector engine not opened>".to_string(),
            )));
        }
    };
    let (registry, prompts) = match (&boot.registry, &boot.prompts) {
        (Some(registry), Some(prompts)) => (registry, prompts),
        _ => {
            // Unreachable: assembled above.
            return Err(CliError::Unsupported(
                "<runner collaborators not assembled>".to_string(),
            ));
        }
    };
    Ok(Runner::new(RunnerParams {
        db: &boot.db,
        ingest_cfg: &config.ingestion,
        global: boot.global.as_ref(),
        domains: &boot.domains,
        registry,
        embed: boot.embed.as_ref(),
        vectors,
        prompts,
        linker_cfg: &config.linker,
        prompts_path: &config.paths.prompts_path,
        llm_cache: boot.cache.clone(),
    }))
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Db` (the r2d2 pool) and `dyn EmbeddingProvider` are not `Debug`;
        // the fields are summarized instead of skipped.
        f.debug_struct("Bootstrap")
            .field("config", &self.config)
            .field("global", &self.global)
            .field("domains", &self.domains)
            .field("db", &"<Db>")
            .field("cache", &self.cache.as_ref().is_some())
            .field("embed", &self.embed.name())
            .field("onnx", &self.onnx)
            .field("registry", &self.registry.is_some())
            .field("prompts", &self.prompts.is_some())
            .field("vectors", &self.vectors.is_some())
            .field("dimension_mismatch", &self.dimension_mismatch)
            .finish()
    }
}
