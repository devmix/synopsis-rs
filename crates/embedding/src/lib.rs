//! Embedding pipeline: ONNX Runtime lifecycle, model and library management,
//! tokenization, in-memory caching, and the bge-m3 int8 embedding provider
//! (see design decisions D1–D10).
//!
//! The entry point is the factory [`new_onnx_provider`]: it ensures the ONNX
//! Runtime shared library and the model files (downloading them on first use),
//! initializes the runtime, and returns a ready `Arc<dyn EmbeddingProvider>`.
//! Every public module item is re-exported at the crate root
//! (`embedding::OnnxProvider`, `embedding::EmbeddingCache`, …), so callers only
//! need the root namespace.
//!
//! All pipeline methods are synchronous and may block on network, disk, or
//! inference; async callers dispatch calls onto a blocking thread pool
//! (design D4).

pub mod cache;
pub mod downloader;
pub mod error;
pub mod library;
pub mod model;
pub mod provider;
pub mod runtime;
pub mod tokenizer;

pub use cache::{DEFAULT_MAX_SIZE, EmbeddingCache, cache_key};
pub use downloader::Downloader;
pub use error::EmbeddingError;
pub use library::{LibraryCache, LibraryManager, current_platform_key};
pub use model::{InstalledModel, ModelCache, ModelManager};
pub use provider::OnnxProvider;
pub use runtime::{build_session, init_runtime};
pub use tokenizer::{DEFAULT_MAX_LENGTH, Tokenized, Tokenizer};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::onnx::OnnxConfig;
use config::preset::LocalEmbedding;

/// Tokenizer file name shipped alongside model files.
const TOKENIZER_FILE_NAME: &str = "tokenizer.json";

/// Builds the ONNX embedding provider from configuration.
///
/// `workspace_dir` is the GLOBAL workspace root (`PathsConfig::workspace_dir`):
/// the models and the ONNX Runtime are shared across datasets
/// (storage-layout-restructure D3), never per-dataset.
///
/// Wiring order (a broken runtime fails fast, before a multi-GB model
/// download):
/// 1. [`LibraryManager::ensure_library`] — ensure the ONNX Runtime shared
///    library is installed under `workspace_dir` (downloaded on first use);
/// 2. [`init_runtime`] — load the library into this process;
/// 3. resolve the model from the `onnx.yaml` registry by `cfg.model_name`
///    (empty name → the registry default), its `tokenizer.json`, and the
///    vector dimension (the registry entry's `vector_dim`);
/// 4. [`build_session`] — create the inference session (design D7 options);
/// 5. [`Tokenizer::from_file`] + [`EmbeddingCache`] + [`OnnxProvider::new`] —
///    assemble the shareable provider.
///
/// The model is resolved from the `onnx.yaml` registry by `cfg.model_name`
/// (empty name → the registry default) and downloaded through
/// [`ModelManager::ensure_model`] when not installed yet. The provider's
/// vector dimension comes from the registry entry's `vector_dim` (a
/// non-positive value is a [`EmbeddingError::Config`] naming the model).
///
/// Synchronous: async callers dispatch this onto `spawn_blocking`.
///
/// # Errors
///
/// [`EmbeddingError::Config`] for an unsupported platform or a
/// non-positive `vector_dim` in a registry entry; [`EmbeddingError::Download`]
/// / [`EmbeddingError::Io`] when a library or model download fails;
/// [`EmbeddingError::Ort`] when the runtime library cannot be loaded or the
/// session cannot be built; [`EmbeddingError::Model`] for an unknown model
/// name or a missing model/tokenizer file; [`EmbeddingError::Tokenizer`] when
/// the tokenizer file is missing or malformed.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use config::onnx::OnnxConfig;
/// use config::preset::LocalEmbedding;
/// use embedding::{new_onnx_provider, EmbeddingProvider};
///
/// let cfg = LocalEmbedding {
///     model_name: "bge-m3-int8".to_string(),
/// };
/// let onnx = OnnxConfig::default();
/// let provider: Arc<dyn EmbeddingProvider> =
///     new_onnx_provider(&cfg, "workspace", &onnx).expect("provider builds");
/// ```
pub fn new_onnx_provider(
    cfg: &LocalEmbedding,
    workspace_dir: impl AsRef<Path>,
    onnx_cfg: &OnnxConfig,
) -> Result<Arc<dyn EmbeddingProvider>, EmbeddingError> {
    let workspace_dir = workspace_dir.as_ref();
    let lib_path = LibraryManager::new(workspace_dir, onnx_cfg)?.ensure_library()?;
    init_runtime(&lib_path)?;
    let resolved = resolve_model(cfg, workspace_dir, onnx_cfg)?;
    let session = build_session(&resolved.path)?;
    let tokenizer = Tokenizer::from_file(&resolved.tokenizer_path)?;
    let provider = OnnxProvider::new(
        session,
        tokenizer,
        EmbeddingCache::new(),
        resolved.name,
        resolved.vector_dim,
    )?;
    Ok(Arc::new(provider))
}

/// A resolved model: the ONNX file to load, the name used as the embedding
/// cache key, the tokenizer file, and the provider's vector dimension.
#[derive(Debug)]
struct ResolvedModel {
    path: PathBuf,
    name: String,
    tokenizer_path: PathBuf,
    vector_dim: usize,
}

/// Resolves the model, tokenizer, and dimension for the factory (step 3 of
/// [`new_onnx_provider`]). Kept separate from the factory so the
/// registry-driven decisions are unit-testable without the ONNX Runtime.
///
/// Resolves the name (`cfg.model_name`, empty → the `onnx.yaml`
/// default), takes the dimension from the registry entry (a non-positive
/// value is a [`EmbeddingError::Config`] naming the model), ensures the
/// model files are installed, and locates the tokenizer.
fn resolve_model(
    cfg: &LocalEmbedding,
    workspace_dir: &Path,
    onnx_cfg: &OnnxConfig,
) -> Result<ResolvedModel, EmbeddingError> {
    let manager = ModelManager::new(workspace_dir, onnx_cfg);
    let name = if cfg.model_name.trim().is_empty() {
        manager.default_model().to_string()
    } else {
        cfg.model_name.clone()
    };
    if name.is_empty() {
        return Err(EmbeddingError::Model(
            "no model name: set embeddings.local.model_name or a default in onnx.yaml".to_string(),
        ));
    }
    let info = manager.model(&name).ok_or_else(|| {
        EmbeddingError::Model(format!("model {name:?} not found in onnx.yaml registry"))
    })?;
    if info.vector_dim <= 0 {
        return Err(EmbeddingError::Config(format!(
            "model {name:?} in onnx.yaml declares a non-positive vector_dim {}",
            info.vector_dim
        )));
    }
    let vector_dim = info.vector_dim as usize;
    let path = manager.ensure_model(&name)?;
    let tokenizer_path = manager
        .path_for_file(&name, TOKENIZER_FILE_NAME)
        .ok_or_else(|| {
            EmbeddingError::Model(format!(
                "{TOKENIZER_FILE_NAME} not found in model directory {}",
                manager.model_dir(&name).display()
            ))
        })?;
    Ok(ResolvedModel {
        path,
        name,
        tokenizer_path,
        vector_dim,
    })
}

/// Generates vector embeddings for batches of texts.
///
/// Implementations are `Send + Sync` so a single instance can be shared (e.g.
/// behind `Arc`) across threads. Methods are synchronous and may block on
/// inference or disk I/O; async callers are responsible for dispatching calls
/// onto a blocking thread pool (design D4).
///
/// # Examples
///
/// ```
/// use embedding::EmbeddingProvider;
///
/// struct ConstProvider {
///     dim: usize,
/// }
///
/// impl EmbeddingProvider for ConstProvider {
///     fn generate_embeddings(
///         &self,
///         texts: &[String],
///     ) -> Result<Vec<Vec<f32>>, embedding::EmbeddingError> {
///         Ok(vec![vec![1.0; self.dim]; texts.len()])
///     }
///
///     fn vector_dim(&self) -> usize {
///         self.dim
///     }
///
///     fn name(&self) -> &'static str {
///         "const"
///     }
/// }
///
/// fn assert_send_sync<T: Send + Sync>() {}
/// assert_send_sync::<ConstProvider>();
///
/// let provider = ConstProvider { dim: 4 };
/// let vectors = provider.generate_embeddings(&["hello".to_string()]).unwrap();
/// assert_eq!(vectors.len(), 1);
/// assert_eq!(vectors[0].len(), 4);
/// assert_eq!(provider.name(), "const");
/// ```
pub trait EmbeddingProvider: Send + Sync {
    /// Generates one embedding vector per input text, in input order.
    ///
    /// Every returned vector has the length reported by [`vector_dim`][Self::vector_dim].
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Dimensionality of the vectors this provider produces.
    fn vector_dim(&self) -> usize;

    /// Human-readable provider name, for logging and diagnostics.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::PathBuf;

    use config::onnx::{
        ArchiveFormat, ModelFile, ModelInfo, OnnxConfig, OnnxModelsConfig, OnnxPlatformConfig,
        OnnxRuntimeConfig,
    };
    use config::preset::LocalEmbedding;

    use super::*;

    const LIB_NAME: &str = "libonnxruntime.so.1.28.0";
    const LIB_ENTRY: &str = "onnxruntime-pkg/lib/libonnxruntime.so.1.28.0";
    const FAKE_LIBRARY: &[u8] = b"fake-onnxruntime-shared-library-bytes";
    const FAKE_MODEL: &[u8] = b"fake-model-onnx-bytes";
    const FAKE_TOKENIZER: &[u8] = b"fake-tokenizer-json";

    /// The platform key of the machine running the tests (the CI matrix covers
    /// linux-amd64 / linux-arm64 / windows-amd64 / darwin-arm64).
    fn test_platform_key() -> String {
        current_platform_key().expect("test host must be a supported platform")
    }

    /// Registry fixture: a runtime entry for the current platform (the archive
    /// URL is unroutable — a test that reaches it has a bug) plus one model,
    /// optionally with a `tokenizer.json` file entry.
    fn test_config(archive_url: &str, tokenizer_file: bool) -> OnnxConfig {
        let key = test_platform_key();
        let (os, arch) = key.split_once('-').expect("key has os-arch shape");
        let mut files = vec![ModelFile {
            name: "model.onnx".to_string(),
            url: format!("{archive_url}/model.onnx"),
            size_bytes: FAKE_MODEL.len() as i64,
            ..Default::default()
        }];
        if tokenizer_file {
            files.push(ModelFile {
                name: TOKENIZER_FILE_NAME.to_string(),
                url: format!("{archive_url}/{TOKENIZER_FILE_NAME}"),
                size_bytes: FAKE_TOKENIZER.len() as i64,
                ..Default::default()
            });
        }
        OnnxConfig {
            runtime: OnnxRuntimeConfig {
                version: "1.28.0".to_string(),
                platforms: vec![OnnxPlatformConfig {
                    os: os.to_string(),
                    arch: arch.to_string(),
                    archive_url: archive_url.to_string(),
                    archive_format: ArchiveFormat::Zip,
                    library_name: LIB_NAME.to_string(),
                    library_path: LIB_ENTRY.to_string(),
                    key,
                }],
            },
            models: OnnxModelsConfig {
                default: "bge-m3-int8".to_string(),
                entries: vec![ModelInfo {
                    name: "bge-m3-int8".to_string(),
                    version: "1.0.0".to_string(),
                    vector_dim: 1024,
                    files,
                    ..Default::default()
                }],
            },
        }
    }

    fn local_cfg(model_name: &str) -> LocalEmbedding {
        LocalEmbedding {
            model_name: model_name.to_string(),
        }
    }

    /// Fresh per-test directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("embedding-factory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pre-installs the fake runtime library and its cache manifest so
    /// `ensure_library` is a cache hit (no network).
    fn preinstall_library(workspace_dir: &Path) -> PathBuf {
        let cache_dir = workspace_dir.join("onnxruntime");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let lib = cache_dir.join(LIB_NAME);
        std::fs::write(&lib, FAKE_LIBRARY).unwrap();
        let manifest = LibraryCache {
            version: "1.28.0".to_string(),
            library_path: lib.clone(),
            install_time: "2026-08-21T00:00:00Z".to_string(),
            platform: test_platform_key(),
        };
        std::fs::write(
            cache_dir.join(".cache.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        lib
    }

    /// Pre-installs the registry model (files + manifest) so `ensure_model` is
    /// a cache hit (no network).
    fn preinstall_model(workspace_dir: &Path, with_tokenizer: bool) {
        let model_dir = workspace_dir.join("models").join("bge-m3-int8");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("model.onnx"), FAKE_MODEL).unwrap();
        if with_tokenizer {
            std::fs::write(model_dir.join(TOKENIZER_FILE_NAME), FAKE_TOKENIZER).unwrap();
        }
        ModelCache::new(workspace_dir.join("models"))
            .mark_installed(InstalledModel {
                name: "bge-m3-int8".to_string(),
                version: "1.0.0".to_string(),
                vector_dim: 1024,
                installed_at: "2026-08-21T00:00:00Z".to_string(),
            })
            .unwrap();
    }

    // --- factory: error paths (no ONNX Runtime, no network) ---

    /// Unwraps the factory's error: `Arc<dyn EmbeddingProvider>` is not
    /// `Debug`, so `Result::unwrap_err` cannot be used.
    fn factory_error(cfg: &LocalEmbedding, dir: &Path, onnx: &OnnxConfig) -> EmbeddingError {
        match new_onnx_provider(cfg, dir, onnx) {
            Ok(_) => panic!("expected the factory to fail"),
            Err(err) => err,
        }
    }

    /// A platform key the current machine cannot match is rejected before any
    /// filesystem or network access.
    #[test]
    fn factory_unsupported_platform_is_config_error() {
        let dir = temp_dir("no-platform");
        let cfg = local_cfg("bge-m3-int8");
        let mut onnx = test_config("http://127.0.0.1:1", true);
        onnx.runtime.platforms[0].key = "solaris-sparc".to_string();

        let err = factory_error(&cfg, &dir, &onnx);

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        assert!(err.to_string().contains(&test_platform_key()), "got: {err}");
    }

    /// A missing runtime library with an unreachable download URL fails in
    /// `ensure_library` with a `Download` error naming the rejected host —
    /// before any `ort` API is touched, so the outcome is independent of the
    /// process-wide runtime state.
    #[test]
    fn factory_missing_library_is_download_error() {
        let dir = temp_dir("no-lib");
        let cfg = local_cfg("bge-m3-int8");
        let onnx = test_config("http://127.0.0.1:1", true);

        let err = factory_error(&cfg, &dir, &onnx);

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert!(err.to_string().contains("127.0.0.1"), "got: {err}");
    }

    /// A pre-installed library file that is not a loadable shared library
    /// fails the factory with a clear, structured error — never a panic.
    ///
    /// The variant depends on this process's `ort` state. The pinned pre-release
    /// `ort` manages the dynamic loader as a process-wide singleton that a
    /// failed load *poisons*: its internal `OnceLock` marks the slot
    /// "initialized" even when the dlopen failed, so after the first failed
    /// `init_from` in a process every later `init_from` returns `Ok` without
    /// loading anything. Consequently:
    /// - when this test performs the process's first `init_runtime` call, the
    ///   dlopen fails and the factory returns `EmbeddingError::Ort` naming the
    ///   library file, before any model work;
    /// - when another test (e.g. `runtime::tests`) already consumed that first
    ///   attempt, `init_runtime` is a silent no-op and the factory proceeds to
    ///   the (uninstalled) model download, which the SSRF guard rejects with
    ///   `EmbeddingError::Download` naming the rejected host.
    ///
    /// Both variants are asserted; the real success path is covered by the
    /// `#[ignore]`d end-to-end test below.
    #[test]
    fn factory_broken_runtime_library_returns_clear_error() {
        let dir = temp_dir("bad-lib");
        preinstall_library(&dir);
        let onnx = test_config("http://127.0.0.1:1", true);
        let cfg = local_cfg("bge-m3-int8");

        let err = factory_error(&cfg, &dir, &onnx);

        match err {
            EmbeddingError::Ort(msg) => {
                assert!(
                    msg.contains(LIB_NAME),
                    "message should name the failed library, got: {msg}"
                );
                assert!(
                    !dir.join("models").exists(),
                    "no model work after a runtime failure"
                );
            }
            EmbeddingError::Download(msg) => {
                assert!(
                    msg.contains("127.0.0.1"),
                    "message should name the rejected host, got: {msg}"
                );
            }
            other => panic!("expected Ort or Download, got: {other:?}"),
        }
    }

    // --- resolve_model: config decisions (no ONNX Runtime, no network) ---

    /// An unknown registry name is a `Model` error naming the model.
    #[test]
    fn resolve_model_unknown_name_is_model_error() {
        let dir = temp_dir("unknown-model");
        let cfg = local_cfg("does-not-exist");
        let onnx = test_config("http://127.0.0.1:1", true);

        let err = resolve_model(&cfg, &dir, &onnx).unwrap_err();

        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
        assert!(err.to_string().contains("does-not-exist"), "got: {err}");
    }

    /// An empty name resolves to the registry default; the tokenizer is
    /// located next to the installed model files.
    #[test]
    fn resolve_model_empty_name_uses_registry_default() {
        let dir = temp_dir("default-name");
        preinstall_model(&dir, true);
        let cfg = local_cfg("");
        let onnx = test_config("http://127.0.0.1:1", true);

        let resolved = resolve_model(&cfg, &dir, &onnx).unwrap();

        assert_eq!(resolved.name, "bge-m3-int8");
        assert_eq!(resolved.vector_dim, 1024);
        assert_eq!(resolved.path, dir.join("models/bge-m3-int8/model.onnx"));
        assert_eq!(
            resolved.tokenizer_path,
            dir.join("models/bge-m3-int8/tokenizer.json")
        );
    }

    /// A model installed without a `tokenizer.json` is a `Model` error naming
    /// the missing file (no download is attempted for it).
    #[test]
    fn resolve_model_missing_tokenizer_is_model_error() {
        let dir = temp_dir("no-tokenizer");
        preinstall_model(&dir, false);
        let cfg = local_cfg("bge-m3-int8");
        let onnx = test_config("http://127.0.0.1:1", false);

        let err = resolve_model(&cfg, &dir, &onnx).unwrap_err();

        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
        assert!(err.to_string().contains(TOKENIZER_FILE_NAME), "got: {err}");
    }

    /// A registry entry with a non-positive `vector_dim` is a `Config` error
    /// naming the model and the declared value.
    #[test]
    fn resolve_model_non_positive_registry_dim_is_config_error() {
        let dir = temp_dir("bad-registry-dim");
        let cfg = local_cfg("bge-m3-int8");
        let mut onnx = test_config("http://127.0.0.1:1", true);
        onnx.models.entries[0].vector_dim = 0;

        let err = resolve_model(&cfg, &dir, &onnx).unwrap_err();

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        let msg = err.to_string();
        assert!(msg.contains("bge-m3-int8"), "got: {msg}");
        assert!(msg.contains("0"), "got: {msg}");
    }

    /// End-to-end with the real ONNX Runtime library and model.
    ///
    /// Run manually:
    /// `EMBEDDING_TEST_ONNXRUNTIME_LIB=/path/libonnxruntime.so \
    ///  EMBEDDING_TEST_MODEL=/path/model.onnx \
    ///  EMBEDDING_TEST_TOKENIZER=/path/tokenizer.json \
    ///  EMBEDDING_TEST_DIM=1024 cargo test -p embedding -- --ignored`
    ///
    /// The runtime library and model are pre-installed through the cache
    /// manifests (no download); the model is resolved via the registry flow.
    #[test]
    #[ignore]
    fn factory_builds_real_provider_end_to_end() {
        let lib = std::env::var("EMBEDDING_TEST_ONNXRUNTIME_LIB")
            .expect("set EMBEDDING_TEST_ONNXRUNTIME_LIB to a real onnxruntime .so/.dylib");
        let model = std::env::var("EMBEDDING_TEST_MODEL").expect("set EMBEDDING_TEST_MODEL");
        let tokenizer =
            std::env::var("EMBEDDING_TEST_TOKENIZER").expect("set EMBEDDING_TEST_TOKENIZER");
        let dim: usize = std::env::var("EMBEDDING_TEST_DIM")
            .unwrap_or_else(|_| "1024".to_string())
            .parse()
            .unwrap();

        let dir = temp_dir("e2e");
        let cache_dir = dir.join("onnxruntime");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::copy(&lib, cache_dir.join(LIB_NAME)).unwrap();
        let manifest = LibraryCache {
            version: "1.28.0".to_string(),
            library_path: cache_dir.join(LIB_NAME),
            install_time: "2026-08-21T00:00:00Z".to_string(),
            platform: test_platform_key(),
        };
        std::fs::write(
            cache_dir.join(".cache.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        // Pre-install the model files in the registry model directory.
        let model_dir = dir.join("models").join("bge-m3-int8");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::copy(&model, model_dir.join("model.onnx")).unwrap();
        std::fs::copy(&tokenizer, model_dir.join(TOKENIZER_FILE_NAME)).unwrap();
        ModelCache::new(dir.join("models"))
            .mark_installed(InstalledModel {
                name: "bge-m3-int8".to_string(),
                version: "1.0.0".to_string(),
                vector_dim: dim as i32,
                installed_at: "2026-08-21T00:00:00Z".to_string(),
            })
            .unwrap();

        // Set the registry entry's vector_dim to match the model.
        let mut onnx = test_config("http://127.0.0.1:1", true);
        onnx.models.entries[0].vector_dim = dim as i32;

        let cfg = local_cfg("bge-m3-int8");
        let provider = new_onnx_provider(&cfg, &dir, &onnx).unwrap();

        assert_eq!(provider.vector_dim(), dim);
        let texts = vec!["hello world".to_string(), "second text".to_string()];
        let vectors = provider.generate_embeddings(&texts).unwrap();
        assert_eq!(vectors.len(), 2);
        for vector in &vectors {
            assert_eq!(vector.len(), dim);
            let norm: f64 = vector
                .iter()
                .map(|v| f64::from(*v) * f64::from(*v))
                .sum::<f64>()
                .sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
        }
        // A repeat call is served from the cache (same values, no error).
        let again = provider.generate_embeddings(&texts).unwrap();
        assert_eq!(again, vectors);
    }
}
