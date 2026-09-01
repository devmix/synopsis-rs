//! ONNX Runtime shared-library manager (design D1/D5, task 1.4).
//!
//! [`LibraryManager`] owns the lifecycle of the external ONNX Runtime shared
//! library: it resolves the platform entry for the current OS/architecture
//! from `onnx.yaml` ([`OnnxConfig::platform_for_key`]), downloads the release
//! archive through [`Downloader`], extracts it into `<workspace_dir>/onnxruntime/`,
//! and records the installation in a `.cache.json` manifest. A repeat call
//! with the same version returns the cached path without touching the network.
//!
//! Behavior is re-architected from `../synopsis/internal/onnx/library.go`,
//! `library_cache.go` and `library_registry.go` (not transcribed).
//! Deliberate deviations, all stricter than the oracle:
//! - downloads go through [`Downloader`] (retries, SSRF protection, progress,
//!   partial-file cleanup) instead of a bare `http.Client`;
//! - archive entries that would escape the cache directory (zip-slip /
//!   tar-slip) are rejected — the oracle joined entry names verbatim;
//! - no unversioned symlink (`libonnxruntime.so`) is created (design D10):
//!   `ort::init_from` loads the library by its explicit cached path.
//!
//! The cache manifest keeps the oracle's JSON field names
//! (`version`, `library_path`, `install_time`, `platform`) so a manifest
//! written by the Go binary remains readable.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use config::onnx::{ArchiveFormat, OnnxConfig, OnnxPlatformConfig};
use serde::{Deserialize, Serialize};

use crate::downloader::Downloader;
use crate::error::EmbeddingError;

/// Cache directory name under the workspace directory (oracle parity).
const CACHE_DIR_NAME: &str = "onnxruntime";
/// Installation manifest name inside the cache directory (oracle parity).
const CACHE_FILE_NAME: &str = ".cache.json";
/// Downloaded-archive name prefix inside the cache directory (oracle parity).
const ARCHIVE_NAME_PREFIX: &str = "onnxruntime-archive.";

/// Installation manifest for the ONNX Runtime shared library (`.cache.json`).
///
/// Field names mirror the oracle's `LibraryCache` JSON so manifests written
/// by either implementation remain readable by the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCache {
    /// ONNX Runtime version the library was installed for.
    pub version: String,
    /// Path of the installed library file.
    pub library_path: PathBuf,
    /// Installation time, RFC 3339 UTC (e.g. `"2026-08-21T10:15:30Z"`).
    pub install_time: String,
    /// Platform key the library was installed for (e.g. `"linux-amd64"`).
    pub platform: String,
}

/// Manages download, extraction and caching of the ONNX Runtime shared
/// library (design D1/D5).
///
/// All methods are synchronous and may block on network or disk I/O; async
/// callers dispatch calls onto a blocking thread pool.
pub struct LibraryManager {
    cache_dir: PathBuf,
    platform: OnnxPlatformConfig,
    version: String,
    downloader: Downloader,
}

impl LibraryManager {
    /// Creates a manager rooted at `workspace_dir` (the GLOBAL workspace
    /// root, not per-dataset — storage-layout-restructure D3), resolving the
    /// platform entry for the current OS/architecture from `cfg`
    /// (oracle `NewLibraryManager`).
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Config`] when the current platform is not supported
    /// or `cfg` has no matching `runtime.platforms` entry.
    pub fn new(workspace_dir: impl AsRef<Path>, cfg: &OnnxConfig) -> Result<Self, EmbeddingError> {
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
    ) -> Result<Self, EmbeddingError> {
        let key = current_platform_key().ok_or_else(|| {
            EmbeddingError::Config(format!(
                "unsupported platform: {}/{} (expected linux, darwin or windows with amd64 or arm64)",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
        let platform = cfg.platform_for_key(&key).cloned().ok_or_else(|| {
            EmbeddingError::Config(format!(
                "unsupported platform: {key} (no matching entry in onnx.yaml runtime.platforms)"
            ))
        })?;
        Ok(Self {
            cache_dir: workspace_dir.as_ref().join(CACHE_DIR_NAME),
            platform,
            version: cfg.runtime.version.clone(),
            downloader,
        })
    }

    /// Ensures the library is installed and returns its path
    /// (oracle `EnsureLibrary`).
    ///
    /// When the cache manifest names the configured version and the file
    /// still exists, the cached path is returned without any download.
    /// Otherwise the archive is downloaded and extracted; the manifest is
    /// written only after a successful install, so a failed install never
    /// leaves the cache marked.
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Download`] / [`EmbeddingError::Io`] when the
    /// download or extraction fails, [`EmbeddingError::Config`] for an
    /// unsupported archive format or `library_path`, and
    /// [`EmbeddingError::Model`] when the install cannot be verified.
    pub fn ensure_library(&self) -> Result<PathBuf, EmbeddingError> {
        if let Some(path) = self.library_path() {
            return Ok(path);
        }
        self.install()?;
        self.library_path().ok_or_else(|| {
            EmbeddingError::Model("library installation verification failed".to_string())
        })
    }

    /// Returns the path of the installed library, if the cache manifest
    /// matches the configured version and the file still exists
    /// (oracle `GetLibraryPath` / `IsInstalled`).
    #[must_use]
    pub fn library_path(&self) -> Option<PathBuf> {
        let cache = self.load_cache()?;
        if cache.version != self.version || !cache.library_path.is_file() {
            return None;
        }
        Some(cache.library_path)
    }

    /// Path of the cache directory (`<workspace_dir>/onnxruntime`).
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// ONNX Runtime version from the config.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Removes the installed library and its metadata (oracle
    /// `UninstallLibrary`). A no-op when nothing is installed.
    pub fn uninstall(&self) -> Result<(), EmbeddingError> {
        if self.library_path().is_none() {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.cache_dir)?;
        Ok(())
    }

    /// Downloads the release archive, extracts it, and writes the cache
    /// manifest (oracle `DownloadLibrary`).
    fn install(&self) -> Result<(), EmbeddingError> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let archive_path = self
            .cache_dir
            .join(format!("{ARCHIVE_NAME_PREFIX}{}", self.archive_ext()?));
        self.downloader
            .download(&self.platform.archive_url, &archive_path, None)?;
        // The archive is never needed once extracted, so it is removed on
        // every exit, success or failure (oracle parity).
        let extracted = self
            .extract_archive(&archive_path)
            .and_then(|()| self.place_library());
        let _ = std::fs::remove_file(&archive_path);
        extracted?;
        self.save_cache(&LibraryCache {
            version: self.version.clone(),
            library_path: self.installed_path(),
            install_time: installed_at_now()?,
            platform: format!("{}-{}", self.platform.os, self.platform.arch),
        })?;
        Ok(())
    }

    /// Final, versioned library path inside the cache directory.
    fn installed_path(&self) -> PathBuf {
        self.cache_dir.join(&self.platform.library_name)
    }

    /// Archive file extension from the config (`zip`/`tgz`); anything else
    /// is a config error (oracle "unsupported archive format").
    fn archive_ext(&self) -> Result<&'static str, EmbeddingError> {
        match &self.platform.archive_format {
            ArchiveFormat::Zip => Ok("zip"),
            ArchiveFormat::Tgz => Ok("tgz"),
            ArchiveFormat::Unknown(format) => Err(unsupported_format(format.clone())),
        }
    }

    fn extract_archive(&self, archive_path: &Path) -> Result<(), EmbeddingError> {
        match &self.platform.archive_format {
            ArchiveFormat::Zip => extract_zip(archive_path, &self.cache_dir),
            ArchiveFormat::Tgz => extract_tgz(archive_path, &self.cache_dir),
            ArchiveFormat::Unknown(format) => Err(unsupported_format(format.clone())),
        }
    }

    /// Copies the extracted library to its final versioned name
    /// (oracle copyFile step). No unversioned symlink is created
    /// (design D10).
    fn place_library(&self) -> Result<(), EmbeddingError> {
        let rel = safe_relative(&self.platform.library_path).map_err(|reason| {
            EmbeddingError::Config(format!("invalid library_path in onnx.yaml: {reason}"))
        })?;
        let src = self.cache_dir.join(rel);
        let dst = self.installed_path();
        std::fs::copy(&src, &dst)?;
        Ok(())
    }

    /// Reads the cache manifest; a missing or unreadable/corrupt manifest
    /// means "not installed" (oracle `IsInstalled` swallows load errors, so
    /// a broken manifest simply triggers a reinstall).
    fn load_cache(&self) -> Option<LibraryCache> {
        let data = std::fs::read(self.cache_dir.join(CACHE_FILE_NAME)).ok()?;
        serde_json::from_slice(&data).ok()
    }

    fn save_cache(&self, cache: &LibraryCache) -> Result<(), EmbeddingError> {
        let data = serde_json::to_vec_pretty(cache).map_err(|err| {
            EmbeddingError::Io(std::io::Error::other(format!(
                "serialize library cache: {err}"
            )))
        })?;
        std::fs::write(self.cache_dir.join(CACHE_FILE_NAME), data)?;
        Ok(())
    }
}

impl std::fmt::Debug for LibraryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The downloader (ureq `Agent`) is not `Debug`, so it is left out.
        f.debug_struct("LibraryManager")
            .field("cache_dir", &self.cache_dir)
            .field("platform", &self.platform)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Error for an archive format the schema does not recognize.
fn unsupported_format(format: String) -> EmbeddingError {
    EmbeddingError::Config(format!("unsupported archive format: {format}"))
}

/// Maps the Rust compile-time OS/arch to the oracle-style platform key
/// (e.g. `"linux-amd64"`), the `key` values used in `onnx.yaml` (computed
/// from Go's `GOOS`/`GOARCH` in the oracle).
///
/// Public so other crates' tests (e.g. `cli`) can delegate their
/// `test_platform_key` fixture helper to the production code path instead
/// of re-implementing the OS/arch mapping (test-hygiene-phase-1 D8).
pub fn current_platform_key() -> Option<String> {
    let os = match std::env::consts::OS {
        "linux" | "windows" => std::env::consts::OS,
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// Normalizes an archive entry name to a relative path, rejecting names that
/// are absolute or contain `..` components (zip-slip / tar-slip protection;
/// the oracle joined entry names verbatim).
///
/// Crate-internal: also used by [`crate::model`] to validate model file names
/// from `onnx.yaml` before joining them to the models directory.
pub(crate) fn safe_relative(name: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            other => {
                return Err(format!(
                    "entry {name:?} escapes the extraction directory ({other:?} component)"
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("entry {name:?} is empty"));
    }
    Ok(out)
}

/// Extracts a ZIP archive into `dest_dir` (oracle `extractZip`).
fn extract_zip(archive_path: &Path, dest_dir: &Path) -> Result<(), EmbeddingError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| {
        EmbeddingError::Io(std::io::Error::other(format!(
            "open zip {archive_path:?}: {err}"
        )))
    })?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|err| {
            EmbeddingError::Io(std::io::Error::other(format!(
                "read zip entry {index}: {err}"
            )))
        })?;
        let rel = safe_relative(entry.name()).map_err(EmbeddingError::Download)?;
        write_entry(&dest_dir.join(rel), entry.is_dir(), &mut entry)?;
    }
    Ok(())
}

/// Extracts a TAR.GZ archive into `dest_dir` (oracle `extractTgz`).
fn extract_tgz(archive_path: &Path, dest_dir: &Path) -> Result<(), EmbeddingError> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|err| {
        EmbeddingError::Io(std::io::Error::other(format!(
            "read tar {archive_path:?}: {err}"
        )))
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|err| {
            EmbeddingError::Io(std::io::Error::other(format!("read tar entry: {err}")))
        })?;
        let name = entry
            .path()
            .map_err(|err| {
                EmbeddingError::Io(std::io::Error::other(format!("read tar entry name: {err}")))
            })?
            .to_string_lossy()
            .into_owned();
        let rel = safe_relative(&name).map_err(EmbeddingError::Download)?;
        write_entry(
            &dest_dir.join(rel),
            entry.header().entry_type().is_dir(),
            &mut entry,
        )?;
    }
    Ok(())
}

/// Creates `dest_path` (and missing parents) and copies `read` into it, or
/// creates it as a directory when `is_dir` is set.
fn write_entry<R: Read>(
    dest_path: &Path,
    is_dir: bool,
    read: &mut R,
) -> Result<(), EmbeddingError> {
    if is_dir {
        std::fs::create_dir_all(dest_path)?;
        return Ok(());
    }
    if let Some(parent) = dest_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(dest_path)?;
    std::io::copy(read, &mut out)?;
    Ok(())
}

/// The current time as an RFC 3339 UTC manifest timestamp
/// (`utils::temporal::now_rfc3339`, the workspace date/time seam).
///
/// Crate-internal: used by the library cache manifest (`install_time`) and
/// the model cache manifest ([`crate::model`] `installed_at`). Fails when the
/// system clock precedes the Unix epoch — the manifests must never carry a
/// silent epoch fallback.
pub(crate) fn installed_at_now() -> Result<String, EmbeddingError> {
    utils::temporal::now_rfc3339()
        .ok_or_else(|| EmbeddingError::Model("system clock precedes the Unix epoch".to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use config::onnx::{OnnxConfig, OnnxPlatformConfig, OnnxRuntimeConfig};

    use super::*;

    const FAKE_LIBRARY: &[u8] = b"fake-onnxruntime-shared-library-bytes";
    const LIB_ENTRY: &str = "onnxruntime-pkg/lib/libonnxruntime.so.1.28.0";
    const LIB_NAME: &str = "libonnxruntime.so.1.28.0";

    /// The platform key of the machine running the tests (the CI matrix covers
    /// linux-amd64 / linux-arm64 / windows-amd64 / darwin-arm64).
    fn test_platform_key() -> String {
        current_platform_key().expect("test host must be a supported platform")
    }

    fn test_config(version: &str, format: ArchiveFormat, archive_url: &str) -> OnnxConfig {
        let key = test_platform_key();
        let (os, arch) = key.split_once('-').expect("key has os-arch shape");
        OnnxConfig {
            runtime: OnnxRuntimeConfig {
                version: version.to_string(),
                platforms: vec![OnnxPlatformConfig {
                    os: os.to_string(),
                    arch: arch.to_string(),
                    archive_url: archive_url.to_string(),
                    archive_format: format,
                    library_name: LIB_NAME.to_string(),
                    library_path: LIB_ENTRY.to_string(),
                    key,
                }],
            },
            models: Default::default(),
        }
    }

    fn test_manager(workspace_dir: &Path, cfg: &OnnxConfig) -> LibraryManager {
        let downloader = Downloader::with_params(0, Duration::ZERO, Duration::from_secs(10), false);
        LibraryManager::with_downloader(workspace_dir, cfg, downloader).unwrap()
    }

    /// Fresh per-test directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embedding-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Builds the release-archive fixture (one directory, one library entry)
    /// in the requested container format.
    fn build_archive(path: &Path, format: &ArchiveFormat) {
        match format {
            ArchiveFormat::Zip => {
                let file = std::fs::File::create(path).unwrap();
                let options = zip::write::SimpleFileOptions::default();
                let mut zip = zip::ZipWriter::new(file);
                zip.add_directory("onnxruntime-pkg/", options).unwrap();
                zip.start_file(LIB_ENTRY, options).unwrap();
                zip.write_all(FAKE_LIBRARY).unwrap();
                zip.finish().unwrap();
            }
            ArchiveFormat::Tgz => {
                let file = std::fs::File::create(path).unwrap();
                let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
                let mut tar = tar::Builder::new(encoder);
                let mut header = tar::Header::new_gnu();
                header.set_size(FAKE_LIBRARY.len() as u64);
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                tar.append_data(&mut header, LIB_ENTRY, FAKE_LIBRARY)
                    .unwrap();
                tar.finish().unwrap();
            }
            ArchiveFormat::Unknown(_) => panic!("fixture requires zip or tgz"),
        }
    }

    /// Minimal single-request-per-connection HTTP/1.1 server on 127.0.0.1
    /// with an ephemeral port — plain TCP, no TLS, `Connection: close`.
    /// Serves the fixed `(status, body)` pair for every request and counts
    /// them; keeps CI network-free.
    struct MockServer {
        url: String,
        requests: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(status: u16, body: Vec<u8>) -> Self {
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
                            serve(stream, status, &body);
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

    fn serve(mut stream: TcpStream, status: u16, body: &[u8]) {
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

    /// Fresh install through the mock server; asserts library bytes, request
    /// count, archive cleanup, and the manifest contents.
    fn assert_fresh_install(format: ArchiveFormat, ext: &str) {
        let workspace_dir = temp_dir(&format!("fresh-{ext}"));
        let fixture = workspace_dir.join(format!("fixture.{ext}"));
        build_archive(&fixture, &format);
        let server = MockServer::start(200, std::fs::read(&fixture).unwrap());
        let cfg = test_config("1.28.0", format, &server.url);
        let manager = test_manager(&workspace_dir, &cfg);

        let lib = manager.ensure_library().unwrap();

        let cache_dir = workspace_dir.join(CACHE_DIR_NAME);
        assert_eq!(lib, cache_dir.join(LIB_NAME));
        assert_eq!(std::fs::read(&lib).unwrap(), FAKE_LIBRARY);
        assert_eq!(server.request_count(), 1);
        // The archive is removed after extraction (oracle parity).
        assert!(
            !cache_dir
                .join(format!("{ARCHIVE_NAME_PREFIX}{ext}"))
                .exists()
        );
        // Manifest written with the oracle's field names.
        let manifest: LibraryCache =
            serde_json::from_slice(&std::fs::read(cache_dir.join(CACHE_FILE_NAME)).unwrap())
                .unwrap();
        let key = test_platform_key();
        let (os, arch) = key.split_once('-').unwrap();
        assert_eq!(manifest.version, "1.28.0");
        assert_eq!(manifest.library_path, lib);
        assert_eq!(manifest.platform, format!("{os}-{arch}"));
        assert!(
            manifest.install_time.ends_with('Z'),
            "got: {}",
            manifest.install_time
        );
    }

    #[test]
    fn ensure_library_installs_zip_from_scratch() {
        assert_fresh_install(ArchiveFormat::Zip, "zip");
    }

    #[test]
    fn ensure_library_installs_tgz_from_scratch() {
        assert_fresh_install(ArchiveFormat::Tgz, "tgz");
    }

    #[test]
    fn ensure_library_second_call_skips_download() {
        let workspace_dir = temp_dir("second-call");
        let fixture = workspace_dir.join("fixture.zip");
        build_archive(&fixture, &ArchiveFormat::Zip);
        let server = MockServer::start(200, std::fs::read(&fixture).unwrap());
        let cfg = test_config("1.28.0", ArchiveFormat::Zip, &server.url);
        let manager = test_manager(&workspace_dir, &cfg);

        let first = manager.ensure_library().unwrap();
        let second = manager.ensure_library().unwrap();

        assert_eq!(first, second);
        assert_eq!(server.request_count(), 1, "second call must not download");
    }

    #[test]
    fn version_mismatch_triggers_reinstall() {
        let workspace_dir = temp_dir("version-bump");
        let fixture = workspace_dir.join("fixture.zip");
        build_archive(&fixture, &ArchiveFormat::Zip);
        let server = MockServer::start(200, std::fs::read(&fixture).unwrap());

        let cfg_v1 = test_config("1.28.0", ArchiveFormat::Zip, &server.url);
        test_manager(&workspace_dir, &cfg_v1)
            .ensure_library()
            .unwrap();
        assert_eq!(server.request_count(), 1);

        let cfg_v2 = test_config("1.29.0", ArchiveFormat::Zip, &server.url);
        test_manager(&workspace_dir, &cfg_v2)
            .ensure_library()
            .unwrap();
        assert_eq!(server.request_count(), 2, "version change must re-download");

        let manifest: LibraryCache = serde_json::from_slice(
            &std::fs::read(workspace_dir.join(CACHE_DIR_NAME).join(CACHE_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.version, "1.29.0");
    }

    #[test]
    fn download_failure_leaves_cache_unmarked() {
        let workspace_dir = temp_dir("dl-fail");
        let server = MockServer::start(404, Vec::new());
        let cfg = test_config("1.28.0", ArchiveFormat::Zip, &server.url);
        let manager = test_manager(&workspace_dir, &cfg);

        let err = manager.ensure_library().unwrap_err();

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert_eq!(server.request_count(), 1, "404 is permanent: no retries");
        let cache_dir = manager.cache_dir();
        assert!(
            !cache_dir.join(CACHE_FILE_NAME).exists(),
            "cache must stay unmarked"
        );
        assert!(
            !cache_dir.join(format!("{ARCHIVE_NAME_PREFIX}zip")).exists(),
            "archive must be cleaned up"
        );
        assert!(manager.library_path().is_none());
    }

    #[test]
    fn missing_platform_entry_is_config_error() {
        let cfg = OnnxConfig {
            runtime: OnnxRuntimeConfig {
                version: "1.28.0".to_string(),
                platforms: vec![OnnxPlatformConfig {
                    key: "solaris-sparc".to_string(),
                    ..Default::default()
                }],
            },
            models: Default::default(),
        };

        let err = LibraryManager::new(temp_dir("no-platform"), &cfg).unwrap_err();

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        assert!(err.to_string().contains(&test_platform_key()), "got: {err}");
    }

    #[test]
    fn current_platform_key_is_an_oracle_key() {
        let key = test_platform_key();
        assert!(
            [
                "linux-amd64",
                "linux-arm64",
                "windows-amd64",
                "darwin-amd64",
                "darwin-arm64"
            ]
            .contains(&key.as_str()),
            "unexpected platform key: {key}"
        );
    }

    #[test]
    fn unknown_archive_format_is_config_error() {
        let workspace_dir = temp_dir("bad-format");
        let server = MockServer::start(200, b"whatever".to_vec());
        let cfg = test_config(
            "1.28.0",
            ArchiveFormat::Unknown("7z".to_string()),
            &server.url,
        );
        let manager = test_manager(&workspace_dir, &cfg);

        let err = manager.ensure_library().unwrap_err();

        assert!(matches!(err, EmbeddingError::Config(_)), "got: {err}");
        assert!(err.to_string().contains("7z"), "got: {err}");
        assert_eq!(
            server.request_count(),
            0,
            "no download before the format is validated"
        );
    }

    #[test]
    fn uninstall_removes_cache_dir_and_is_idempotent() {
        let workspace_dir = temp_dir("uninstall");
        let fixture = workspace_dir.join("fixture.zip");
        build_archive(&fixture, &ArchiveFormat::Zip);
        let server = MockServer::start(200, std::fs::read(&fixture).unwrap());
        let cfg = test_config("1.28.0", ArchiveFormat::Zip, &server.url);
        let manager = test_manager(&workspace_dir, &cfg);

        manager.ensure_library().unwrap();
        assert!(manager.cache_dir().exists());

        manager.uninstall().unwrap();
        assert!(!manager.cache_dir().exists());
        assert!(manager.library_path().is_none());
        manager.uninstall().unwrap(); // no-op when nothing is installed
    }

    #[test]
    fn zip_entry_escaping_cache_dir_is_rejected() {
        let workspace_dir = temp_dir("zip-slip");
        let archive = workspace_dir.join("evil.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("../evil.txt", options).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }
        let dest = workspace_dir.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&dest).unwrap();

        let err = extract_zip(&archive, &dest).unwrap_err();

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert!(
            !workspace_dir.join("evil.txt").exists(),
            "entry must not escape"
        );
    }

    #[test]
    fn safe_relative_rejects_escaping_and_absolute_entries() {
        // The tar crate's writer itself refuses to create `..` entries, so the
        // shared escape guard is tested directly here; the zip-slip test above
        // covers the same guard end-to-end through `extract_zip`, and the tgz
        // install test covers the rest of the tar flow.
        assert!(safe_relative("../evil.txt").is_err());
        assert!(safe_relative("a/../../evil.txt").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("").is_err());
        assert_eq!(
            safe_relative("pkg/lib/lib.so").unwrap(),
            Path::new("pkg/lib/lib.so")
        );
        assert_eq!(
            safe_relative("./pkg/lib.so").unwrap(),
            Path::new("pkg/lib.so")
        );
    }
}
