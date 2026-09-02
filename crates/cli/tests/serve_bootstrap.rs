//! Integration tests for `cli::serve::bootstrap` (moved verbatim from
//! `crates/cli/src/serve/bootstrap.rs`, test-hygiene task 1.3). Import paths
//! rewritten from `use super::*` to the public `cli::serve::bootstrap` API.

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use config::onnx::{ModelFile, ModelInfo, OnnxModelsConfig, OnnxPlatformConfig, OnnxRuntimeConfig};
use config::preset::{Config, EmbeddingsMode, LocalEmbedding};
use config::{ConfigError, GlobalConfig, OnnxConfig};
use db::Db;
use embedding::{EmbeddingError, EmbeddingProvider};
use ingestion::Runner;
use vectors::{ENGINE_USEARCH, VectorIndexConfig, VectorsError, create_vector_engine};

use cli::error::CliError;
use cli::serve::bootstrap::*;

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
/// `workspace_dir` / `onnx` point inside `dir`; the dataset directory
/// `<workspace_dir>/datasets/edtech` is absent here, so the no-data gate
/// (design D2) skips the ontology load.
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
  workspace_dir: {workspace_dir}
  onnx_config: {onnx}
dataset:
  name: edtech
 "#,
        workspace_dir = dir.join("workspace").display(),
        onnx = dir.join("onnx.yaml").display(),
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

fn local_config(workspace_dir: &Path) -> Config {
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
            workspace_dir: workspace_dir.to_string_lossy().into_owned(),
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
    assert_eq!(
        user_version, 1,
        "temp db must be migrated (the squashed init migration sets user_version 1)"
    );
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
    // A regular file as the parent directory: `Db::open_cache` cannot
    // create the parent and fails → nil-on-failure port → `None`.
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

    let err =
        discover_domains(ontology.to_str().unwrap()).expect_err("duplicate domain name must fail");
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

// --- build_runner ----------------------------------------------------------

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
fn sync_config(workspace_dir: &Path) -> Config {
    let mut config = local_config(workspace_dir);
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
    let mut config = sync_config(&dir.as_ref().join("workspace"));
    config.dataset.name = "edtech".to_string();
    let global = one_markdown_source(&src);
    // The knowledge db at the derived dataset path (the same file the
    // production bootstrap opens): the engine's WAL wiring (task 3.9)
    // points the factory at it.
    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir)).expect("open db");
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
    let workspace_dir = dir.as_ref().join("workspace");
    let mut config = sync_config(&workspace_dir); // 4-dim embedding vs 8-dim index
    config.dataset.name = "edtech".to_string();
    // Pre-create the stored index (at the dataset's vectors path) with a
    // different dimension.
    let stored = VectorIndexConfig::new(8, 16, 100, 256).expect("index config");
    create_vector_engine(
        ENGINE_USEARCH,
        &config.dataset.vectors_path(&config.paths.workspace_dir),
        &stored,
        None,
    )
    .expect("create stored index");

    let db = open_db(&config.dataset.db_path(&config.paths.workspace_dir)).expect("open db");
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

// --- vectors_index_config / open_vectors_engine (task 3.9) ---------------

/// Task 3.9: `vectors_index_config` maps the `vectors.usearch` preset
/// section field by field (dependency direction D7: config cannot
/// depend on vectors).
#[test]
fn vectors_index_config_maps_the_usearch_section() {
    let dir = TempDir::new("index-config");
    let mut config = local_config(dir.as_ref());
    config.vectors = Some(config::preset::VectorsConfig {
        usearch: Some(config::preset::UsearchConfig {
            max_segment_vectors: 42,
            compaction_stale_threshold: 25,
            search_threads: 2,
        }),
        ..Default::default()
    });

    let index_config = vectors_index_config(&config).expect("mapping succeeds");
    let usearch = index_config
        .usearch
        .expect("the usearch section must be mapped");
    assert_eq!(usearch.max_segment_vectors, 42);
    assert_eq!(usearch.compaction_stale_threshold, 25);
    assert_eq!(usearch.search_threads, 2);
}

/// Task 3.9: `open_vectors_engine` wires the WAL database end-to-end —
/// the factory receives the knowledge.db path (ADR 0004 §3): insert →
/// shutdown save (task 3.7) → restart the engine on the same db + dir
/// → the data is visible.
#[test]
fn open_vectors_engine_wires_the_wal_db_end_to_end() {
    let dir = TempDir::new("wiring-wal");
    let mut config = sync_config(&dir.as_ref().join("workspace"));
    config.dataset.name = "edtech".to_string();
    let db_path = config.dataset.db_path(&config.paths.workspace_dir);
    let db = open_db(&db_path).expect("open db");
    let mut boot = test_bootstrap(config, None, db);

    open_vectors_engine(&mut boot).expect("engine opens");
    let engine = boot.vectors.clone().expect("engine stored");
    engine.insert(7, &[0.5f32; 4]).expect("insert");
    // The shutdown save point (task 3.7): persist the RAM layer.
    engine.build_index().expect("save the RAM layer");

    // Restart: a fresh engine on the same db + dir (the open cascade)
    // sees the saved row.
    drop(engine);
    let index_config = vectors_index_config(&boot.config).expect("index config");
    let path = boot
        .config
        .dataset
        .vectors_path(&boot.config.paths.workspace_dir);
    let engine_name = boot.config.vectors_config().engine;
    let restarted = create_vector_engine(
        engine_name.as_deref().unwrap_or(""),
        &path,
        &index_config,
        Some(db_path.as_path()),
    )
    .expect("restart on the same db + dir");
    assert_eq!(
        restarted.count().expect("count"),
        1,
        "the saved row must be visible after the restart"
    );
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
fn bootstrap_dataset_override_is_applied() {
    // The override is applied before the provider step; the provider
    // then fails (no ONNX library in the test env), but the database
    // file must exist at the OVERRIDDEN dataset's derived path, proving
    // the order.
    let dir = TempDir::new("boot-dataset-override");
    write_onnx(&dir);
    let cfg_path = write_config(&dir, "local");
    let ws = dir.as_ref().join("workspace");
    let overridden_db = ws
        .join("datasets")
        .join("other")
        .join("state")
        .join("db")
        .join("knowledge.db");
    let config_db = ws
        .join("datasets")
        .join("edtech")
        .join("state")
        .join("db")
        .join("knowledge.db");

    bootstrap(&cfg_path, Some("other")).expect_err("provider creation must fail in the test env");

    assert!(
        overridden_db.exists(),
        "db must be opened at the overridden dataset path"
    );
    assert!(!config_db.exists(), "config dataset path must be untouched");
}

#[test]
fn has_active_dataset_requires_name_and_directory() {
    let dir = TempDir::new("dataset-active");
    // Empty name (the default): no dataset.
    let config = local_config(dir.as_ref());
    assert!(!has_active_dataset(&config), "empty name must mean no data");

    let mut config = local_config(dir.as_ref());
    config.dataset.name = "edtech".to_string();
    // Named but absent: no dataset.
    assert!(
        !has_active_dataset(&config),
        "absent directory must mean no data"
    );

    // Named and present: active (workspace_dir is `dir` itself).
    std::fs::create_dir_all(dir.as_ref().join("datasets").join("edtech"))
        .expect("create dataset dir");
    assert!(
        has_active_dataset(&config),
        "named + present must be active"
    );
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
    let workspace_dir = dir.as_ref().join("workspace");

    // Pre-install the ONNX runtime library through the cache manifest.
    let cache_dir = workspace_dir.join("onnxruntime");
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
    let model_dir = workspace_dir.join("models").join("bge-m3-int8");
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

    // Config with the explicit model path (skips the auto-download). The
    // database path is derived inside the temp workspace dir (dataset
    // edtech), so this (ignored) e2e run stays inside the temp dir.
    let yaml = format!(
        r#"
embeddings:
  mode: local
  local:
    model_name: bge-m3-int8
    model_path: {model_path}
    vector_dim: {dim}
paths:
  workspace_dir: {workspace_dir}
  onnx_config: {onnx}
dataset:
  name: edtech
"#,
        workspace_dir = workspace_dir.display(),
        model_path = model_dir.join("model.onnx").display(),
        onnx = dir.as_ref().join("onnx.yaml").display(),
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
    assert_eq!(
        user_version, 1,
        "migrated (the squashed init migration sets user_version 1)"
    );

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
