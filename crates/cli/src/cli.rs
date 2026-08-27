//! Command-line interface definition (clap 4.6.6 Builder API, design D1).
//!
//! Invocation format: `synopsis [--config PATH] [--preset NAME] [--db PATH]
//! <subcommand> [flags...]`. Global flags precede the subcommand; per-command
//! flags follow it. Help text is Rust-idiomatic and deliberately NOT
//! byte-matched to the Go oracle (user decision 2026-08-27); flags, defaults
//! and behavior stay faithful to the frozen cli-surface spec and the oracle
//! (`../synopsis/cmd/app/main.go`).

use clap::{Arg, ArgAction, Command as ClapCommand};

/// Default configuration preset name (`config.{preset}.yaml`).
pub const DEFAULT_PRESET: &str = "default";
/// Default dataset scale for `load-test`.
pub const DEFAULT_SCALE: &str = "small";
/// Default PRNG seed for `load-test`.
///
/// Keep in sync with the `"42"` literal in [`build_command`]: clap's
/// `default_value` accepts string types only, so the typed constant cannot be
/// shared with the builder.
pub const DEFAULT_SEED: i64 = 42;
/// Default measured iterations per tool case for `load-test`.
///
/// Keep in sync with the `"100"` literal in [`build_command`] (same reason as
/// [`DEFAULT_SEED`]).
pub const DEFAULT_ITERATIONS: u32 = 100;

/// Parsed command line: global flags plus exactly one subcommand.
pub struct Cli {
    /// Explicit config path (`--config`); wins over preset auto-search.
    pub config: Option<String>,
    /// Configuration preset name (`--preset`, default `default`).
    pub preset: String,
    /// SQLite database path override (`--db`).
    pub db: Option<String>,
    /// The subcommand to run.
    pub command: Subcommand,
}

/// A parsed subcommand with its per-command flags.
pub enum Subcommand {
    /// `sync`: one-shot full re-index of all sources.
    Sync {
        /// Clear all existing data before re-indexing.
        rebuild: bool,
        /// Automatically rebuild vectors on dimension mismatch.
        auto_rebuild_vectors: bool,
    },
    /// `serve`: long-running MCP server mode.
    Serve {
        /// Skip the full source scan on startup.
        no_initial_sync: bool,
        /// HTTP listen port; `0` keeps the `server.port` config value.
        port: u16,
        /// Automatically rebuild vectors on dimension mismatch.
        auto_rebuild_vectors: bool,
    },
    /// `model`: manage embedding models.
    Model {
        /// The model sub-action.
        action: ModelAction,
        /// Model name (oracle: optional positional after the sub-action).
        name: Option<String>,
    },
    /// `onnx-runtime`: manage the ONNX runtime library.
    OnnxRuntime {
        /// The runtime sub-action.
        action: OnnxRuntimeAction,
    },
    /// `load-test`: benchmark the MCP tool handlers on generated data.
    LoadTest {
        /// Dataset scale: `small`, `medium` or `large`.
        scale: String,
        /// PRNG seed for deterministic data generation.
        seed: i64,
        /// Measured iterations per tool case.
        iterations: u32,
        /// Write the report as JSON to this path.
        json: Option<String>,
        /// Benchmark an existing database without generating data.
        no_fill: bool,
    },
}

/// `model` sub-actions (oracle: `cmd/app/model_cmd.go`).
pub enum ModelAction {
    /// List available models.
    List,
    /// Download a model.
    Download,
    /// Delete a model.
    Delete,
    /// Show model info.
    Info,
    /// Benchmark a model.
    Benchmark,
}

/// `onnx-runtime` sub-actions (oracle: `cmd/app/onnx_runtime.go`).
pub enum OnnxRuntimeAction {
    /// Install the ONNX runtime library.
    Install,
    /// Show runtime status.
    Status,
    /// Uninstall the ONNX runtime library.
    Uninstall,
}

/// Builds the top-level clap command: global flags + five subcommands.
fn build_command() -> ClapCommand {
    ClapCommand::new("synopsis")
        .about("Synopsis RAG service")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("PATH")
                .action(ArgAction::Set)
                .help("path to configuration file (default: auto-search)")
                .global(true),
        )
        .arg(
            Arg::new("preset")
                .long("preset")
                .value_name("NAME")
                .action(ArgAction::Set)
                .default_value(DEFAULT_PRESET)
                .help("configuration preset name (config.{preset}.yaml)")
                .global(true),
        )
        .arg(
            Arg::new("db")
                .long("db")
                .value_name("PATH")
                .action(ArgAction::Set)
                .help("path to SQLite database file (overrides config)")
                .global(true),
        )
        .subcommand_required(true)
        .subcommand(
            ClapCommand::new("sync")
                .about("force a full re-index of all sources and exit")
                .arg(
                    Arg::new("rebuild")
                        .long("rebuild")
                        .action(ArgAction::SetTrue)
                        .help("clear all existing data before re-indexing"),
                )
                .arg(auto_rebuild_vectors_flag()),
        )
        .subcommand(
            ClapCommand::new("serve")
                .about("start MCP server with auto-update (initial sync + file watching)")
                .arg(
                    Arg::new("no_initial_sync")
                        .long("no-initial-sync")
                        .action(ArgAction::SetTrue)
                        .help("skip the full source scan on startup"),
                )
                .arg(
                    Arg::new("port")
                        .long("port")
                        .value_name("N")
                        .action(ArgAction::Set)
                        .value_parser(clap::value_parser!(u16))
                        .default_value("0")
                        .help("HTTP listen port (overrides server.port config; default 8080)"),
                )
                .arg(auto_rebuild_vectors_flag()),
        )
        .subcommand(build_model_command())
        .subcommand(build_onnx_runtime_command())
        .subcommand(
            ClapCommand::new("load-test")
                .about("fill the DB with synthetic data and benchmark all MCP tool handlers")
                .arg(
                    Arg::new("scale")
                        .long("scale")
                        .value_name("SCALE")
                        .action(ArgAction::Set)
                        .default_value(DEFAULT_SCALE)
                        .help("dataset scale: small, medium or large"),
                )
                .arg(
                    Arg::new("seed")
                        .long("seed")
                        .value_name("N")
                        .action(ArgAction::Set)
                        .value_parser(clap::value_parser!(i64))
                        .default_value("42")
                        .help("PRNG seed for deterministic data generation"),
                )
                .arg(
                    Arg::new("iterations")
                        .long("iterations")
                        .value_name("N")
                        .action(ArgAction::Set)
                        .value_parser(clap::value_parser!(u32))
                        .default_value("100")
                        .help("measured iterations per tool case"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .value_name("PATH")
                        .action(ArgAction::Set)
                        .help("write the report as JSON to this path"),
                )
                .arg(
                    Arg::new("no_fill")
                        .long("no-fill")
                        .action(ArgAction::SetTrue)
                        .help("benchmark an existing database without generating data"),
                ),
        )
}

/// The `--auto-rebuild-vectors` flag shared by `sync` and `serve`.
fn auto_rebuild_vectors_flag() -> Arg {
    Arg::new("auto_rebuild_vectors")
        .long("auto-rebuild-vectors")
        .action(ArgAction::SetTrue)
        .help("automatically rebuild vectors on dimension mismatch")
}

/// Builds the `model` subcommand: `list|download|delete|info|benchmark`.
///
/// Every action carries the same optional `MODEL_NAME` positional (oracle:
/// `subArgs[1]` is parsed for all actions; `list` simply ignores it).
fn build_model_command() -> ClapCommand {
    let name_arg = || {
        Arg::new("model_name")
            .value_name("MODEL_NAME")
            .num_args(0..=1)
    };

    ClapCommand::new("model")
        .about("manage embedding models (list, download, delete, info, benchmark)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            ClapCommand::new("list")
                .about("list available models")
                .arg(name_arg()),
        )
        .subcommand(
            ClapCommand::new("download")
                .about("download a model")
                .arg(name_arg()),
        )
        .subcommand(
            ClapCommand::new("delete")
                .about("delete a model")
                .arg(name_arg()),
        )
        .subcommand(
            ClapCommand::new("info")
                .about("show model info")
                .arg(name_arg()),
        )
        .subcommand(
            ClapCommand::new("benchmark")
                .about("benchmark a model")
                .arg(name_arg()),
        )
}

/// Builds the `onnx-runtime` subcommand: `install|status|uninstall`.
fn build_onnx_runtime_command() -> ClapCommand {
    ClapCommand::new("onnx-runtime")
        .about("manage the ONNX runtime bundled with the binary")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(ClapCommand::new("install").about("install the ONNX runtime library"))
        .subcommand(ClapCommand::new("status").about("show runtime status"))
        .subcommand(ClapCommand::new("uninstall").about("uninstall the ONNX runtime library"))
}

impl Cli {
    /// Parses process arguments: global flags plus one required subcommand.
    ///
    /// Returns the clap error instead of exiting so the caller can control the
    /// exit code (the oracle exits 1 on usage errors, not clap's default 2).
    pub fn parse() -> Result<Self, clap::Error> {
        let matches = build_command().try_get_matches()?;
        Ok(Self::from_matches(&matches))
    }

    /// Maps parsed clap matches onto the typed [`Cli`] structure.
    ///
    /// Values with declared defaults are read back through the same constants
    /// the builder uses, so a missing value degrades to the documented default
    /// instead of panicking.
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        // `build_command` sets `subcommand_required(true)`, so a missing
        // subcommand is rejected by clap before this point.
        let (name, sub) = match matches.subcommand() {
            Some(pair) => pair,
            None => unreachable!("clap rejected the command: subcommand is required"),
        };
        let command = match name {
            "sync" => Subcommand::Sync {
                rebuild: sub.get_flag("rebuild"),
                auto_rebuild_vectors: sub.get_flag("auto_rebuild_vectors"),
            },
            "serve" => Subcommand::Serve {
                no_initial_sync: sub.get_flag("no_initial_sync"),
                port: sub.get_one::<u16>("port").copied().unwrap_or(0),
                auto_rebuild_vectors: sub.get_flag("auto_rebuild_vectors"),
            },
            "model" => {
                let (action_name, action_matches) = match sub.subcommand() {
                    Some(pair) => pair,
                    None => unreachable!("clap rejected the command: model sub-action required"),
                };
                let action = match action_name {
                    "list" => ModelAction::List,
                    "download" => ModelAction::Download,
                    "delete" => ModelAction::Delete,
                    "info" => ModelAction::Info,
                    "benchmark" => ModelAction::Benchmark,
                    other => unreachable!("clap only accepts the declared sub-actions: {other}"),
                };
                Subcommand::Model {
                    action,
                    name: action_matches.get_one::<String>("model_name").cloned(),
                }
            }
            "onnx-runtime" => {
                let (action_name, _) = match sub.subcommand() {
                    Some(pair) => pair,
                    None => {
                        unreachable!("clap rejected the command: onnx-runtime sub-action required")
                    }
                };
                let action = match action_name {
                    "install" => OnnxRuntimeAction::Install,
                    "status" => OnnxRuntimeAction::Status,
                    "uninstall" => OnnxRuntimeAction::Uninstall,
                    other => unreachable!("clap only accepts the declared sub-actions: {other}"),
                };
                Subcommand::OnnxRuntime { action }
            }
            "load-test" => Subcommand::LoadTest {
                scale: sub
                    .get_one::<String>("scale")
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_SCALE.to_string()),
                seed: sub.get_one::<i64>("seed").copied().unwrap_or(DEFAULT_SEED),
                iterations: sub
                    .get_one::<u32>("iterations")
                    .copied()
                    .unwrap_or(DEFAULT_ITERATIONS),
                json: sub.get_one::<String>("json").cloned(),
                no_fill: sub.get_flag("no_fill"),
            },
            other => unreachable!("clap only accepts the declared subcommands: {other}"),
        };
        Self {
            config: matches.get_one::<String>("config").cloned(),
            preset: matches
                .get_one::<String>("preset")
                .cloned()
                .unwrap_or_else(|| DEFAULT_PRESET.to_string()),
            db: matches.get_one::<String>("db").cloned(),
            command,
        }
    }
}
