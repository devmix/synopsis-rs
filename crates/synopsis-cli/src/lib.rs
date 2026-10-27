//! Library half of the CLI crate: argument parsing, config-path resolution
//! and subcommand dispatch (design D1).
//!
//! The `synopsis` binary (`src/main.rs`) is a thin entry point over this
//! library: parse → resolve config path → init tracing → dispatch.

pub mod cli;
pub mod config_resolver;
pub mod console;
pub mod db;
pub mod error;
pub mod loadtest;
pub mod model;
pub mod onnx_runtime;
pub mod queue;
pub mod serve;

/// Test-only seams for integration tests (change test-hygiene-phase-2
/// task 2.4); docs-hidden, not part of the public API.
#[doc(hidden)]
pub mod test_support;
