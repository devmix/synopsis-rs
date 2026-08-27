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
//! this task leaves it `None`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use config::preset::EmbeddingsMode;
use config::{
    Config, ConfigError, DomainConfig, GlobalConfig, OnnxConfig, load, load_domain_config,
    load_global_config, load_onnx_config,
};
use db::Db;
use embedding::{EmbeddingProvider, ModelManager, new_onnx_provider};
use vectors::VectorsError;

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
    /// Vector dimension mismatch detected when opening the ANN index; `None`
    /// while consistent. Set by the serve/sync wiring (tasks 1.6/1.7) from
    /// `vectors::LanceEngine::open` via
    /// [`DimensionMismatch::from_vectors_error`]; this task leaves it `None`.
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
