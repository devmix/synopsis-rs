//! `synopsis` binary entry point (design D1/D2).
//!
//! Parse global flags + subcommand → resolve the config path → load the
//! config just for the logging level (the full bootstrap arrives with the
//! per-subcommand tasks) → init tracing → dispatch. Real subcommand bodies
//! arrive in tasks 1.6-1.10 (`serve`, `model` and `onnx-runtime` are
//! implemented; `load-test` still dispatches to a stub that prints an
//! error to stderr and exits 1).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cli::cli::{Cli, Subcommand};
use cli::config_resolver::resolve_config_path;
use cli::db::{DbRequest, run_db};
use cli::loadtest::LoadTestRequest;
use cli::model::{ModelRequest, run_model};
use cli::onnx_runtime::{OnnxRuntimeRequest, run_onnx_runtime};
use cli::queue::{QueueRequest, run_queue};
use cli::serve::server::{ServeRequest, run_serve};

fn main() -> ExitCode {
    let cli = match Cli::parse() {
        Ok(cli) => cli,
        Err(err) => {
            // clap's try_get_matches reports `--version` as an error variant;
            // the version line goes to stdout and the process exits 0.
            if err.kind() == clap::error::ErrorKind::DisplayVersion {
                let _ = err.print();
                return ExitCode::SUCCESS;
            }
            return usage_error(&err);
        }
    };

    // Resolve config path: explicit --config > preset-based auto-search.
    let cfg_path = resolve_config_path(cli.config.as_deref().map(Path::new), &cli.preset);

    // Load the config just to read logging.level before dispatch (design D2);
    // the rest of the config is consumed by the per-subcommand bootstrap.
    let cfg = match config::load(&cfg_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("failed to load config {}: {err}", cfg_path.display());
            return ExitCode::FAILURE;
        }
    };
    init_tracing(&cfg.logging.level);

    dispatch(cli, cfg_path)
}

/// Routes the parsed subcommand to its handler. `cfg_path` is the resolved
/// configuration path (explicit `--config` > preset auto-search) the
/// subcommand bootstraps from.
fn dispatch(cli: Cli, cfg_path: PathBuf) -> ExitCode {
    let dataset = cli.dataset;
    match cli.command {
        Subcommand::Serve {
            no_initial_sync,
            port,
            auto_rebuild_vectors,
        } => run_serve(&ServeRequest {
            cfg_path,
            dataset,
            no_initial_sync,
            port,
            auto_rebuild_vectors,
        }),
        Subcommand::Queue { action } => run_queue(&QueueRequest {
            cfg_path,
            dataset,
            action,
        }),
        Subcommand::Db { action } => run_db(&DbRequest {
            cfg_path,
            dataset,
            action,
        }),
        Subcommand::Model { action, name } => run_model(&ModelRequest {
            cfg_path,
            action,
            name,
        }),
        Subcommand::OnnxRuntime { action } => {
            run_onnx_runtime(&OnnxRuntimeRequest { cfg_path, action })
        }
        Subcommand::LoadTest {
            scale,
            seed,
            iterations,
            json,
            no_fill,
        } => run_load_test(
            &cfg_path,
            dataset.as_deref(),
            &scale,
            seed,
            iterations,
            json,
            no_fill,
        ),
    }
}

/// Runs the `load-test` subcommand (task 1.10).
fn run_load_test(
    cfg_path: &Path,
    dataset: Option<&str>,
    scale: &str,
    seed: i64,
    iterations: u32,
    json: Option<String>,
    no_fill: bool,
) -> ExitCode {
    let req = LoadTestRequest {
        scale: scale.to_owned(),
        seed,
        iterations: iterations as usize,
        json,
        no_fill,
    };

    match cli::loadtest::run_load_test(cfg_path, dataset, &req) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Prints a clap usage error to stderr and exits 1.
///
/// Usage errors (unknown subcommand, missing subcommand) exit 1; clap's
/// built-in handler would exit 2.
fn usage_error(err: &clap::Error) -> ExitCode {
    let _ = err.print();
    ExitCode::FAILURE
}

/// Initializes the tracing subscriber once from `config.Logging.Level`
/// (design D2). `RUST_LOG` wins over the config level when set. Library
/// crates stay logger-less; only this binary logs.
fn init_tracing(level: &config::preset::LogLevel) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level_to_filter(level)));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Maps a config log level to its tracing filter word.
fn level_to_filter(level: &config::preset::LogLevel) -> &'static str {
    match level {
        config::preset::LogLevel::Trace => "trace",
        config::preset::LogLevel::Debug => "debug",
        config::preset::LogLevel::Info => "info",
        config::preset::LogLevel::Warn => "warn",
        config::preset::LogLevel::Error => "error",
        // Tolerant enum: an unrecognized level cannot break startup (design D2:
        // default info).
        config::preset::LogLevel::Unknown(_) => "info",
    }
}
