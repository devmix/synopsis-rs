//! Embedding pipeline: ONNX Runtime lifecycle, model and library management,
//! tokenization, in-memory caching, and the bge-m3 int8 embedding provider.
//!
//! Oracle mapping: `../synopsis/internal/embedding` + `../synopsis/internal/onnx`
//! (design.md D1/D5). Per the migration principle the Go oracle is a reference for
//! behavior and contracts only — this crate is re-architected for Rust, not
//! transcribed (see design decisions D1–D10).
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
pub use library::{LibraryCache, LibraryManager};
pub use model::{InstalledModel, ModelCache, ModelManager};
pub use provider::OnnxProvider;
pub use runtime::{build_session, init_runtime};
pub use tokenizer::{DEFAULT_MAX_LENGTH, Tokenized, Tokenizer};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::onnx::OnnxConfig;
use config::preset::LocalEmbedding;

/// Vector dimension used when the config declares none (oracle fallback:
/// 1024, the BGE-M3 default).
const DEFAULT_VECTOR_DIM: usize = 1024;

/// Tokenizer file name shipped alongside model files (oracle parity).
const TOKENIZER_FILE_NAME: &str = "tokenizer.json";

/// Builds the ONNX embedding provider from configuration (oracle
/// `NewONNXProvider`, re-architected — not transcribed).
///
/// Wiring order (a broken runtime fails fast, before a multi-GB model
/// download):
/// 1. [`LibraryManager::ensure_library`] — ensure the ONNX Runtime shared
///    library is installed under `data_dir` (downloaded on first use);
/// 2. [`init_runtime`] — load the library into this process;
/// 3. resolve the model (registry download on first use, or an explicit
///    `model_path` override), its `tokenizer.json`, and the vector dimension;
/// 4. [`build_session`] — create the inference session (design D7 options);
/// 5. [`Tokenizer::from_file`] + [`EmbeddingCache`] + [`OnnxProvider::new`] —
///    assemble the shareable provider.
///
/// `cfg.model_path` (when non-empty) overrides registry resolution, mirroring
/// the oracle: the file is used as-is and the tokenizer is looked for next to
/// it unless `cfg.tokenizer_path` is set. Otherwise the model is resolved from
/// the `onnx.yaml` registry by `cfg.model_name` (empty name → the registry
/// default) and downloaded through [`ModelManager::ensure_model`] when not
/// installed yet.
///
/// The provider's vector dimension comes from `cfg.vector_dim` (the config
/// validation guarantees it is positive in local mode; a non-positive value
/// falls back to the BGE-M3 default, 1024). In the registry flow a mismatch with
/// the dimension declared in `onnx.yaml` is a [`EmbeddingError::Config`] — a
/// wrong dimension would silently truncate every vector.
///
/// Synchronous: async callers dispatch this onto `spawn_blocking`.
///
/// # Errors
///
/// [`EmbeddingError::Config`] for an unsupported platform or a
/// config/registry dimension mismatch; [`EmbeddingError::Download`] /
/// [`EmbeddingError::Io`] when a library or model download fails;
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
///     vector_dim: 1024,
///     ..Default::default()
/// };
/// let onnx = OnnxConfig::default();
/// let provider: Arc<dyn EmbeddingProvider> =
///     new_onnx_provider(&cfg, "data", &onnx).expect("provider builds");
/// ```
pub fn new_onnx_provider(
    cfg: &LocalEmbedding,
    data_dir: impl AsRef<Path>,
    onnx_cfg: &OnnxConfig,
) -> Result<Arc<dyn EmbeddingProvider>, EmbeddingError> {
    let data_dir = data_dir.as_ref();
    let lib_path = LibraryManager::new(data_dir, onnx_cfg)?.ensure_library()?;
    init_runtime(&lib_path)?;
    let resolved = resolve_model(cfg, data_dir, onnx_cfg)?;
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
/// config-driven decisions are unit-testable without the ONNX Runtime.
fn resolve_model(
    cfg: &LocalEmbedding,
    data_dir: &Path,
    onnx_cfg: &OnnxConfig,
) -> Result<ResolvedModel, EmbeddingError> {
    if cfg.model_path.is_empty() {
        resolve_from_registry(cfg, data_dir, onnx_cfg)
    } else {
        resolve_explicit_path(cfg)
    }
}

/// Explicit `model_path` override (oracle `NewONNXProvider` semantics): the
/// file is used as-is — no registry lookup, no download — and the tokenizer
/// is looked for next to it unless `cfg.tokenizer_path` is set.
fn resolve_explicit_path(cfg: &LocalEmbedding) -> Result<ResolvedModel, EmbeddingError> {
    let path = PathBuf::from(&cfg.model_path);
    let name = if cfg.model_name.is_empty() {
        // The oracle's label fallback: an unnamed explicit model is just
        // "default" for the cache key.
        "default".to_string()
    } else {
        cfg.model_name.clone()
    };
    let tokenizer_path = if cfg.tokenizer_path.is_empty() {
        let dir = path.parent().ok_or_else(|| {
            EmbeddingError::Config(format!(
                "cannot derive a tokenizer directory from model path {}",
                path.display()
            ))
        })?;
        dir.join(TOKENIZER_FILE_NAME)
    } else {
        PathBuf::from(&cfg.tokenizer_path)
    };
    Ok(ResolvedModel {
        path,
        name,
        tokenizer_path,
        vector_dim: positive_dim(cfg.vector_dim),
    })
}

/// Registry flow: resolve the name (`cfg.model_name`, empty → the
/// `onnx.yaml` default), validate the dimension against the registry entry,
/// ensure the model files are installed, and locate the tokenizer.
fn resolve_from_registry(
    cfg: &LocalEmbedding,
    data_dir: &Path,
    onnx_cfg: &OnnxConfig,
) -> Result<ResolvedModel, EmbeddingError> {
    let manager = ModelManager::new(data_dir, onnx_cfg);
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
    let vector_dim = positive_dim(cfg.vector_dim);
    if info.vector_dim > 0 && info.vector_dim as usize != vector_dim {
        return Err(EmbeddingError::Config(format!(
            "vector_dim {vector_dim} in the main config does not match the {} declared by model {name:?} in onnx.yaml",
            info.vector_dim
        )));
    }
    let path = manager.ensure_model(&name)?;
    let tokenizer_path = if cfg.tokenizer_path.is_empty() {
        manager
            .path_for_file(&name, TOKENIZER_FILE_NAME)
            .ok_or_else(|| {
                EmbeddingError::Model(format!(
                    "{TOKENIZER_FILE_NAME} not found in model directory {}",
                    manager.model_dir(&name).display()
                ))
            })?
    } else {
        PathBuf::from(&cfg.tokenizer_path)
    };
    Ok(ResolvedModel {
        path,
        name,
        tokenizer_path,
        vector_dim,
    })
}

/// The provider's vector dimension from the config; a non-positive value
/// falls back to the BGE-M3 default (oracle: `if cfg.VectorDim <= 0 { 1024 }`).
fn positive_dim(cfg_dim: i32) -> usize {
    if cfg_dim > 0 {
        cfg_dim as usize
    } else {
        DEFAULT_VECTOR_DIM
    }
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
        let os = match std::env::consts::OS {
            "linux" | "windows" => std::env::consts::OS,
            "macos" => "darwin",
            _ => panic!("unsupported test platform"),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => panic!("unsupported test architecture"),
        };
        format!("{os}-{arch}")
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

    fn local_cfg(model_name: &str, vector_dim: i32) -> LocalEmbedding {
        LocalEmbedding {
            model_name: model_name.to_string(),
            vector_dim,
            ..Default::default()
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
    fn preinstall_library(data_dir: &Path) -> PathBuf {
        let cache_dir = data_dir.join("onnxruntime");
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
    fn preinstall_model(data_dir: &Path, with_tokenizer: bool) {
        let model_dir = data_dir.join("models").join("bge-m3-int8");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("model.onnx"), FAKE_MODEL).unwrap();
        if with_tokenizer {
            std::fs::write(model_dir.join(TOKENIZER_FILE_NAME), FAKE_TOKENIZER).unwrap();
        }
        ModelCache::new(data_dir.join("models"))
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
        let cfg = local_cfg("bge-m3-int8", 1024);
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
        let cfg = local_cfg("bge-m3-int8", 1024);
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
        let cfg = local_cfg("bge-m3-int8", 1024);

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
        let cfg = local_cfg("does-not-exist", 1024);
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
        let cfg = local_cfg("", 1024);
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
        let cfg = local_cfg("bge-m3-int8", 1024);
        let onnx = test_config("http://127.0.0.1:1", false);

        let err = resolve_model(&cfg, &dir, &onnx).unwrap_err();

        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
        assert!(err.to_string().contains(TOKENIZER_FILE_NAME), "got: {err}");
    }

    /// A dimension the main config declares that the registry entry
    /// contradicts is a `Config` error (a wrong dimension would silently
    /// truncate every vector).
    #[test]
    fn resolve_model_dim_mismatch_is_config_error() {
        let dir = temp_dir("dim-mismatch");
        let cfg = local_cfg("bge-m3-int8", 384); // registry says 1024
        let onnx = test_config("http://127.0.0.1:1", true);

        let err = resolve_model(&cfg, &dir, &onnx).unwrap_err();

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        assert!(err.to_string().contains("384"), "got: {err}");
    }

    /// An explicit `model_path` overrides the registry: the file is used
    /// as-is, the tokenizer is derived from its directory, and the config
    /// dimension wins.
    #[test]
    fn resolve_model_explicit_path_overrides_registry() {
        let cfg = LocalEmbedding {
            model_name: "m".to_string(),
            model_path: "/models/m/model.onnx".to_string(),
            vector_dim: 768,
            ..Default::default()
        };
        let onnx = test_config("http://127.0.0.1:1", true);

        let resolved = resolve_model(&cfg, Path::new("/unused"), &onnx).unwrap();

        assert_eq!(resolved.path, Path::new("/models/m/model.onnx"));
        assert_eq!(resolved.name, "m");
        assert_eq!(
            resolved.tokenizer_path,
            Path::new("/models/m/tokenizer.json")
        );
        assert_eq!(resolved.vector_dim, 768);
    }

    /// An explicit path with no name and no dimension falls back to the
    /// oracle defaults: label "default", BGE-M3 dimension 1024, and an
    /// explicit tokenizer path when given.
    #[test]
    fn resolve_model_explicit_path_defaults() {
        let cfg = LocalEmbedding {
            model_path: "/models/m/model.onnx".to_string(),
            tokenizer_path: "/tok/custom.json".to_string(),
            ..Default::default()
        };
        let onnx = test_config("http://127.0.0.1:1", true);

        let resolved = resolve_model(&cfg, Path::new("/unused"), &onnx).unwrap();

        assert_eq!(resolved.name, "default");
        assert_eq!(resolved.vector_dim, DEFAULT_VECTOR_DIM);
        assert_eq!(resolved.tokenizer_path, Path::new("/tok/custom.json"));
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
    /// manifests (no download), and the model is used via the explicit
    /// `model_path` override.
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

        let cfg = LocalEmbedding {
            model_name: "test".to_string(),
            model_path: model,
            tokenizer_path: tokenizer,
            vector_dim: dim as i32,
        };
        let onnx = test_config("http://127.0.0.1:1", false);

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
