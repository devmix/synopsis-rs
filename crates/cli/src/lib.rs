//! Library half of the CLI crate: argument parsing, config-path resolution
//! and subcommand dispatch. Oracle mapping: `../synopsis/cmd/app` (design D1).
//!
//! The `synopsis` binary (`src/main.rs`) is a thin entry point over this
//! library: parse → resolve config path → init tracing → dispatch.

pub mod cli;
pub mod config_resolver;
pub mod error;
pub mod model;
pub mod onnx_runtime;
pub mod serve;
pub mod sync;
