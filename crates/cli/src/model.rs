//! `model` subcommand body (design D9): a thin CLI facade over
//! `embedding::ModelManager` for the embedding model registry in `onnx.yaml`.
//!
//! `list` / `download` / `delete` / `info` / `benchmark`.
//! Download/verify is NOT re-implemented here — `download` goes through
//! `ModelManager::ensure_model` (the same path the bootstrap uses), which
//! brings the downloader's retries, SSRF protection, progress bar, and
//! size verification for free (design D9: the CLI is a facade).
//!
//! Design decisions:
//! - the registry is the `models` section of `onnx.yaml` itself, so the
//!   "all models" listing reads `OnnxConfig::models.entries` with the same
//!   blank-name filter `ModelManager` applies internally;
//! - installation status uses `ModelManager::is_installed` (manifest +
//!   directory + every configured file), stricter than a manifest+directory
//!   check;
//! - the ONNX Runtime version in the benchmark header comes from the library
//!   cache manifest (`LibraryManager`) instead of a directory-name glob;
//! - `benchmark` measures the production path through the real
//!   `new_onnx_provider` (the exact code used by sync and search; batch-max
//!   padding). A padded (seq=512) measurement and a natural-length section
//!   are intentionally omitted: they require raw ONNX session access, which
//!   this codebase isolates inside the `embedding` crate (`runtime.rs`), and
//!   the `embedding::benchmark_model` the task body references does not exist
//!   yet.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use config::onnx::ModelInfo;
use config::preset::LocalEmbedding;
use config::{OnnxConfig, load, load_onnx_config};
use embedding::{LibraryManager, ModelCache, ModelManager, new_onnx_provider};

use crate::cli::ModelAction;
use crate::console::{Color, Console};
use crate::error::CliError;

/// Warmup iterations before measuring (`BenchOptions` default).
const BENCH_WARMUP: usize = 3;
/// Measured iterations per benchmark section (`BenchOptions` default).
const BENCH_RUNS: usize = 9;
/// Deterministic benchmark text (~90 content tokens);
/// a unique suffix per run keeps results out of the embedding cache.
const BENCH_SAMPLE_TEXT: &str = "The company operates across three regions and provides \
enterprise analytics tooling to mid-market customers worldwide. The organization was \
founded in 2014 and has grown steadily through a series of acquisitions, expanding its \
engineering team and data infrastructure year after year.";

/// One `model` invocation: the resolved config path plus the clap `model`
/// sub-action and its optional model name.
pub struct ModelRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// The model sub-action.
    pub action: ModelAction,
    /// Model name (optional positional; `list` ignores it, `delete` requires it).
    pub name: Option<String>,
}

/// The `model` subcommand entry point (design D9).
///
/// Loads the config (data directory + `onnx.yaml` registry) and dispatches
/// the sub-action. Human-readable output goes to stdout; on error the
/// message goes to stderr and the exit code is non-zero.
pub fn run_model(req: &ModelRequest) -> ExitCode {
    match model_flow(req, &mut std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            err.exit_code()
        }
    }
}

/// Config load + registry load + sub-action dispatch (the production path of
/// [`run_model`]).
///
/// `out` receives the human-readable table output (production: stdout); the
/// tests drive this seam with an in-memory buffer.
///
/// # Errors
///
/// [`CliError::Config`] for config/`onnx.yaml` failures,
/// [`CliError::Embedding`] for download / delete / provider failures,
/// [`CliError::Unsupported`] for unknown model names and missing arguments,
/// [`CliError::Io`] when `out` cannot be written.
pub fn model_flow(req: &ModelRequest, out: &mut dyn Write) -> Result<(), CliError> {
    // Load + ApplyDefaults only (no validate — the model commands need just
    // the paths).
    let mut config = load(&req.cfg_path)?;
    config.apply_defaults();
    let onnx = load_onnx_config(&config.paths.onnx_config)?;
    let manager = ModelManager::new(&config.paths.workspace_dir, &onnx);
    // One console per flow: TTY/NO_COLOR/width gating lives in the
    // constructor (design D2/D3), the handlers only render strings.
    let console = Console::stdout();

    match req.action {
        ModelAction::List => list_models(&manager, &onnx, out, &console),
        ModelAction::Download => download_model(&manager, req.name.as_deref(), out, &console),
        ModelAction::Delete => delete_model(&manager, req.name.as_deref(), out, &console),
        ModelAction::Info => model_info(&manager, req.name.as_deref(), out, &console),
        ModelAction::Benchmark => benchmark(
            &manager,
            &onnx,
            &config.paths.workspace_dir,
            req.name.as_deref(),
            out,
            &console,
        ),
    }
}

/// `model list`: the registry table with installation status (design D5):
/// NAME (registry identifier), DISPLAY (display name), DIM, STATUS — a
/// box-drawing table; the STATUS cell is styled (`installed ✓` green,
/// `not installed` dim).
fn list_models(
    manager: &ModelManager,
    onnx: &OnnxConfig,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    let models: Vec<&ModelInfo> = onnx
        .models
        .entries
        .iter()
        .filter(|model| !model.name.trim().is_empty())
        .collect();

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(models.len());
    for model in &models {
        let status = if manager.is_installed(&model.name) {
            console.style("installed ✓", Color::Green)
        } else {
            console.style("not installed", Color::Dim)
        };
        rows.push(vec![
            model.name.clone(),
            model.display_name.clone(),
            model.vector_dim.to_string(),
            status,
        ]);
    }

    writeln!(out, "{}", console.header("Available Models:"))?;
    writeln!(
        out,
        "{}",
        console.table(&["NAME", "DISPLAY", "DIM", "STATUS"], &rows, &[2])
    )?;
    writeln!(out)?;
    Ok(())
}

/// `model download [<name>]`: ensure the model files are installed.
fn download_model(
    manager: &ModelManager,
    name: Option<&str>,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    let name = match name {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => {
            let default = manager.default_model().to_string();
            writeln!(
                out,
                "{}",
                console.line(&format!("No model specified, using default: {default}"))
            )?;
            writeln!(out)?;
            default
        }
    };
    let info = manager
        .model(&name)
        .ok_or_else(|| CliError::Unsupported(format!("model {name:?} not found in registry")))?;
    writeln!(
        out,
        "{}",
        console.line(&format!(
            "Downloading {} ({})...",
            info.display_name, info.name
        ))
    )?;
    writeln!(out)?;

    manager.ensure_model(&name)?;
    writeln!(
        out,
        "\n{}",
        console.success(&format!(
            "Model installed at: {}",
            manager.model_dir(&name).display()
        ))
    )?;
    Ok(())
}

/// `model delete <name>`: remove an installed model.
fn delete_model(
    manager: &ModelManager,
    name: Option<&str>,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| CliError::Unsupported("model name is required".to_string()))?;
    manager
        .model(name)
        .ok_or_else(|| CliError::Unsupported(format!("model {name:?} not found in registry")))?;
    manager.delete_model(name)?;
    writeln!(out, "{}", console.success(&format!("Model {name} deleted")))?;
    Ok(())
}

/// `model info [<name>]`: detailed view of one registry entry.
fn model_info(
    manager: &ModelManager,
    name: Option<&str>,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    let name = match name {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => manager.default_model().to_string(),
    };
    let info = manager
        .model(&name)
        .ok_or_else(|| CliError::Unsupported(format!("model {name:?} not found in registry")))?;
    print_model_info(out, info, manager, console)
}

/// Renders the `model info` block (design D5): a borderless kv block
/// (label column auto-width) plus the plain indented `Files:` list.
fn print_model_info(
    out: &mut dyn Write,
    info: &ModelInfo,
    manager: &ModelManager,
    console: &Console,
) -> Result<(), CliError> {
    let installed = manager.is_installed(&info.name);
    let mut pairs: Vec<(&str, String)> = vec![
        ("Name", info.name.clone()),
        ("Display Name", info.display_name.clone()),
        ("Description", info.description.clone()),
        ("Version", info.version.clone()),
        ("Vector Dim", info.vector_dim.to_string()),
        ("Source", info.source.clone()),
    ];
    if !info.repo.is_empty() {
        pairs.push(("Repository", info.repo.clone()));
    }
    if installed && let Some(cache_info) = ModelCache::new(manager.models_dir()).info(&info.name) {
        pairs.push((
            "Installed At",
            format_installed_at(&cache_info.installed_at),
        ));
    }
    let status = if installed {
        console.style("Installed ✓", Color::Green)
    } else {
        console.style("Not installed", Color::Dim)
    };
    pairs.push(("Status", status));
    let rows: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(label, value)| (*label, value.as_str()))
        .collect();
    writeln!(out, "{}", console.kv(&rows))?;

    if !info.files.is_empty() {
        writeln!(out, "\nFiles:")?;
        for file in &info.files {
            let size = if file.size_bytes > 0 {
                format!("~{}", human_file_size(file.size_bytes))
            } else {
                "unknown".to_string()
            };
            let checksum = if file.checksum.as_deref().is_some_and(|c| !c.is_empty()) {
                " (checksum verified)"
            } else {
                ""
            };
            writeln!(
                out,
                "{}",
                console.line(&format!("  - {:<25} {}{}", file.name, size, checksum))
            )?;
        }
    }
    writeln!(out)?;
    Ok(())
}

/// `model benchmark [<name>]`: embedding speed of installed models
/// (see module docs).
fn benchmark(
    manager: &ModelManager,
    onnx: &OnnxConfig,
    data_dir: &str,
    requested: Option<&str>,
    out: &mut dyn Write,
    console: &Console,
) -> Result<(), CliError> {
    let targets = select_targets(manager, onnx, requested)?;

    writeln!(out, "{}", console.header("Embedding Benchmark"))?;
    let runtime = detect_runtime(data_dir, onnx);
    let cpu = if runtime.cpu_model.is_empty() {
        "unknown CPU"
    } else {
        &runtime.cpu_model
    };
    let ram = if runtime.mem_total_bytes > 0 {
        format!(", ~{} RAM", human_file_size(runtime.mem_total_bytes as i64))
    } else {
        String::new()
    };
    writeln!(
        out,
        "{}",
        console.line(&format!(
            "Hardware: {cpu}, {} logical CPUs{ram}",
            runtime.num_cpu
        ))
    )?;
    if !runtime.onnx_version.is_empty() {
        writeln!(
            out,
            "{}",
            console.line(&format!("ONNX Runtime: {}", runtime.onnx_version))
        )?;
    }

    for model in &targets {
        if !manager.is_installed(&model.name) {
            return Err(CliError::Unsupported(format!(
                "model {} is not installed; run 'synopsis model download {}' first",
                model.name, model.name
            )));
        }
        let stats = run_production_benchmark(model, data_dir, onnx)?;
        writeln!(
            out,
            "{}",
            console.line(&format!(
                "\n{} (dim={})",
                model.display_name, model.vector_dim
            ))
        )?;
        writeln!(
            out,
            "{}",
            console.line(&format!("  production: {}", format_stats(&stats)))
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// Selects the benchmark targets: the named model (must be in the registry)
/// or, with no name, every installed registry model.
fn select_targets<'a>(
    manager: &'a ModelManager,
    onnx: &'a OnnxConfig,
    requested: Option<&str>,
) -> Result<Vec<&'a ModelInfo>, CliError> {
    match requested {
        Some(name) if !name.is_empty() => {
            let info = manager.model(name).ok_or_else(|| {
                CliError::Unsupported(format!("model {name:?} not found in registry"))
            })?;
            Ok(vec![info])
        }
        _ => {
            let installed: Vec<&ModelInfo> = onnx
                .models
                .entries
                .iter()
                .filter(|model| !model.name.trim().is_empty() && manager.is_installed(&model.name))
                .collect();
            if installed.is_empty() {
                return Err(CliError::Unsupported(
                    "no installed models found; run 'synopsis model download <name>' first"
                        .to_string(),
                ));
            }
            Ok(installed)
        }
    }
}

/// Machine description for the benchmark header.
struct RuntimeInfo {
    cpu_model: String,
    num_cpu: usize,
    mem_total_bytes: u64,
    onnx_version: String,
}

/// Detects CPU model, logical CPU count, total RAM (from `/proc` on Linux;
/// zero/empty when unavailable), and the installed ONNX Runtime version
/// (from the library cache manifest).
fn detect_runtime(data_dir: &str, onnx: &OnnxConfig) -> RuntimeInfo {
    let num_cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|data| {
            data.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| value.trim().to_string())
            })
        })
        .unwrap_or_default();
    let mem_total_bytes = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|data| {
            data.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "MemTotal")
                    .then_some(value)
                    .and_then(|value| {
                        let fields: Vec<&str> = value.split_whitespace().collect();
                        fields
                            .first()
                            .and_then(|kb| kb.parse::<u64>().ok())
                            .filter(|_| fields.get(1).is_some_and(|unit| *unit == "kB"))
                            .map(|kb| kb * 1024)
                    })
            })
        })
        .unwrap_or(0);
    let onnx_version = match LibraryManager::new(data_dir, onnx) {
        Ok(library) if library.library_path().is_some() => library.version().to_string(),
        _ => String::new(),
    };
    RuntimeInfo {
        cpu_model,
        num_cpu,
        mem_total_bytes,
        onnx_version,
    }
}

/// Aggregated benchmark latencies in milliseconds.
struct BenchStats {
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    runs: usize,
}

/// Measures the production embedding path of one installed model: the real
/// `new_onnx_provider` (registry flow, installed models only — the caller's
/// `is_installed` gate runs first, so `ensure_model` performs no download)
/// with warmup + measured runs of unique sample texts (unique suffixes
/// bypass the embedding cache).
///
/// # Errors
///
/// [`CliError::Embedding`] when the runtime library, the model, the
/// tokenizer, or an inference run fails.
fn run_production_benchmark(
    model: &ModelInfo,
    data_dir: &str,
    onnx: &OnnxConfig,
) -> Result<BenchStats, CliError> {
    // Registry flow by name (registry-as-model-source-of-truth D3): the
    // dimension and file locations come from the registry entry.
    let local = LocalEmbedding {
        model_name: model.name.clone(),
    };
    let provider = new_onnx_provider(&local, data_dir, onnx)?;
    let mut samples = Vec::with_capacity(BENCH_WARMUP + BENCH_RUNS);
    for i in 0..(BENCH_WARMUP + BENCH_RUNS) {
        let text = format!("{BENCH_SAMPLE_TEXT} [sample {i}]");
        let start = Instant::now();
        provider.generate_embeddings(&[text])?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(stats_from_ms(&samples[BENCH_WARMUP..]))
}

/// Median/min/max of a sample.
fn stats_from_ms(samples: &[f64]) -> BenchStats {
    if samples.is_empty() {
        return BenchStats {
            median_ms: 0.0,
            min_ms: 0.0,
            max_ms: 0.0,
            runs: 0,
        };
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    };
    BenchStats {
        median_ms: median,
        min_ms: sorted[0],
        max_ms: sorted[sorted.len() - 1],
        runs: samples.len(),
    }
}

/// Renders the stats line.
fn format_stats(stats: &BenchStats) -> String {
    format!(
        "median {:.1} ms/text [min {:.1}, max {:.1}], {} runs",
        stats.median_ms, stats.min_ms, stats.max_ms, stats.runs
    )
}

/// Formats an RFC 3339 UTC installation timestamp as `YYYY-MM-DD HH:MM:SS`;
/// a value of a different shape is printed as-is.
fn format_installed_at(value: &str) -> String {
    let trimmed = value.strip_suffix('Z').unwrap_or(value);
    match trimmed.split_once('T') {
        Some((date, time)) if date.len() == 10 && time.len() == 8 => format!("{date} {time}"),
        _ => value.to_string(),
    }
}

/// Human-readable byte count
/// (`B` / `KiB` / `MiB` / `GiB` with 0/1/2 fraction digits).
fn human_file_size(bytes: i64) -> String {
    const UNIT: i64 = 1024;
    if bytes < UNIT {
        return format!("{bytes} B");
    }
    let mut divisor: i64 = 1;
    let mut exponent = 0;
    let mut magnitude = bytes;
    while magnitude >= UNIT && exponent < 3 {
        divisor *= UNIT;
        exponent += 1;
        magnitude /= UNIT;
    }
    let ratio = bytes as f64 / divisor as f64;
    match exponent {
        1 => format!("{ratio:.0} KiB"),
        2 => format!("{ratio:.1} MiB"),
        _ => format!("{ratio:.2} GiB"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use embedding::InstalledModel;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-model-{tag}-{}-{id}",
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

    /// Writes a minimal config (paths only — `model` needs no validation)
    /// pointing at `dir/data` and `dir/onnx.yaml`; returns the config path.
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

    /// The `onnx.yaml` registry fixture: a default model with two files
    /// (one with size + checksum) and a second model; all URLs unroutable —
    /// a test that reaches them has a bug.
    fn write_onnx(dir: &TempDir) {
        let yaml = r#"
runtime:
  version: "1.28.0"
models:
  default: bge-m3-int8
  entries:
    - name: bge-m3-int8
      display_name: BGE-M3 int8
      description: Multilingual embedding model (int8)
      version: 1.0.0
      vector_dim: 1024
      source: huggingface
      repo: BAAI/bge-m3
      files:
        - name: model.onnx
          url: http://127.0.0.1:1/model.onnx
          size_bytes: 2266820608
          checksum: "sha256:deadbeef"
        - name: tokenizer.json
          url: http://127.0.0.1:1/tokenizer.json
    - name: other
      display_name: Other Model
      description: A second registry entry
      version: 2.0.0
      vector_dim: 384
      files:
        - name: model.onnx
          url: http://127.0.0.1:1/other-model.onnx
    - name: paraphrase-multilingual-MiniLM-L12-v2
      display_name: Paraphrase Multilingual MiniLM
      description: A third registry entry with a 30-char display name
      version: 2.0.0
      vector_dim: 384
      files:
        - name: model.onnx
          url: http://127.0.0.1:1/paraphrase-model.onnx
"#;
        std::fs::write(dir.as_ref().join("onnx.yaml"), yaml).expect("write onnx.yaml");
    }

    /// Pre-installs the default model (files + manifest) so every
    /// installation check passes without any network access.
    fn preinstall(dir: &TempDir) {
        let model_dir = dir.as_ref().join("data").join("models").join("bge-m3-int8");
        std::fs::create_dir_all(&model_dir).expect("create model dir");
        std::fs::write(model_dir.join("model.onnx"), b"fake-model-bytes").expect("write model");
        std::fs::write(model_dir.join("tokenizer.json"), b"fake-tokenizer-bytes")
            .expect("write tokenizer");
        ModelCache::new(dir.as_ref().join("data").join("models"))
            .mark_installed(InstalledModel {
                name: "bge-m3-int8".to_string(),
                version: "1.0.0".to_string(),
                vector_dim: 1024,
                installed_at: "2026-08-21T00:00:00Z".to_string(),
            })
            .expect("mark installed");
    }

    fn run_flow(
        dir: &TempDir,
        action: ModelAction,
        name: Option<&str>,
    ) -> (Vec<u8>, Result<(), CliError>) {
        let cfg_path = write_config(dir);
        write_onnx(dir);
        let req = ModelRequest {
            cfg_path,
            action,
            name: name.map(str::to_string),
        };
        let mut out: Vec<u8> = Vec::new();
        let result = model_flow(&req, &mut out);
        (out, result)
    }

    fn stdout_of(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    // --- list ----------------------------------------------------------------

    #[test]
    fn list_prints_registry_table_with_install_status() {
        let dir = TempDir::new("list");
        preinstall(&dir);

        let (out, result) = run_flow(&dir, ModelAction::List, None);
        let stdout = stdout_of(&out);

        result.expect("list must succeed");
        assert!(stdout.starts_with("Available Models:\n"), "{stdout:?}");
        // Non-TTY rendering: no ANSI escapes, every line within 120 columns.
        assert!(!stdout.contains('\x1b'), "{stdout:?}");
        for line in stdout.lines() {
            assert!(line.chars().count() <= 120, "line fits 120: {line:?}");
        }
        // Box-drawing table with the four column headers.
        assert!(stdout.contains('┌'), "box border: {stdout:?}");
        assert!(stdout.contains('┬'), "box border: {stdout:?}");
        assert!(stdout.contains('┐'), "box border: {stdout:?}");
        for header in ["NAME", "DISPLAY", "DIM", "STATUS"] {
            assert!(stdout.contains(header), "header {header}: {stdout:?}");
        }
        // NAME column: registry identifiers; DISPLAY column: display names.
        assert!(stdout.contains("bge-m3-int8"), "{stdout:?}");
        assert!(stdout.contains("BGE-M3 int8"), "{stdout:?}");
        assert!(
            stdout.contains("installed ✓"),
            "installed model: {stdout:?}"
        );
        assert!(stdout.contains("Other Model"), "{stdout:?}");
        assert!(
            stdout.contains("not installed"),
            "uninstalled model: {stdout:?}"
        );
        // The former VERSION column is gone (four columns only).
        assert!(!stdout.contains("1.0.0"), "no version column: {stdout:?}");
        assert!(stdout.contains("1024"), "{stdout:?}");
        assert!(stdout.contains("384"), "{stdout:?}");
        // The 30-char display name: NAME and DISPLAY stay on the same row
        // (the table is content-fit below 120), alignment intact.
        let long_row = stdout
            .lines()
            .find(|line| line.contains("Paraphrase Multilingual MiniLM"))
            .expect("long display name row: {stdout:?}");
        assert!(
            long_row.contains("paraphrase-multilingual-MiniLM-L12-v2"),
            "NAME and DISPLAY on the same row: {long_row:?}"
        );
    }

    #[test]
    fn list_empty_registry_prints_header_only() {
        let dir = TempDir::new("list-empty");
        let cfg_path = write_config(&dir);
        std::fs::write(
            dir.as_ref().join("onnx.yaml"),
            "models:\n  default: \"\"\n  entries: []\n",
        )
        .expect("write empty registry");

        let mut out: Vec<u8> = Vec::new();
        let req = ModelRequest {
            cfg_path,
            action: ModelAction::List,
            name: None,
        };
        model_flow(&req, &mut out).expect("list must succeed");

        let stdout = stdout_of(&out);
        assert!(stdout.starts_with("Available Models:\n"), "{stdout:?}");
        assert!(stdout.contains('┌'), "header-only box table: {stdout:?}");
        assert!(stdout.contains("STATUS"), "{stdout:?}");
        assert!(!stdout.contains("bge-m3"), "no rows: {stdout:?}");
    }

    // --- info ------------------------------------------------------------------

    #[test]
    fn info_prints_all_fields_for_installed_model() {
        let dir = TempDir::new("info");
        preinstall(&dir);

        let (out, result) = run_flow(&dir, ModelAction::Info, Some("bge-m3-int8"));
        let stdout = stdout_of(&out);

        result.expect("info must succeed");
        // Non-TTY rendering: no ANSI escapes, every line within 120 columns.
        assert!(!stdout.contains('\x1b'), "{stdout:?}");
        for line in stdout.lines() {
            assert!(line.chars().count() <= 120, "line fits 120: {line:?}");
        }
        // kv block: the value column is aligned across rows (the label set's
        // longest label, "Display Name"/"Installed At", fixes the padding).
        assert!(stdout.contains("Name:         bge-m3-int8"), "{stdout:?}");
        assert!(stdout.contains("Display Name: BGE-M3 int8"), "{stdout:?}");
        assert!(
            stdout.contains("Description:  Multilingual embedding model (int8)"),
            "{stdout:?}"
        );
        assert!(stdout.contains("Version:      1.0.0"), "{stdout:?}");
        assert!(stdout.contains("Vector Dim:   1024"), "{stdout:?}");
        assert!(stdout.contains("Source:       huggingface"), "{stdout:?}");
        assert!(stdout.contains("Repository:   BAAI/bge-m3"), "{stdout:?}");
        assert!(
            stdout.contains("Installed At: 2026-08-21 00:00:00"),
            "date format: {stdout:?}"
        );
        assert!(stdout.contains("Status:       Installed ✓"), "{stdout:?}");
        assert!(stdout.contains("\nFiles:"), "{stdout:?}");
        assert!(stdout.contains("model.onnx"), "{stdout:?}");
        assert!(stdout.contains("~2.11 GiB"), "size formatting: {stdout:?}");
        assert!(stdout.contains("(checksum verified)"), "{stdout:?}");
        assert!(stdout.contains("tokenizer.json"), "{stdout:?}");
        assert!(stdout.contains("unknown"), "size-less file: {stdout:?}");
    }

    #[test]
    fn info_not_installed_model_omits_installed_at() {
        let dir = TempDir::new("info-uninstalled");

        let (out, result) = run_flow(&dir, ModelAction::Info, Some("other"));
        let stdout = stdout_of(&out);

        result.expect("info must succeed");
        assert!(stdout.contains("Name:         other"), "{stdout:?}");
        assert!(stdout.contains("Status:       Not installed"), "{stdout:?}");
        assert!(!stdout.contains("Installed At"), "{stdout:?}");
        assert!(
            !stdout.contains("Repository"),
            "empty repo is omitted: {stdout:?}"
        );
    }

    #[test]
    fn info_default_name_uses_registry_default() {
        let dir = TempDir::new("info-default");
        preinstall(&dir);

        let (out, result) = run_flow(&dir, ModelAction::Info, None);
        let stdout = stdout_of(&out);

        result.expect("info must succeed");
        assert!(stdout.contains("Name:         bge-m3-int8"), "{stdout:?}");
    }

    #[test]
    fn info_unknown_model_is_an_error() {
        let dir = TempDir::new("info-unknown");

        let (_, result) = run_flow(&dir, ModelAction::Info, Some("no-such-model"));

        let err = result.expect_err("unknown model must fail");
        assert!(
            err.to_string().contains("no-such-model"),
            "message names the model: {err}"
        );
        assert!(err.to_string().contains("not found in registry"), "{err}");
    }

    // --- download ----------------------------------------------------------------

    #[test]
    fn download_unknown_model_is_an_error() {
        let dir = TempDir::new("download-unknown");

        let (_, result) = run_flow(&dir, ModelAction::Download, Some("no-such-model"));

        assert!(result.is_err(), "unknown model must fail");
        assert!(
            !dir.as_ref().join("data").join("models").exists(),
            "no download for an unknown model"
        );
    }

    #[test]
    fn download_default_model_reports_default_then_fails_offline() {
        let dir = TempDir::new("download-default");
        // The registry URL is unroutable (127.0.0.1:1): the download fails
        // deterministically without touching the network.
        let (out, result) = run_flow(&dir, ModelAction::Download, None);
        let stdout = stdout_of(&out);

        result.expect_err("unroutable model URL must fail");
        assert!(
            stdout.contains("No model specified, using default: bge-m3-int8"),
            "{stdout:?}"
        );
        assert!(
            stdout.contains("Downloading BGE-M3 int8 (bge-m3-int8)..."),
            "{stdout:?}"
        );
    }

    // --- delete ------------------------------------------------------------------

    #[test]
    fn delete_removes_installed_model() {
        let dir = TempDir::new("delete");
        preinstall(&dir);

        let (out, result) = run_flow(&dir, ModelAction::Delete, Some("bge-m3-int8"));
        let stdout = stdout_of(&out);

        result.expect("delete must succeed");
        assert!(stdout.contains("✓ Model bge-m3-int8 deleted"), "{stdout:?}");
        assert!(
            !dir.as_ref()
                .join("data")
                .join("models")
                .join("bge-m3-int8")
                .exists(),
            "model directory removed"
        );
    }

    #[test]
    fn delete_requires_a_name() {
        let dir = TempDir::new("delete-no-name");

        let (_, result) = run_flow(&dir, ModelAction::Delete, None);

        let err = result.expect_err("missing name must fail");
        assert!(err.to_string().contains("model name is required"), "{err}");
    }

    #[test]
    fn delete_unknown_model_is_an_error() {
        let dir = TempDir::new("delete-unknown");

        let (_, result) = run_flow(&dir, ModelAction::Delete, Some("no-such-model"));

        assert!(result.is_err(), "unknown model must fail");
    }

    #[test]
    fn delete_not_installed_model_is_an_error() {
        let dir = TempDir::new("delete-uninstalled");

        let (_, result) = run_flow(&dir, ModelAction::Delete, Some("bge-m3-int8"));

        let err = result.expect_err("uninstalled model must fail");
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    // --- benchmark ----------------------------------------------------------------

    #[test]
    fn benchmark_without_installed_models_is_an_error() {
        let dir = TempDir::new("bench-empty");

        let (_, result) = run_flow(&dir, ModelAction::Benchmark, None);

        let err = result.expect_err("no installed models must fail");
        assert!(
            err.to_string().contains("no installed models found"),
            "{err}"
        );
        assert!(
            err.to_string().contains("synopsis model download"),
            "hint names the fix: {err}"
        );
    }

    #[test]
    fn benchmark_unknown_model_is_an_error() {
        let dir = TempDir::new("bench-unknown");

        let (_, result) = run_flow(&dir, ModelAction::Benchmark, Some("no-such-model"));

        let err = result.expect_err("unknown model must fail");
        assert!(err.to_string().contains("not found in registry"), "{err}");
    }

    // --- exit codes ----------------------------------------------------------------

    #[test]
    fn run_model_maps_success_and_failure_exit_codes() {
        let dir = TempDir::new("exit-codes");
        preinstall(&dir);
        let cfg_path = write_config(&dir);
        write_onnx(&dir);

        let ok = run_model(&ModelRequest {
            cfg_path: cfg_path.clone(),
            action: ModelAction::List,
            name: None,
        });
        assert_eq!(ok, ExitCode::SUCCESS, "list exits 0");

        let fail = run_model(&ModelRequest {
            cfg_path,
            action: ModelAction::Info,
            name: Some("no-such-model".to_string()),
        });
        assert_eq!(fail, ExitCode::FAILURE, "unknown model exits 1");
    }

    // --- helpers -------------------------------------------------------------------

    #[test]
    fn format_installed_at_renders_date_format() {
        assert_eq!(
            format_installed_at("2026-08-21T10:15:30Z"),
            "2026-08-21 10:15:30"
        );
        // A non-RFC-3339 value is printed as-is (never a parse failure).
        assert_eq!(format_installed_at("garbage"), "garbage");
        assert_eq!(
            format_installed_at("2026-08-21T10:15:30.5Z"),
            "2026-08-21T10:15:30.5Z"
        );
    }

    /// Golden values for the human-readable byte formatting.
    #[test]
    fn human_file_size_matches_goldens() {
        assert_eq!(human_file_size(0), "0 B");
        assert_eq!(human_file_size(366), "366 B");
        assert_eq!(human_file_size(724923), "708 KiB");
        assert_eq!(human_file_size(133093490), "126.9 MiB");
        assert_eq!(human_file_size(2266820608), "2.11 GiB");
    }

    #[test]
    fn stats_from_ms_computes_median_min_max() {
        let even = stats_from_ms(&[4.0, 1.0, 3.0, 2.0]);
        assert_eq!(even.median_ms, 2.5, "even count averages the middle pair");
        assert_eq!(even.min_ms, 1.0);
        assert_eq!(even.max_ms, 4.0);
        assert_eq!(even.runs, 4);

        let odd = stats_from_ms(&[3.0, 1.0, 2.0]);
        assert_eq!(odd.median_ms, 2.0, "odd count takes the middle");
        assert_eq!(odd.runs, 3);

        assert_eq!(stats_from_ms(&[]).runs, 0);
    }
}
