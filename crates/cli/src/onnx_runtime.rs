//! `onnx-runtime` subcommand body (design D10): a thin CLI facade over
//! `embedding::LibraryManager` for the ONNX Runtime shared library.
//!
//! `install` / `status` / `uninstall`. Download, extraction, verification,
//! and the `.cache.json` manifest are NOT re-implemented here — `install`
//! goes through `LibraryManager::ensure_library` (the same path the embedding
//! factory uses), which brings the downloader's retries, SSRF protection,
//! zip/tar extraction, and cache bookkeeping for free (design D10: the CLI
//! is a facade).
//!
//! Design decisions:
//! - a config or `onnx.yaml` load failure is a hard error; continuing with a
//!   zero-value config would either dereference a nil pointer (main config)
//!   or fail later with a confusing "unsupported platform" (empty registry);
//! - the `LibraryManager` is constructed before the "Installing..." line,
//!   so an unsupported platform reports the error without a misleading
//!   progress line;
//! - `status` renders a kv block (console layer, design D5) plus the
//!   "Supported Platforms" list with tabwriter-style column geometry
//!   (longest key + 2 spaces) without a tabwriter dependency.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use config::onnx::OnnxPlatformConfig;
use config::{OnnxConfig, load, load_onnx_config};
use embedding::LibraryManager;

use crate::cli::OnnxRuntimeAction;
use crate::console::{Color, Console};
use crate::error::CliError;

/// One `onnx-runtime` invocation: the resolved config path plus the clap
/// sub-action.
pub struct OnnxRuntimeRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// The runtime sub-action.
    pub action: OnnxRuntimeAction,
}

/// The `onnx-runtime` subcommand entry point (design D10).
///
/// Loads the config (data directory + `onnx.yaml` registry) and dispatches
/// the sub-action. Human-readable output goes to stdout; on error the
/// message goes to stderr and the exit code is non-zero.
pub fn run_onnx_runtime(req: &OnnxRuntimeRequest) -> ExitCode {
    match onnx_runtime_flow(req, &mut std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            err.exit_code()
        }
    }
}

/// Config load + `onnx.yaml` load + sub-action dispatch (the production
/// path of [`run_onnx_runtime`]).
///
/// `out` receives the human-readable output (production: stdout); the tests
/// drive this seam with an in-memory buffer.
///
/// # Errors
///
/// [`CliError::Config`] for config/`onnx.yaml` failures,
/// [`CliError::Embedding`] for library-manager, download, and uninstall
/// failures, [`CliError::Io`] when `out` cannot be written.
pub fn onnx_runtime_flow(req: &OnnxRuntimeRequest, out: &mut dyn Write) -> Result<(), CliError> {
    // Load + ApplyDefaults only (the runtime commands need just the paths).
    let mut config = load(&req.cfg_path)?;
    config.apply_defaults();
    let onnx = load_onnx_config(&config.paths.onnx_config)?;
    let manager = LibraryManager::new(&config.paths.workspace_dir, &onnx)?;
    // One console per flow: TTY/NO_COLOR/width gating lives in the
    // constructor (design D2/D3), the handlers only render strings.
    let console = Console::stdout();

    match req.action {
        OnnxRuntimeAction::Install => install(&manager, out, &console),
        OnnxRuntimeAction::Status => status(&manager, &onnx, out, &console),
        OnnxRuntimeAction::Uninstall => uninstall(&manager, out, &console),
    }
}

/// `onnx-runtime install`: ensure the runtime library is installed.
fn install(
    manager: &LibraryManager,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    // This line is printed before the already-installed check, so both
    // branches keep the same prefix.
    writeln!(
        out,
        "{}",
        console.line("Installing ONNX Runtime library...")
    )?;
    if let Some(path) = manager.library_path() {
        writeln!(
            out,
            "{}",
            console.line(&format!(
                "ONNX Runtime {} is already installed at: {}",
                manager.version(),
                path.display()
            ))
        )?;
        return Ok(());
    }
    let path = manager.ensure_library()?;
    writeln!(
        out,
        "{}",
        console.success(&format!(
            "ONNX Runtime {} installed successfully",
            manager.version()
        ))
    )?;
    writeln!(
        out,
        "{}",
        console.line(&format!("  Library: {}", path.display()))
    )?;
    writeln!(
        out,
        "{}",
        console.line(&format!("  Cache: {}", manager.cache_dir().display()))
    )?;
    Ok(())
}

/// `onnx-runtime status`: a kv block (version, installation state, library
/// path, cache directory, install hint) plus the supported-platforms list.
fn status(
    manager: &LibraryManager,
    onnx: &OnnxConfig,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    writeln!(out, "{}", console.header("ONNX Runtime Status"))?;
    let mut pairs: Vec<(&str, String)> = vec![("Version", manager.version().to_string())];
    match manager.library_path() {
        Some(path) => {
            pairs.push(("Status", console.style("Installed", Color::Green)));
            pairs.push(("Library", path.display().to_string()));
            pairs.push(("Cache", manager.cache_dir().display().to_string()));
        }
        None => {
            pairs.push(("Status", console.style("Not installed", Color::Dim)));
            pairs.push(("Cache", manager.cache_dir().display().to_string()));
            pairs.push(("Run", "synopsis onnx-runtime install".to_string()));
        }
    }
    let rows: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(label, value)| (*label, value.as_str()))
        .collect();
    writeln!(out, "{}", console.kv(&rows))?;
    print_platforms(out, &onnx.runtime.platforms)
}

/// `onnx-runtime uninstall`: remove the installed library. A no-installation
/// is not an error (exit 0 with a message).
fn uninstall(
    manager: &LibraryManager,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    if manager.library_path().is_none() {
        writeln!(out, "{}", console.line("ONNX Runtime is not installed"))?;
        return Ok(());
    }
    manager.uninstall()?;
    writeln!(
        out,
        "{}",
        console.success("ONNX Runtime uninstalled successfully")
    )?;
    Ok(())
}

/// Renders the "Supported Platforms" list (tabwriter-style: 2-space
/// padding — each key is padded to the longest key, then two spaces before
/// the library name).
fn print_platforms(out: &mut dyn Write, platforms: &[OnnxPlatformConfig]) -> Result<(), CliError> {
    writeln!(out, "\nSupported Platforms:")?;
    let width = platforms
        .iter()
        .map(|platform| platform.key.len())
        .max()
        .unwrap_or(0);
    for platform in platforms {
        writeln!(out, "  {:<width$}  {}", platform.key, platform.library_name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use embedding::LibraryCache;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-onnx-{tag}-{}-{id}",
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

    const LIB_NAME: &str = "libonnxruntime.so.1.28.0";

    /// The platform key of the machine running the tests (the CI matrix
    /// covers linux-amd64 / linux-arm64 / windows-amd64 / darwin-arm64).
    fn test_platform_key() -> String {
        embedding::current_platform_key().expect("test host must be a supported platform")
    }

    /// Writes a minimal config (paths only — the runtime commands need no
    /// validation) pointing at `dir/data` and `dir/onnx.yaml`; returns the
    /// config path.
    fn write_config(dir: &TempDir) -> PathBuf {
        let yaml = format!(
            "paths:\n  workspace_dir: {data}\n  onnx_config: {onnx}\n",
            data = dir.as_ref().join("data").display(),
            onnx = dir.as_ref().join("onnx.yaml").display(),
        );
        let path = dir.as_ref().join("config.yaml");
        std::fs::write(&path, yaml).expect("write config");
        path
    }

    /// The fixture's platform rows in file order: the current platform plus
    /// one other (two table rows), as `(key, library_name)` pairs.
    fn onnx_platform_rows() -> Vec<(String, String)> {
        let key = test_platform_key();
        let second = if key == "darwin-arm64" {
            ("linux-amd64", "libonnxruntime.so.1.28.0")
        } else {
            ("darwin-arm64", "libonnxruntime.dylib")
        };
        vec![
            (key, LIB_NAME.to_string()),
            (second.0.to_string(), second.1.to_string()),
        ]
    }

    /// The `onnx.yaml` registry fixture built from [`onnx_platform_rows`];
    /// all archive URLs are unroutable — a test that reaches them has a bug.
    fn write_onnx(dir: &TempDir) {
        let mut platforms = String::new();
        for (key, library_name) in onnx_platform_rows() {
            let (os, arch) = key.split_once('-').expect("key has os-arch shape");
            platforms.push_str(&format!(
                "    - key: {key}\n      os: {os}\n      arch: {arch}\n      archive_url: http://127.0.0.1:1/onnxruntime-{key}.zip\n      archive_format: zip\n      library_name: {library_name}\n      library_path: onnxruntime-pkg/lib/{library_name}\n"
            ));
        }
        let yaml = format!(
            "runtime:\n  version: \"1.28.0\"\n  platforms:\n{platforms}models:\n  default: \"\"\n  entries: []\n"
        );
        std::fs::write(dir.as_ref().join("onnx.yaml"), yaml).expect("write onnx.yaml");
    }

    /// Pre-installs the fake runtime library and its cache manifest so
    /// `library_path` is a cache hit (no network).
    fn preinstall_library(dir: &TempDir) {
        let cache_dir = dir.as_ref().join("data").join("onnxruntime");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");
        let lib = cache_dir.join(LIB_NAME);
        std::fs::write(&lib, b"fake-onnxruntime-shared-library-bytes").expect("write library");
        std::fs::write(
            cache_dir.join(".cache.json"),
            serde_json::to_vec(&LibraryCache {
                version: "1.28.0".to_string(),
                library_path: lib,
                install_time: "2026-08-21T00:00:00Z".to_string(),
                platform: test_platform_key(),
            })
            .expect("serialize manifest"),
        )
        .expect("write manifest");
    }

    fn run_flow(dir: &TempDir, action: OnnxRuntimeAction) -> (Vec<u8>, Result<(), CliError>) {
        let cfg_path = write_config(dir);
        write_onnx(dir);
        let req = OnnxRuntimeRequest { cfg_path, action };
        let mut out: Vec<u8> = Vec::new();
        let result = onnx_runtime_flow(&req, &mut out);
        (out, result)
    }

    fn stdout_of(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    // --- status ----------------------------------------------------------------

    #[test]
    fn status_not_installed_prints_version_cache_hint_and_platforms() {
        let dir = TempDir::new("status-not-installed");

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Status);
        let stdout = stdout_of(&out);

        result.expect("status must succeed");
        assert!(stdout.starts_with("ONNX Runtime Status\n"), "{stdout:?}");
        // Non-TTY rendering: no ANSI escapes, every line within 120 columns.
        assert!(!stdout.contains('\x1b'), "{stdout:?}");
        for line in stdout.lines() {
            assert!(line.chars().count() <= 120, "line fits 120: {line:?}");
        }
        assert!(
            stdout.contains("Version:") && stdout.contains("1.28.0"),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("Status:") && stdout.contains("Not installed"),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("Cache:")
                && stdout.contains(
                    &dir.as_ref()
                        .join("data")
                        .join("onnxruntime")
                        .display()
                        .to_string()
                ),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("Run:") && stdout.contains("synopsis onnx-runtime install"),
            "{stdout:?}"
        );
        assert!(stdout.contains("\nSupported Platforms:"), "{stdout:?}");

        // kv block: the value column is aligned across rows.
        let version_line = stdout
            .lines()
            .find(|line| line.starts_with("Version:"))
            .expect("version row: {stdout:?}");
        let status_line = stdout
            .lines()
            .find(|line| line.starts_with("Status:"))
            .expect("status row: {stdout:?}");
        assert_eq!(
            version_line.find("1.28.0").expect("value present"),
            status_line.find("Not installed").expect("value present"),
            "value column aligned: {stdout:?}"
        );

        // platforms list: each key padded to the longest key + 2 spaces.
        let rows = onnx_platform_rows();
        let width = rows.iter().map(|(key, _)| key.len()).max().expect("rows");
        for (key, library_name) in &rows {
            assert!(
                stdout.contains(&format!("  {:<width$}  {library_name}", key)),
                "aligned row for {key}: {stdout:?}"
            );
        }
    }

    #[test]
    fn status_installed_prints_library_path() {
        let dir = TempDir::new("status-installed");
        preinstall_library(&dir);

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Status);
        let stdout = stdout_of(&out);

        result.expect("status must succeed");
        assert!(
            stdout.contains("Status:") && stdout.contains("Installed"),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("Library:")
                && stdout.contains(
                    &dir.as_ref()
                        .join("data")
                        .join("onnxruntime")
                        .join(LIB_NAME)
                        .display()
                        .to_string()
                ),
            "{stdout:?}"
        );
        assert!(!stdout.contains("Not installed"), "{stdout:?}");
        assert!(
            !stdout.contains("Run:"),
            "no install hint when installed: {stdout:?}"
        );
    }

    #[test]
    fn status_unsupported_platform_is_an_error() {
        let dir = TempDir::new("status-no-platform");
        let cfg_path = write_config(&dir);
        std::fs::write(
            dir.as_ref().join("onnx.yaml"),
            "runtime:\n  version: \"1.28.0\"\n  platforms:\n    - key: solaris-sparc\n",
        )
        .expect("write onnx.yaml");

        let req = OnnxRuntimeRequest {
            cfg_path,
            action: OnnxRuntimeAction::Status,
        };
        let mut out: Vec<u8> = Vec::new();
        let err = onnx_runtime_flow(&req, &mut out).expect_err("unsupported platform must fail");

        assert!(err.to_string().contains("unsupported platform"), "{err}");
        assert!(err.to_string().contains(&test_platform_key()), "{err}");
    }

    // --- install -----------------------------------------------------------------

    #[test]
    fn install_already_installed_reports_existing_path() {
        let dir = TempDir::new("install-existing");
        preinstall_library(&dir);

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Install);
        let stdout = stdout_of(&out);

        result.expect("install must succeed");
        assert!(
            stdout.contains("Installing ONNX Runtime library..."),
            "{stdout:?}"
        );
        assert!(
            stdout.contains(&format!(
                "ONNX Runtime 1.28.0 is already installed at: {}",
                dir.as_ref()
                    .join("data")
                    .join("onnxruntime")
                    .join(LIB_NAME)
                    .display()
            )),
            "{stdout:?}"
        );
        assert!(!stdout.contains("installed successfully"), "{stdout:?}");
    }

    #[test]
    fn install_missing_library_fails_offline_and_leaves_cache_unmarked() {
        let dir = TempDir::new("install-offline");
        // The registry archive URL is unroutable (127.0.0.1:1, rejected by
        // the SSRF guard before any network I/O): the install fails
        // deterministically.

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Install);
        let stdout = stdout_of(&out);

        let err = result.expect_err("unroutable archive URL must fail");
        assert!(
            stdout.contains("Installing ONNX Runtime library..."),
            "{stdout:?}"
        );
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
        assert!(
            !dir.as_ref()
                .join("data")
                .join("onnxruntime")
                .join(".cache.json")
                .exists(),
            "cache must stay unmarked"
        );
    }

    // --- uninstall ------------------------------------------------------------------

    #[test]
    fn uninstall_removes_installed_library() {
        let dir = TempDir::new("uninstall");
        preinstall_library(&dir);

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Uninstall);
        let stdout = stdout_of(&out);

        result.expect("uninstall must succeed");
        assert!(
            stdout.contains("✓ ONNX Runtime uninstalled successfully"),
            "{stdout:?}"
        );
        assert!(
            !dir.as_ref().join("data").join("onnxruntime").exists(),
            "cache directory removed"
        );
    }

    #[test]
    fn uninstall_without_installation_reports_not_installed_and_succeeds() {
        let dir = TempDir::new("uninstall-absent");

        let (out, result) = run_flow(&dir, OnnxRuntimeAction::Uninstall);
        let stdout = stdout_of(&out);

        result.expect("not installed is exit 0, not an error");
        assert!(
            stdout.contains("ONNX Runtime is not installed"),
            "{stdout:?}"
        );
        assert!(!stdout.contains("uninstalled successfully"), "{stdout:?}");
    }

    // --- exit codes -------------------------------------------------------------------

    #[test]
    fn run_onnx_runtime_maps_success_and_failure_exit_codes() {
        let dir = TempDir::new("exit-codes");
        let cfg_path = write_config(&dir);
        write_onnx(&dir);

        let ok = run_onnx_runtime(&OnnxRuntimeRequest {
            cfg_path: cfg_path.clone(),
            action: OnnxRuntimeAction::Status,
        });
        assert_eq!(ok, ExitCode::SUCCESS, "status exits 0");

        std::fs::write(
            dir.as_ref().join("onnx.yaml"),
            "runtime:\n  version: \"1.28.0\"\n  platforms:\n    - key: solaris-sparc\n",
        )
        .expect("write onnx.yaml");
        let fail = run_onnx_runtime(&OnnxRuntimeRequest {
            cfg_path,
            action: OnnxRuntimeAction::Status,
        });
        assert_eq!(fail, ExitCode::FAILURE, "unsupported platform exits 1");
    }
}
