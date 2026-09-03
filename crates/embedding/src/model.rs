//! Model manager: registry lookup, file download, and the `.cache.json`
//! installation manifest (design D5/D8, task 1.5).
//!
//! [`ModelManager`] owns the lifecycle of embedding model files under
//! `<workspace_dir>/models/<name>/`. [`ModelManager::ensure_model`] resolves the
//! name (an empty name means the config default), checks the installation
//! against the [`ModelCache`] manifest and the files on disk, and downloads
//! any missing files through [`Downloader`] (retries, SSRF protection,
//! progress, post-download size verification — design D8). The manifest is
//! written only after every file has been downloaded, so a failed download
//! never leaves the model marked as installed.
//!
//! Behavior is re-architected from `model-manager.go`, `model-cache.go` and
//! `model-registry.go` (not transcribed). Deliberate
//! deviations, all stricter than the oracle:
//! - the registry is the `models` section of `onnx.yaml` itself
//!   (`ModelForName` first-match semantics, entries with a blank name are
//!   skipped like in the oracle) instead of a separate wrapper type —
//!   ownership, not copying, is what isolates the manager from config
//!   mutation;
//! - the installation check verifies that *every* configured file exists,
//!   not only that the model directory does (the oracle could return a path
//!   whose primary file had been deleted while the directory survived);
//! - [`ModelCache`] is stateless: the manifest is re-read from disk on every
//!   call instead of mirrored in an in-memory map behind a mutex. A corrupt
//!   manifest means "nothing installed" (reinstall), consistent with
//!   [`crate::library::LibraryManager`]; the oracle failed at cache
//!   construction instead;
//! - file names from the config are validated with the shared escape guard
//!   (`safe_relative` in `crate::library`) before any download, so a hostile
//!   `onnx.yaml` cannot write outside the models directory (the oracle
//!   joined names verbatim).
//!
//! Known limitation (deviation from the oracle): `ModelFile::checksum` stays
//! in the config schema but is not verified here — the oracle verifies a
//! `sha256:` checksum when present, but no shipped `onnx.yaml` sets one, and
//! the size verification (design D8) is the active integrity check. Adding
//! checksum verification belongs to the downloader, not this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::onnx::{ModelInfo, OnnxConfig};
use serde::{Deserialize, Serialize};

use crate::downloader::Downloader;
use crate::error::EmbeddingError;
use crate::library::{installed_at_now, safe_relative};

/// Models directory name under the workspace directory (oracle parity).
const MODELS_DIR_NAME: &str = "models";
/// Installation manifest name inside the models directory (oracle parity).
const CACHE_FILE_NAME: &str = ".cache.json";

/// Installation manifest entry for one model (a `.cache.json` value).
///
/// Field names mirror the oracle's `InstalledModelInfo` JSON so manifests
/// written by either implementation remain readable by the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledModel {
    /// Registry name of the model.
    pub name: String,
    /// Model version the files were installed for.
    pub version: String,
    /// Embedding vector dimension of the model.
    pub vector_dim: i32,
    /// Installation time, RFC 3339 UTC (e.g. `"2026-08-21T10:15:30Z"`).
    pub installed_at: String,
}

/// Manifest of installed models: a JSON object mapping model name to
/// [`InstalledModel`], stored at `<models_dir>/.cache.json` (oracle
/// `ModelCache`).
///
/// The cache is stateless — every call re-reads the manifest from disk — so
/// no locking is needed and a missing or corrupt file simply reads as
/// "nothing installed", which triggers a reinstall.
#[derive(Debug)]
pub struct ModelCache {
    path: PathBuf,
}

impl ModelCache {
    /// Creates a cache backed by `<models_dir>/.cache.json`.
    #[must_use]
    pub fn new(models_dir: impl AsRef<Path>) -> Self {
        Self {
            path: models_dir.as_ref().join(CACHE_FILE_NAME),
        }
    }

    /// True when the manifest marks `name` as installed.
    #[must_use]
    pub fn is_installed(&self, name: &str) -> bool {
        self.load().contains_key(name)
    }

    /// Manifest entry for `name`, if present.
    #[must_use]
    pub fn info(&self, name: &str) -> Option<InstalledModel> {
        self.load().get(name).cloned()
    }

    /// Names of all installed models, sorted.
    #[must_use]
    pub fn list_installed(&self) -> Vec<String> {
        self.load().into_keys().collect()
    }

    /// Records a successful installation and persists the manifest.
    pub fn mark_installed(&self, info: InstalledModel) -> Result<(), EmbeddingError> {
        let mut models = self.load();
        models.insert(info.name.clone(), info);
        self.save(&models)
    }

    /// Removes the installation record and persists the manifest.
    pub fn remove(&self, name: &str) -> Result<(), EmbeddingError> {
        let mut models = self.load();
        models.remove(name);
        self.save(&models)
    }

    /// Reads the manifest; a missing or corrupt file means "nothing
    /// installed" (consistent with [`crate::library::LibraryManager`]).
    fn load(&self) -> BTreeMap<String, InstalledModel> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn save(&self, models: &BTreeMap<String, InstalledModel>) -> Result<(), EmbeddingError> {
        let data = serde_json::to_vec_pretty(models).map_err(|err| {
            EmbeddingError::Io(std::io::Error::other(format!(
                "serialize model cache: {err}"
            )))
        })?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

/// Manages download, caching, and lifecycle of embedding model files
/// (design D5).
///
/// The registry is the `models` section of `onnx.yaml` cloned at
/// construction. All methods are synchronous and may block on network or
/// disk I/O; async callers dispatch calls onto a blocking thread pool.
pub struct ModelManager {
    models_dir: PathBuf,
    entries: Vec<ModelInfo>,
    default_name: String,
    cache: ModelCache,
    downloader: Downloader,
}

impl ModelManager {
    /// Creates a manager rooted at `workspace_dir` (the GLOBAL workspace root,
    /// not per-dataset — storage-layout-restructure D3), reading the model
    /// registry and the default model name from `cfg` (oracle `NewModelManager`).
    #[must_use]
    pub fn new(workspace_dir: impl AsRef<Path>, cfg: &OnnxConfig) -> Self {
        Self::with_downloader(workspace_dir, cfg, Downloader::new())
    }

    /// Constructor with an explicit [`Downloader`].
    ///
    /// Tests use it to reach a local mock server with SSRF checking
    /// disabled; production code uses [`Self::new`].
    pub(crate) fn with_downloader(
        workspace_dir: impl AsRef<Path>,
        cfg: &OnnxConfig,
        downloader: Downloader,
    ) -> Self {
        let models_dir = workspace_dir.as_ref().join(MODELS_DIR_NAME);
        Self {
            cache: ModelCache::new(&models_dir),
            models_dir,
            entries: cfg
                .models
                .entries
                .iter()
                .filter(|model| !model.name.trim().is_empty())
                .cloned()
                .collect(),
            default_name: cfg.models.default.clone(),
            downloader,
        }
    }

    /// Ensures the named model is installed and returns the path of its
    /// primary file (the first file in the model definition, or the model
    /// directory when the model has no files) (oracle `EnsureModel`).
    ///
    /// An empty `name` uses the default model from the config. When the
    /// manifest marks the model installed and every configured file exists,
    /// the path is returned without touching the network; otherwise every
    /// missing file is downloaded (already-present files are skipped) and
    /// the manifest is written only after a complete download.
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Model`] for a name that is not in the registry
    /// (including an empty name when the config sets no default),
    /// [`EmbeddingError::Download`] / [`EmbeddingError::Io`] when a download
    /// or the cache write fails, [`EmbeddingError::Config`] for an invalid
    /// file name in the config.
    pub fn ensure_model(&self, name: &str) -> Result<PathBuf, EmbeddingError> {
        let name = if name.is_empty() {
            self.default_name.as_str()
        } else {
            name
        };
        let info = self.model(name).ok_or_else(|| {
            EmbeddingError::Model(format!("model {name:?} not found in registry"))
        })?;
        if !self.is_installed(&info.name) {
            self.download_model(info)?;
        }
        Ok(self.primary_path(info))
    }

    /// True when the manifest marks `name` installed and every configured
    /// file of the model exists on disk (oracle `IsInstalled` plus the
    /// directory check, stricter: all files, not just the directory).
    #[must_use]
    pub fn is_installed(&self, name: &str) -> bool {
        let Some(info) = self.model(name) else {
            return false;
        };
        let model_dir = self.models_dir.join(&info.name);
        self.cache.is_installed(name)
            && model_dir.is_dir()
            && info
                .files
                .iter()
                .all(|file| model_dir.join(&file.name).is_file())
    }

    /// Returns the registry entry for `name`, if any (first match,
    /// `ModelForName` semantics).
    #[must_use]
    pub fn model(&self, name: &str) -> Option<&ModelInfo> {
        self.entries.iter().find(|model| model.name == name)
    }

    /// Default model name from the config (empty when the config sets none).
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default_name
    }

    /// Directory that holds the model files (`<workspace_dir>/models/<name>`).
    #[must_use]
    pub fn model_dir(&self, name: &str) -> PathBuf {
        self.models_dir.join(name)
    }

    /// Path of a specific file of an installed model, if it exists
    /// (oracle `ModelPathForFile`).
    #[must_use]
    pub fn path_for_file(&self, name: &str, file_name: &str) -> Option<PathBuf> {
        let path = self.model_dir(name).join(file_name);
        path.is_file().then_some(path)
    }

    /// Root models directory (`<workspace_dir>/models`).
    #[must_use]
    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Removes the model files and its manifest entry (oracle
    /// `DeleteModel`).
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Model`] when the model is not marked installed.
    pub fn delete_model(&self, name: &str) -> Result<(), EmbeddingError> {
        if !self.cache.is_installed(name) {
            return Err(EmbeddingError::Model(format!(
                "model {name:?} is not installed"
            )));
        }
        let model_dir = self.model_dir(name);
        if model_dir.exists() {
            std::fs::remove_dir_all(&model_dir)?;
        }
        self.cache.remove(name)
    }

    /// Downloads every missing file of the model and marks it installed in
    /// the manifest (oracle `DownloadModel`). The manifest is the last step,
    /// so a failed download never leaves the model marked installed.
    fn download_model(&self, info: &ModelInfo) -> Result<(), EmbeddingError> {
        // Validate every file name before downloading anything, so a hostile
        // onnx.yaml cannot write outside the models directory.
        let rel_paths: Vec<PathBuf> = info
            .files
            .iter()
            .map(|file| {
                safe_relative(&file.name).map_err(|reason| {
                    EmbeddingError::Config(format!("invalid file name in onnx.yaml: {reason}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let model_dir = self.model_dir(&info.name);
        std::fs::create_dir_all(&model_dir)?;
        for (file, rel) in info.files.iter().zip(rel_paths) {
            let dest = model_dir.join(rel);
            if dest.is_file() {
                continue; // already downloaded (oracle parity)
            }
            let expected = (file.size_bytes > 0).then_some(file.size_bytes as u64);
            self.downloader.download(&file.url, &dest, expected)?;
        }
        self.cache.mark_installed(InstalledModel {
            name: info.name.clone(),
            version: info.version.clone(),
            vector_dim: info.vector_dim,
            installed_at: installed_at_now()?,
        })
    }

    /// Path of the primary model file: the first file in the definition, or
    /// the model directory when the model has no files (oracle parity).
    fn primary_path(&self, info: &ModelInfo) -> PathBuf {
        match info.files.first() {
            Some(file) => self.model_dir(&info.name).join(&file.name),
            None => self.model_dir(&info.name),
        }
    }
}

impl std::fmt::Debug for ModelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The downloader (ureq `Agent`) is not `Debug`, so it is left out.
        f.debug_struct("ModelManager")
            .field("models_dir", &self.models_dir)
            .field("entries", &self.entries)
            .field("default_name", &self.default_name)
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use config::onnx::{ModelFile, OnnxConfig, OnnxModelsConfig, OnnxRuntimeConfig};

    use super::*;

    const MODEL_ONNX: &[u8] = b"fake-model-onnx-bytes";
    const MODEL_DATA: &[u8] = b"fake-external-data-bytes";
    const TOKENIZER_JSON: &[u8] = b"fake-tokenizer-json";
    const OTHER_MODEL: &[u8] = b"fake-other-model-bytes";

    /// Registry fixture: a default model with three files plus a second
    /// single-file model; all URLs point at the test mock server.
    fn test_config(base_url: &str) -> OnnxConfig {
        OnnxConfig {
            runtime: OnnxRuntimeConfig::default(),
            models: OnnxModelsConfig {
                default: "bge-m3-int8".to_string(),
                entries: vec![
                    ModelInfo {
                        name: "bge-m3-int8".to_string(),
                        version: "1.0.0".to_string(),
                        vector_dim: 1024,
                        files: vec![
                            ModelFile {
                                name: "model.onnx".to_string(),
                                url: format!("{base_url}/model.onnx"),
                                size_bytes: MODEL_ONNX.len() as i64,
                                ..Default::default()
                            },
                            ModelFile {
                                name: "model.onnx_data".to_string(),
                                url: format!("{base_url}/model.onnx_data"),
                                size_bytes: MODEL_DATA.len() as i64,
                                ..Default::default()
                            },
                            ModelFile {
                                name: "tokenizer.json".to_string(),
                                url: format!("{base_url}/tokenizer.json"),
                                size_bytes: TOKENIZER_JSON.len() as i64,
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    },
                    ModelInfo {
                        name: "other".to_string(),
                        version: "2.0.0".to_string(),
                        vector_dim: 384,
                        files: vec![ModelFile {
                            name: "model.onnx".to_string(),
                            url: format!("{base_url}/other-model.onnx"),
                            size_bytes: OTHER_MODEL.len() as i64,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                ],
            },
        }
    }

    fn test_manager(workspace_dir: &Path, cfg: &OnnxConfig) -> ModelManager {
        ModelManager::with_downloader(
            workspace_dir,
            cfg,
            Downloader::with_params(0, Duration::ZERO, Duration::from_secs(10), false),
        )
    }

    /// Fresh per-test directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("embedding-model-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn models_dir_of(workspace_dir: &Path) -> PathBuf {
        workspace_dir.join("models")
    }

    fn model_dir_of(workspace_dir: &Path, name: &str) -> PathBuf {
        models_dir_of(workspace_dir).join(name)
    }

    fn default_files() -> HashMap<String, Vec<u8>> {
        HashMap::from([
            ("/model.onnx".to_string(), MODEL_ONNX.to_vec()),
            ("/model.onnx_data".to_string(), MODEL_DATA.to_vec()),
            ("/tokenizer.json".to_string(), TOKENIZER_JSON.to_vec()),
            ("/other-model.onnx".to_string(), OTHER_MODEL.to_vec()),
        ])
    }

    /// Minimal HTTP/1.1 server on 127.0.0.1 with an ephemeral port that
    /// serves a fixed set of files by request path (`Connection: close`, no
    /// TLS) and counts requests; keeps CI network-free.
    struct MockServer {
        url: String,
        requests: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(files: HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let shutdown = Arc::new(AtomicBool::new(false));
            let server_requests = Arc::clone(&requests);
            let server_shutdown = Arc::clone(&shutdown);
            let thread = std::thread::spawn(move || {
                loop {
                    if server_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            server_requests.fetch_add(1, Ordering::SeqCst);
                            handle(stream, &files);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                requests,
                shutdown,
                thread: Some(thread),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            // The accept loop polls the shutdown flag, so the join returns
            // promptly.
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Reads one request (headers only — all test requests are GETs) and
    /// serves the file for the request path, or 404 for an unknown path.
    fn handle(mut stream: TcpStream, files: &HashMap<String, Vec<u8>>) {
        let _ = stream.set_nonblocking(false);
        let mut buffer = [0u8; 4096];
        let mut received = Vec::new();
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    received.extend_from_slice(&buffer[..n]);
                    if received.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let path = std::str::from_utf8(&received)
            .ok()
            .and_then(|text| text.lines().next())
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        match files.get(path) {
            Some(body) => write_response(&mut stream, 200, body),
            None => write_response(&mut stream, 404, &[]),
        }
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
        // Let the client drain the response before the socket is closed.
        std::thread::sleep(Duration::from_millis(25));
    }

    /// Marks the default model installed without any network access.
    fn preinstall(workspace_dir: &Path) {
        let target = model_dir_of(workspace_dir, "bge-m3-int8");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("model.onnx"), MODEL_ONNX).unwrap();
        std::fs::write(target.join("model.onnx_data"), MODEL_DATA).unwrap();
        std::fs::write(target.join("tokenizer.json"), TOKENIZER_JSON).unwrap();
        ModelCache::new(models_dir_of(workspace_dir))
            .mark_installed(InstalledModel {
                name: "bge-m3-int8".to_string(),
                version: "1.0.0".to_string(),
                vector_dim: 1024,
                installed_at: "2026-08-21T00:00:00Z".to_string(),
            })
            .unwrap();
    }

    #[test]
    fn ensure_model_downloads_all_files_and_marks_cache() {
        let dir = temp_dir("fresh");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        let path = manager.ensure_model("bge-m3-int8").unwrap();

        let target = model_dir_of(&dir, "bge-m3-int8");
        assert_eq!(path, target.join("model.onnx"));
        assert_eq!(std::fs::read(&path).unwrap(), MODEL_ONNX);
        assert_eq!(
            std::fs::read(target.join("model.onnx_data")).unwrap(),
            MODEL_DATA
        );
        assert_eq!(
            std::fs::read(target.join("tokenizer.json")).unwrap(),
            TOKENIZER_JSON
        );
        assert_eq!(server.request_count(), 3, "one request per file");
        assert!(manager.is_installed("bge-m3-int8"));
        // Manifest written with the oracle's field names.
        let manifest: BTreeMap<String, InstalledModel> = serde_json::from_slice(
            &std::fs::read(models_dir_of(&dir).join(CACHE_FILE_NAME)).unwrap(),
        )
        .unwrap();
        let info = &manifest["bge-m3-int8"];
        assert_eq!(info.name, "bge-m3-int8");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.vector_dim, 1024);
        assert!(
            info.installed_at.ends_with('Z'),
            "got: {}",
            info.installed_at
        );
    }

    #[test]
    fn ensure_model_second_call_skips_download() {
        let dir = temp_dir("second");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        let first = manager.ensure_model("bge-m3-int8").unwrap();
        let second = manager.ensure_model("bge-m3-int8").unwrap();

        assert_eq!(first, second);
        assert_eq!(server.request_count(), 3, "second call must not download");
    }

    #[test]
    fn ensure_model_installed_returns_path_without_download() {
        // Pre-installed on disk (no server at all: any download attempt would
        // fail, so success proves the cache hit).
        let dir = temp_dir("preinstalled");
        preinstall(&dir);
        let cfg = test_config("http://127.0.0.1:1");
        let manager = test_manager(&dir, &cfg);

        let path = manager.ensure_model("bge-m3-int8").unwrap();

        assert_eq!(path, model_dir_of(&dir, "bge-m3-int8").join("model.onnx"));
    }

    #[test]
    fn unknown_model_name_is_an_error() {
        let dir = temp_dir("unknown");
        let cfg = test_config("http://127.0.0.1:1");
        let manager = test_manager(&dir, &cfg);

        let err = manager.ensure_model("does-not-exist").unwrap_err();

        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
        assert!(err.to_string().contains("does-not-exist"), "got: {err}");
        assert!(
            !models_dir_of(&dir).exists(),
            "no download for an unknown model"
        );
    }

    #[test]
    fn empty_name_without_default_is_an_error() {
        let dir = temp_dir("no-default");
        let mut cfg = test_config("http://127.0.0.1:1");
        cfg.models.default.clear();
        let manager = test_manager(&dir, &cfg);

        let err = manager.ensure_model("").unwrap_err();

        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
    }

    #[test]
    fn empty_name_uses_default_model() {
        let dir = temp_dir("default");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        let path = manager.ensure_model("").unwrap();

        assert_eq!(path, model_dir_of(&dir, "bge-m3-int8").join("model.onnx"));
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn size_mismatch_fails_and_model_stays_unmarked() {
        let dir = temp_dir("mismatch");
        // The server serves a body shorter than the configured size.
        let files = HashMap::from([("/model.onnx".to_string(), b"short".to_vec())]);
        let server = MockServer::start(files);
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        let err = manager.ensure_model("bge-m3-int8").unwrap_err();

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert!(err.to_string().contains("size mismatch"), "got: {err}");
        assert!(
            !models_dir_of(&dir).join(CACHE_FILE_NAME).exists(),
            "manifest must not be written"
        );
        assert!(!manager.is_installed("bge-m3-int8"));
        assert!(
            !model_dir_of(&dir, "bge-m3-int8")
                .join("model.onnx")
                .exists(),
            "mismatched file must be removed"
        );
    }

    #[test]
    fn existing_files_are_skipped_on_redownload() {
        let dir = temp_dir("partial");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        manager.ensure_model("bge-m3-int8").unwrap();
        let target = model_dir_of(&dir, "bge-m3-int8");
        std::fs::remove_file(target.join("model.onnx_data")).unwrap();
        std::fs::remove_file(models_dir_of(&dir).join(CACHE_FILE_NAME)).unwrap();

        let path = manager.ensure_model("bge-m3-int8").unwrap();

        assert_eq!(path, target.join("model.onnx"));
        assert_eq!(
            std::fs::read(target.join("model.onnx_data")).unwrap(),
            MODEL_DATA
        );
        assert_eq!(
            server.request_count(),
            4,
            "only the missing file is re-downloaded"
        );
    }

    #[test]
    fn delete_model_removes_files_and_cache_entry() {
        let dir = temp_dir("delete");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);

        manager.ensure_model("bge-m3-int8").unwrap();
        manager.delete_model("bge-m3-int8").unwrap();

        assert!(!model_dir_of(&dir, "bge-m3-int8").exists());
        assert!(!manager.is_installed("bge-m3-int8"));

        let err = manager.delete_model("bge-m3-int8").unwrap_err();
        assert!(matches!(err, EmbeddingError::Model(_)), "got: {err}");
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }

    #[test]
    fn path_for_file_resolves_existing_files_only() {
        let dir = temp_dir("path-for-file");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);
        manager.ensure_model("bge-m3-int8").unwrap();

        let target = model_dir_of(&dir, "bge-m3-int8");
        assert_eq!(
            manager.path_for_file("bge-m3-int8", "tokenizer.json"),
            Some(target.join("tokenizer.json"))
        );
        assert!(
            manager
                .path_for_file("bge-m3-int8", "missing.json")
                .is_none()
        );
        assert!(manager.path_for_file("other", "model.onnx").is_none());
    }

    #[test]
    fn corrupt_manifest_is_treated_as_not_installed() {
        let dir = temp_dir("corrupt");
        let server = MockServer::start(default_files());
        let cfg = test_config(&server.url);
        let manager = test_manager(&dir, &cfg);
        manager.ensure_model("bge-m3-int8").unwrap();

        std::fs::write(models_dir_of(&dir).join(CACHE_FILE_NAME), b"not json").unwrap();

        assert!(!manager.is_installed("bge-m3-int8"));
        let path = manager.ensure_model("bge-m3-int8").unwrap();
        assert_eq!(path, model_dir_of(&dir, "bge-m3-int8").join("model.onnx"));
        assert!(manager.is_installed("bge-m3-int8"));
        assert_eq!(
            server.request_count(),
            3,
            "all files exist: no re-download, only a re-mark"
        );
    }

    #[test]
    fn invalid_file_name_is_rejected_before_download() {
        let dir = temp_dir("bad-name");
        let server = MockServer::start(default_files());
        let mut cfg = test_config(&server.url);
        cfg.models.entries[0].files = vec![ModelFile {
            name: "../evil.onnx".to_string(),
            url: format!("{}/evil.onnx", server.url),
            size_bytes: 1,
            ..Default::default()
        }];
        let manager = test_manager(&dir, &cfg);

        let err = manager.ensure_model("bge-m3-int8").unwrap_err();

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        assert_eq!(
            server.request_count(),
            0,
            "no download before name validation"
        );
        assert!(
            !models_dir_of(&dir).join("evil.onnx").exists(),
            "entry must not escape the models directory"
        );
        assert!(
            !models_dir_of(&dir).join(CACHE_FILE_NAME).exists(),
            "manifest must not be written"
        );
    }

    #[test]
    fn blank_names_are_filtered_from_the_registry() {
        let dir = temp_dir("blank-names");
        let mut cfg = test_config("http://127.0.0.1:1");
        cfg.models.entries.push(ModelInfo {
            name: "   ".to_string(),
            ..Default::default()
        });
        let manager = test_manager(&dir, &cfg);

        assert!(manager.model("   ").is_none());
        assert_eq!(manager.model("bge-m3-int8").unwrap().vector_dim, 1024);
        assert!(manager.model("nope").is_none());
        assert_eq!(manager.default_model(), "bge-m3-int8");
        assert_eq!(manager.models_dir(), models_dir_of(&dir).as_path());
        assert_eq!(manager.model_dir("other"), model_dir_of(&dir, "other"));
    }

    #[test]
    fn cache_mark_info_list_remove_round_trip() {
        let dir = temp_dir("cache-rt");
        let cache = ModelCache::new(&dir);

        assert!(!cache.is_installed("m"));
        assert!(cache.list_installed().is_empty());
        cache
            .mark_installed(InstalledModel {
                name: "m".to_string(),
                version: "1".to_string(),
                vector_dim: 4,
                installed_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .unwrap();
        assert!(cache.is_installed("m"));
        assert_eq!(cache.info("m").unwrap().vector_dim, 4);
        assert_eq!(cache.list_installed(), vec!["m".to_string()]);

        cache.remove("m").unwrap();
        assert!(!cache.is_installed("m"));
        assert!(cache.info("m").is_none());
    }

    #[test]
    fn cache_manifest_uses_oracle_field_names() {
        let dir = temp_dir("cache-shape");
        let cache = ModelCache::new(&dir);
        cache
            .mark_installed(InstalledModel {
                name: "m".to_string(),
                version: "1".to_string(),
                vector_dim: 4,
                installed_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .unwrap();

        let json = std::fs::read_to_string(dir.join(CACHE_FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["m"]["name"], "m");
        assert_eq!(value["m"]["version"], "1");
        assert_eq!(value["m"]["vector_dim"], 4);
        assert_eq!(value["m"]["installed_at"], "2026-01-01T00:00:00Z");
    }
}
