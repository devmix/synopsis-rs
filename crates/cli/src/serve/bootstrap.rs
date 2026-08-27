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
//! a mismatch, and the check surfaces from `vectors::LanceEngine::open` as
//! [`VectorsError::DimensionMismatch`]. The [`Bootstrap::dimension_mismatch`]
//! flag carries the non-fatal signal to the serve/sync wiring (tasks 1.6/1.7);
//! [`build_runner`] sets it when the stored index disagrees with the
//! configured dimension.

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
    MediawikiSource, NerPrompts, Registry, Runner, RunnerParams, SummaryStats, UnstructuredSource,
    WebpageSource, load_ner_prompts,
};
use vectors::{LanceEngine, VectorIndex, VectorIndexConfig, VectorsError};

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
    /// the mismatch surfaces from `vectors::LanceEngine::open` as
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

/// Assembled application state shared by the `serve` and `sync` subcommands
/// (design D3).
pub struct Bootstrap {
    /// Effective configuration (defaults applied, validated, `--db` override
    /// applied).
    pub config: Config,
    /// Global ontology (`global.xml`); `None` when the section is absent or
    /// empty (no configured sources, no cross-domain linking).
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
    /// while consistent. Set by [`build_runner`] from
    /// `vectors::LanceEngine::open` via [`DimensionMismatch::from_vectors_error`].
    pub dimension_mismatch: Option<DimensionMismatch>,
}

/// Assembles the shared application state (design D3).
///
/// `db_path` is the effective database path override (the `--db` flag);
/// `None` keeps the config file's value. The assembly order mirrors the
/// oracle: config → domain discovery → onnx.yaml → `--db` override → main
/// database → model provisioning → provider → cache database.
///
/// # Errors
///
/// [`CliError::Config`] for config/domain/ontology failures,
/// [`CliError::Db`] for the main database, [`CliError::Embedding`] for model
/// provisioning and provider construction, [`CliError::Unsupported`] when the
/// configured embeddings mode is not available in this build.
pub fn bootstrap(cfg_path: &Path, db_path: Option<&Path>) -> Result<Bootstrap, CliError> {
    // 1. Config: load → defaults → validate.
    let mut config = load(cfg_path)?;
    config.apply_defaults();
    config.validate()?;

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

    // 3. Domain discovery (port of domain.DiscoveryWithLogger).
    let (global, domains) = discover_domains(&config.paths.global_config_path)?;

    // 4. External ONNX registry.
    let onnx = load_onnx_config(&config.paths.onnx_config)?;

    // 5. Effective database path: CLI flag > config file.
    if let Some(db_path) = db_path {
        config.database.path = db_path.to_string_lossy().into_owned();
    }

    // 6. Main database + migrations.
    let db = open_db(&config.db_path())?;

    // 7. Model provisioning + provider.
    ensure_model(&config, &onnx)?;
    let embed = new_onnx_provider(&config.embeddings.local, &config.paths.data_dir, &onnx)?;

    // 8. Cache database (nil-on-failure).
    let cache = open_cache(&config.cache_db_path());

    tracing::info!(
        config = %cfg_path.display(),
        db = %config.db_path().display(),
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

/// Opens the main database (creating the file and parent directories if
/// absent) and applies the embedded migrations — `Db::open` runs them
/// internally, `PRAGMA user_version` being the sole schema authority.
///
/// All failures are fatal: the squashed DDL migrations contain no vector
/// dimension, so the oracle's non-fatal `IsDimensionMismatchError` branch has
/// no counterpart at this layer (see [`Bootstrap::dimension_mismatch`]).
///
/// # Errors
///
/// [`CliError::Db`] when the file cannot be created or the migrations fail.
pub fn open_db(path: &Path) -> Result<Db, CliError> {
    let db = Db::open(path)?;
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
    let manager = ModelManager::new(&config.paths.data_dir, onnx);
    let model_path = manager.ensure_model(model_name)?;
    tracing::info!(name = model_name, path = %model_path.display(), "model ensured");
    Ok(())
}

/// Opens the separate cache database. Returns `None` (not an error) when it
/// cannot be opened — the application continues without caching (nil-on-
/// failure port of the oracle's `openCacheStore`).
pub fn open_cache(path: &Path) -> Option<Db> {
    match Db::open(path) {
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
    ontology_dir: &str,
) -> Result<(Option<GlobalConfig>, HashMap<String, DomainConfig>), CliError> {
    let global = load_global_config(ontology_dir)?;
    let mut domains: HashMap<String, DomainConfig> = HashMap::new();

    if ontology_dir.is_empty() {
        return Ok((global, domains));
    }
    let domain_dir = Path::new(ontology_dir).join("domains");
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
fn build_registry(chunking: &ChunkingConfig) -> Result<Registry, IngestionError> {
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
fn vectors_index_config(config: &Config) -> Result<VectorIndexConfig, VectorsError> {
    let tuning = config.vectors_config();
    let dim = i32::max(config.vector_dim(), 0) as usize;
    VectorIndexConfig::new(
        dim,
        tuning.m,
        tuning.ef_construction,
        tuning.num_partitions,
        tuning.nprobes,
        tuning.ef_search,
    )
}

/// Opens the vector-index engine (design D5), creating it on first run:
/// `LanceEngine::open` → `NotFound` → `LanceEngine::create`. The engine is
/// stored on the bootstrap so the runner and the later search wiring share
/// one instance.
///
/// A dimension mismatch between the configured embedding dimension and the
/// stored index is recorded on [`Bootstrap::dimension_mismatch`] (the
/// non-fatal signal the serve/sync wiring acts on) and also returned.
///
/// # Errors
///
/// [`CliError::Vectors`] when the index cannot be opened/created or its
/// stored dimension disagrees with the configuration.
fn open_vectors_engine(boot: &mut Bootstrap) -> Result<(), CliError> {
    let index_config = vectors_index_config(&boot.config)?;
    let path = Path::new(&boot.config.paths.data_dir);
    let engine = match LanceEngine::open(path, index_config) {
        Ok(engine) => engine,
        Err(VectorsError::NotFound(_)) => LanceEngine::create(path, index_config)?,
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
    tracing::info!(path = %path.display(), dim = index_config.dim, "vector index ready");
    boot.vectors = Some(Arc::new(engine));
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

/// Runs the initial multi-source sync (oracle `serve.go` initial-sync block):
/// assembles the runner (idempotent) and ingests every enabled source so the
/// index is up to date before the server accepts requests.
///
/// `rebuild` clears stored vectors before re-embedding (design D6
/// rebuild-clear) — the serve wiring passes `false`, the `sync --rebuild`
/// path passes `true`. Per-source failures are collected in
/// [`SummaryStats::errors`], never returned (oracle parity).
///
/// # Errors
///
/// [`CliError`] when the runner cannot be assembled (see [`build_runner`]).
pub fn initial_sync(boot: &mut Bootstrap, rebuild: bool) -> Result<SummaryStats, CliError> {
    let runner = build_runner(boot)?;
    tracing::info!(rebuild, "initial sync started");
    let stats = runner.ingest_all(rebuild);
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
    Ok(stats)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use config::onnx::{
        ModelFile, ModelInfo, OnnxModelsConfig, OnnxPlatformConfig, OnnxRuntimeConfig,
    };
    use config::preset::{Config, EmbeddingsMode, LocalEmbedding};
    use db::{ChunkDao, ConnectionOrTx, DocumentDao, DocumentFilter};
    use embedding::EmbeddingError;

    use super::*;

    /// A unique temporary directory that removes itself (and its contents)
    /// when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-test-{}-{}-{}",
                std::process::id(),
                tag,
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
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

    /// The ONNX registry platform key of this machine (oracle naming:
    /// `linux-amd64`, `darwin-arm64`, …).
    fn platform_key() -> (&'static str, &'static str) {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        (os, arch)
    }

    /// Writes a valid local-mode config into `dir` and returns its path.
    /// `data_dir` / `ontology` / `onnx` point inside `dir`.
    fn write_config(dir: &TempDir, mode: &str) -> PathBuf {
        let dir = dir.as_ref();
        let yaml = format!(
            r#"
embeddings:
  mode: {mode}
  local:
    model_name: bge-m3-int8
    vector_dim: 1024
  api:
    base_url: http://127.0.0.1:9999/v1
    model_name: test-model
    vector_dim: 1024
paths:
  data_dir: {data_dir}
  onnx_config: {onnx}
  global_config_path: {ontology}
"#,
            data_dir = dir.join("data").display(),
            onnx = dir.join("onnx.yaml").display(),
            ontology = dir.join("ontology").display(),
        );
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).expect("write config");
        path
    }

    /// Writes a minimal onnx.yaml registry (unroutable URLs — a test that
    /// reaches them has a bug) with the bge-m3-int8 model entry.
    fn write_onnx(dir: &TempDir) {
        let yaml = r#"
runtime:
  version: "1.28.0"
  platforms:
    - key: linux-amd64
      os: linux
      arch: amd64
      archive_url: http://127.0.0.1:1/onnxruntime.tgz
      archive_format: tgz
      library_name: libonnxruntime.so.1.28.0
      library_path: onnxruntime-pkg/lib/libonnxruntime.so.1.28.0
models:
  default: bge-m3-int8
  entries:
    - name: bge-m3-int8
      vector_dim: 1024
      files:
        - name: model.onnx
          url: http://127.0.0.1:1/model.onnx
"#;
        std::fs::write(dir.as_ref().join("onnx.yaml"), yaml).expect("write onnx.yaml");
    }

    /// An in-code [`OnnxConfig`] with the bge-m3-int8 registry entry.
    fn test_onnx() -> OnnxConfig {
        OnnxConfig {
            runtime: OnnxRuntimeConfig {
                version: "1.28.0".to_string(),
                platforms: vec![OnnxPlatformConfig {
                    key: "linux-amd64".to_string(),
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                    archive_url: "http://127.0.0.1:1/onnxruntime.tgz".to_string(),
                    archive_format: Default::default(),
                    library_name: "libonnxruntime.so.1.28.0".to_string(),
                    library_path: "onnxruntime-pkg/lib/libonnxruntime.so.1.28.0".to_string(),
                }],
            },
            models: OnnxModelsConfig {
                default: "bge-m3-int8".to_string(),
                entries: vec![ModelInfo {
                    name: "bge-m3-int8".to_string(),
                    display_name: String::new(),
                    description: String::new(),
                    version: String::new(),
                    vector_dim: 1024,
                    files: vec![ModelFile {
                        name: "model.onnx".to_string(),
                        url: "http://127.0.0.1:1/model.onnx".to_string(),
                        size_bytes: 0,
                        checksum: None,
                    }],
                    source: String::new(),
                    repo: String::new(),
                }],
            },
        }
    }

    fn local_config(data_dir: &Path) -> Config {
        Config {
            embeddings: config::preset::EmbeddingsConfig {
                mode: EmbeddingsMode::Local,
                local: LocalEmbedding {
                    model_name: "bge-m3-int8".to_string(),
                    model_path: String::new(),
                    tokenizer_path: String::new(),
                    vector_dim: 1024,
                },
                api: Default::default(),
                auto_rebuild_vectors: false,
            },
            paths: config::preset::PathsConfig {
                data_dir: data_dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // --- open_db -----------------------------------------------------------

    #[test]
    fn open_db_applies_migrations() {
        let dir = TempDir::new("open-db");
        let path = dir.as_ref().join("knowledge.db");
        let db = open_db(&path).expect("open_db succeeds");

        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .expect("query user_version")
            .expect("user_version row");
        assert_eq!(user_version, 1, "temp db must be migrated to v1");
    }

    // --- open_cache --------------------------------------------------------

    #[test]
    fn open_cache_success_returns_some() {
        let dir = TempDir::new("cache-ok");
        let path = dir.as_ref().join("cache.db");
        let cache = open_cache(&path);
        assert!(cache.is_some(), "valid path must open the cache db");
    }

    #[test]
    fn open_cache_invalid_path_returns_none() {
        let dir = TempDir::new("cache-fail");
        // A regular file as the parent directory: `Db::open` cannot create
        // the parent and fails → nil-on-failure port → `None`.
        let blocker = dir.as_ref().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let path = blocker.join("cache.db");
        assert!(open_cache(&path).is_none(), "invalid path must yield None");
    }

    // --- ensure_model ------------------------------------------------------

    #[test]
    fn ensure_model_skips_with_explicit_model_path() {
        let dir = TempDir::new("ensure-skip");
        let mut config = local_config(dir.as_ref());
        config.embeddings.local.model_path = dir
            .as_ref()
            .join("model.onnx")
            .to_string_lossy()
            .into_owned();
        ensure_model(&config, &test_onnx()).expect("explicit path is a no-op");
        // No download was attempted: the models directory was never created.
        assert!(!dir.as_ref().join("models").exists());
    }

    #[test]
    fn ensure_model_noop_in_api_mode() {
        let dir = TempDir::new("ensure-api");
        let mut config = local_config(dir.as_ref());
        config.embeddings.mode = EmbeddingsMode::Api;
        ensure_model(&config, &test_onnx()).expect("api mode is a no-op");
        assert!(!dir.as_ref().join("models").exists());
    }

    #[test]
    fn ensure_model_unknown_model_is_an_error() {
        let dir = TempDir::new("ensure-unknown");
        let mut config = local_config(dir.as_ref());
        config.embeddings.local.model_name = "no-such-model".to_string();
        let err = ensure_model(&config, &test_onnx()).expect_err("unknown model must fail");
        match err {
            CliError::Embedding(EmbeddingError::Model(msg)) => {
                assert!(
                    msg.contains("no-such-model"),
                    "message names the model: {msg}"
                );
            }
            other => panic!("expected Embedding(Model), got: {other:?}"),
        }
    }

    #[test]
    fn ensure_model_download_failure_is_an_error() {
        // The registry URL is unroutable (127.0.0.1:1): the download fails
        // deterministically without touching the network.
        let dir = TempDir::new("ensure-download");
        let config = local_config(dir.as_ref());
        assert!(
            ensure_model(&config, &test_onnx()).is_err(),
            "unroutable model URL must fail"
        );
    }

    // --- discover_domains --------------------------------------------------

    #[test]
    fn discover_domains_empty_dir_yields_empty() {
        let (global, domains) = discover_domains("").expect("empty dir is valid");
        assert!(global.is_none());
        assert!(domains.is_empty());
    }

    #[test]
    fn discover_domains_missing_directories_yield_empty() {
        let dir = TempDir::new("domains-missing");
        let (global, domains) =
            discover_domains(dir.as_ref().to_str().unwrap()).expect("missing dirs are valid");
        assert!(global.is_none(), "no global.xml → None");
        assert!(domains.is_empty(), "no domains/ dir → empty");
    }

    #[test]
    fn discover_domains_loads_global_and_domains() {
        let dir = TempDir::new("domains-full");
        let ontology = dir.as_ref();
        std::fs::write(
            ontology.join("global.xml"),
            r#"<global version="1.0"><sources></sources></global>"#,
        )
        .expect("write global.xml");
        std::fs::create_dir_all(ontology.join("domains")).expect("create domains dir");
        std::fs::write(
            ontology.join("domains").join("alpha.xml"),
            r#"<domain name="alpha" version="1.0"></domain>"#,
        )
        .expect("write alpha.xml");
        std::fs::write(
            ontology.join("domains").join("beta.xml"),
            r#"<domain name="beta" version="1.0"></domain>"#,
        )
        .expect("write beta.xml");
        // Non-XML files are ignored (oracle `filepath.Match("*.xml", …)`).
        std::fs::write(ontology.join("domains").join("README.md"), b"ignored").expect("write md");

        let (global, domains) =
            discover_domains(ontology.to_str().unwrap()).expect("discovery succeeds");
        assert!(global.is_some(), "global.xml must load");
        assert_eq!(domains.len(), 2);
        assert!(domains.contains_key("alpha"));
        assert!(domains.contains_key("beta"));
    }

    #[test]
    fn discover_domains_duplicate_name_is_an_error() {
        let dir = TempDir::new("domains-dup");
        let ontology = dir.as_ref();
        std::fs::create_dir_all(ontology.join("domains")).expect("create domains dir");
        std::fs::write(
            ontology.join("domains").join("one.xml"),
            r#"<domain name="dup" version="1.0"></domain>"#,
        )
        .expect("write one.xml");
        std::fs::write(
            ontology.join("domains").join("two.xml"),
            r#"<domain name="dup" version="1.0"></domain>"#,
        )
        .expect("write two.xml");

        let err = discover_domains(ontology.to_str().unwrap())
            .expect_err("duplicate domain name must fail");
        match err {
            CliError::Config(ConfigError::Validation { message }) => {
                assert!(message.contains("already registered"), "got: {message}");
            }
            other => panic!("expected Config(Validation), got: {other:?}"),
        }
    }

    // --- DimensionMismatch ---------------------------------------------------

    #[test]
    fn dimension_mismatch_from_vectors_error() {
        let mismatch = VectorsError::DimensionMismatch {
            expected: 1024,
            actual: 384,
        };
        assert_eq!(
            DimensionMismatch::from_vectors_error(&mismatch),
            Some(DimensionMismatch {
                expected: 1024,
                actual: 384
            })
        );
        let other = VectorsError::NotFound("data".to_string());
        assert!(DimensionMismatch::from_vectors_error(&other).is_none());
    }

    // --- build_runner / initial_sync -----------------------------------------

    /// Deterministic offline embedding provider (fixed vectors of `dim`).
    struct MockEmbed {
        dim: usize,
    }

    impl EmbeddingProvider for MockEmbed {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(texts.iter().map(|_| vec![0.25f32; self.dim]).collect())
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            "mock-embed"
        }
    }

    /// A local-mode config with a 4-dim embedding (matching [`MockEmbed`]),
    /// NER disabled, chunker parameters normalized.
    fn sync_config(data_dir: &Path) -> Config {
        let mut config = local_config(data_dir);
        config.embeddings.local.vector_dim = 4;
        config.ingestion.ner.disabled = true;
        config.apply_defaults();
        config
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

    /// Unwraps a successful `build_runner` result (`Runner` is not `Debug`,
    /// so `expect` is unavailable).
    fn must_build(boot: &mut Bootstrap) -> Runner<'_> {
        match build_runner(boot) {
            Ok(runner) => runner,
            Err(err) => panic!("build_runner must succeed: {err}"),
        }
    }

    /// A Bootstrap with a mock provider, a temp-file db, and no assembled
    /// pipeline collaborators (the state right before `build_runner`).
    fn test_bootstrap(config: Config, global: Option<GlobalConfig>, db: Db) -> Bootstrap {
        Bootstrap {
            config,
            global,
            domains: HashMap::new(),
            db,
            cache: None,
            embed: Arc::new(MockEmbed { dim: 4 }),
            onnx: test_onnx(),
            registry: None,
            prompts: None,
            vectors: None,
            dimension_mismatch: None,
        }
    }

    #[test]
    fn build_runner_minimal_config_does_not_panic() {
        let dir = TempDir::new("build-runner");
        let src = dir.as_ref().join("src");
        std::fs::create_dir_all(&src).expect("create source dir");
        let config = sync_config(&dir.as_ref().join("data"));
        let global = one_markdown_source(&src);
        let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
        let mut boot = test_bootstrap(config, Some(global), db);

        let runner = must_build(&mut boot);
        drop(runner);
        assert!(boot.vectors.is_some(), "engine must be opened");
        assert!(boot.registry.is_some(), "registry must be built");
        assert!(boot.prompts.is_some(), "prompts must be loaded");

        // Idempotent: a second call reuses the assembled collaborators.
        let runner = must_build(&mut boot);
        drop(runner);
    }

    #[test]
    fn build_runner_dimension_mismatch_sets_flag() {
        let dir = TempDir::new("dim-mismatch");
        let data_dir = dir.as_ref().join("data");
        // Pre-create the stored index with a different dimension.
        let stored = VectorIndexConfig::new(8, 16, 100, 256, 32, 200).expect("index config");
        LanceEngine::create(&data_dir, stored).expect("create stored index");

        let config = sync_config(&data_dir); // 4-dim embedding vs 8-dim index
        let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
        let mut boot = test_bootstrap(config, None, db);

        // `Runner` is not `Debug`, so the failure is unwrapped by hand.
        let err = match build_runner(&mut boot) {
            Err(err) => err,
            Ok(_) => panic!("build_runner must fail on dimension mismatch"),
        };
        assert!(
            matches!(
                err,
                CliError::Vectors(VectorsError::DimensionMismatch { .. })
            ),
            "got: {err:?}"
        );
        assert_eq!(
            boot.dimension_mismatch,
            Some(DimensionMismatch {
                expected: 4,
                actual: 8
            })
        );
    }

    #[test]
    fn initial_sync_creates_documents_and_chunks() {
        let dir = TempDir::new("initial-sync");
        let src = dir.as_ref().join("src");
        std::fs::create_dir_all(&src).expect("create source dir");
        std::fs::write(
            src.join("doc.md"),
            "# Title\n\nBody text of the document.\n",
        )
        .expect("write source document");
        let config = sync_config(&dir.as_ref().join("data"));
        let global = one_markdown_source(&src);
        let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
        let mut boot = test_bootstrap(config, Some(global), db);

        let stats = initial_sync(&mut boot, false).expect("initial_sync succeeds");
        assert_eq!(stats.sources_processed, 1, "one source processed");
        assert_eq!(stats.documents_created, 1, "one document created");
        assert!(
            stats.errors.is_empty(),
            "unexpected errors: {:?}",
            stats.errors
        );

        // Persisted rows (the task's count check).
        let docs = boot
            .db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).count(&DocumentFilter::default())
            })
            .expect("with_conn documents")
            .expect("count documents");
        assert_eq!(docs, 1, "one document row");
        let chunks = boot
            .db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).count())
            .expect("with_conn chunks")
            .expect("count chunks");
        assert!(chunks >= 1, "at least one chunk row: {chunks}");

        // The chunk vector landed in the engine.
        let vectors = boot.vectors.as_deref().expect("engine opened");
        assert_eq!(vectors.count().expect("engine count"), 1);
    }

    // --- bootstrap -----------------------------------------------------------

    #[test]
    fn bootstrap_missing_config_is_an_error() {
        let dir = TempDir::new("boot-missing-cfg");
        let err = bootstrap(dir.as_ref().join("nope.yaml").as_path(), None)
            .expect_err("missing config must fail");
        assert!(matches!(err, CliError::Config(_)), "got: {err:?}");
    }

    #[test]
    fn bootstrap_api_mode_is_unsupported() {
        let dir = TempDir::new("boot-api");
        let cfg_path = write_config(&dir, "api");
        let err = bootstrap(&cfg_path, None).expect_err("api mode must fail");
        assert!(
            matches!(err, CliError::Unsupported(ref msg) if msg.contains("api")),
            "got: {err:?}"
        );
    }

    #[test]
    fn bootstrap_invalid_config_is_an_error() {
        // local mode without vector_dim fails `Config::validate`.
        let dir = TempDir::new("boot-invalid");
        let yaml = r#"
embeddings:
  mode: local
  local:
    model_name: bge-m3-int8
"#;
        let cfg_path = dir.as_ref().join("config.yaml");
        std::fs::write(&cfg_path, yaml).expect("write config");
        let err = bootstrap(&cfg_path, None).expect_err("invalid config must fail");
        assert!(
            matches!(err, CliError::Config(ConfigError::Validation { .. })),
            "got: {err:?}"
        );
    }

    #[test]
    fn bootstrap_db_override_is_applied() {
        // The override is applied before the provider step; the provider
        // then fails (no ONNX library in the test env), but the database
        // file must exist at the OVERRIDDEN path, proving the order.
        let dir = TempDir::new("boot-db-override");
        write_onnx(&dir);
        let cfg_path = write_config(&dir, "local");
        let override_path = dir.as_ref().join("override").join("custom.db");

        bootstrap(&cfg_path, Some(override_path.as_path()))
            .expect_err("provider creation must fail in the test env");

        assert!(
            override_path.exists(),
            "db must be opened at the overridden path"
        );
        // And NOT at the config-derived default path.
        let default_path = dir.as_ref().join("data").join("knowledge.db");
        assert!(!default_path.exists(), "default path must be untouched");
    }

    // --- e2e (ignored): full bootstrap with a real model --------------------

    /// Run manually with real artifacts (no download — everything is
    /// pre-installed through the cache manifests):
    /// ```sh
    /// CLI_TEST_ONNXRUNTIME_LIB=/path/libonnxruntime.so \
    /// CLI_TEST_MODEL=/path/model.onnx \
    /// CLI_TEST_TOKENIZER=/path/tokenizer.json \
    /// CLI_TEST_DIM=1024 cargo test -p cli -- --ignored
    /// ```
    #[test]
    #[ignore = "requires a real onnxruntime library + bge-m3 model (see test docs)"]
    fn bootstrap_full_with_pre_installed_model() {
        use embedding::LibraryCache;

        let lib = std::env::var("CLI_TEST_ONNXRUNTIME_LIB")
            .expect("set CLI_TEST_ONNXRUNTIME_LIB to a real onnxruntime .so/.dylib");
        let model = std::env::var("CLI_TEST_MODEL").expect("set CLI_TEST_MODEL");
        let tokenizer = std::env::var("CLI_TEST_TOKENIZER").expect("set CLI_TEST_TOKENIZER");
        let dim: usize = std::env::var("CLI_TEST_DIM")
            .unwrap_or_else(|_| "1024".to_string())
            .parse()
            .expect("CLI_TEST_DIM must be a number");

        let dir = TempDir::new("boot-e2e");
        let data_dir = dir.as_ref().join("data");

        // Pre-install the ONNX runtime library through the cache manifest.
        let cache_dir = data_dir.join("onnxruntime");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");
        let lib_name = std::path::Path::new(&lib)
            .file_name()
            .expect("library file name")
            .to_string_lossy()
            .into_owned();
        std::fs::copy(&lib, cache_dir.join(&lib_name)).expect("copy library");
        let (os, arch) = platform_key();
        let platform = format!("{os}-{arch}");
        let manifest = LibraryCache {
            version: "1.28.0".to_string(),
            library_path: cache_dir.join(&lib_name),
            install_time: "2026-08-21T00:00:00Z".to_string(),
            platform: platform.clone(),
        };
        std::fs::write(
            cache_dir.join(".cache.json"),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");

        // Pre-install the model + tokenizer next to each other (explicit
        // model_path flow: the tokenizer is derived from the model's dir).
        let model_dir = data_dir.join("models").join("bge-m3-int8");
        std::fs::create_dir_all(&model_dir).expect("create model dir");
        std::fs::copy(&model, model_dir.join("model.onnx")).expect("copy model");
        std::fs::copy(&tokenizer, model_dir.join("tokenizer.json")).expect("copy tokenizer");

        // onnx.yaml registry matching the pre-installed library name.
        let onnx_yaml = format!(
            r#"
runtime:
  version: "1.28.0"
  platforms:
    - key: {platform}
      os: {os}
      arch: {arch}
      archive_url: http://127.0.0.1:1/onnxruntime.tgz
      archive_format: tgz
      library_name: {lib_name}
      library_path: onnxruntime-pkg/lib/{lib_name}
models:
  default: bge-m3-int8
  entries:
    - name: bge-m3-int8
      vector_dim: {dim}
      files:
        - name: model.onnx
          url: http://127.0.0.1:1/model.onnx
"#,
            platform = platform.as_str(),
        );
        std::fs::write(dir.as_ref().join("onnx.yaml"), onnx_yaml).expect("write onnx.yaml");

        // Config with the explicit model path (skips the auto-download).
        let yaml = format!(
            r#"
embeddings:
  mode: local
  local:
    model_name: bge-m3-int8
    model_path: {model_path}
    vector_dim: {dim}
paths:
  data_dir: {data_dir}
  onnx_config: {onnx}
  global_config_path: {ontology}
"#,
            data_dir = data_dir.display(),
            model_path = model_dir.join("model.onnx").display(),
            onnx = dir.as_ref().join("onnx.yaml").display(),
            ontology = dir.as_ref().join("ontology").display(),
        );
        let cfg_path = dir.as_ref().join("config.yaml");
        std::fs::write(&cfg_path, yaml).expect("write config");

        let boot = bootstrap(&cfg_path, None).expect("full bootstrap succeeds");

        // Migrations applied (the task's core assertion).
        let user_version: i64 = boot
            .db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .expect("query user_version")
            .expect("user_version row");
        assert_eq!(user_version, 1, "migrated to v1");

        // Cache DB opened (valid path).
        assert!(boot.cache.is_some(), "cache db must open");

        // No dimension mismatch at bootstrap time.
        assert!(boot.dimension_mismatch.is_none());

        // The provider produces vectors of the configured dimension.
        assert_eq!(boot.embed.vector_dim(), dim);
        let vectors = boot
            .embed
            .generate_embeddings(&["hello world".to_string()])
            .expect("embed succeeds");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), dim);
    }
}
