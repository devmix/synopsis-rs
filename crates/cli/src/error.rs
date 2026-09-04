//! CLI boundary error type and exit-code mapping.
//!
//! [`CliError`] wraps every upstream failure that can surface while the
//! `synopsis` binary bootstraps or runs a subcommand (config, database,
//! embedding, vectors) into one enum, and [`CliError::exit_code`] maps it to
//! the process exit code. The binary exits 1 for every fatal startup error
//! and 0 on success, so every variant maps to [`ExitCode::FAILURE`].

use std::process::ExitCode;

use thiserror::Error;

use config::ConfigError;
use db::DbError;
use embedding::EmbeddingError;
use graph::GraphError;
use vectors::VectorsError;

/// Boundary error of the `synopsis` CLI: wraps the upstream crate errors that
/// can surface during bootstrap and subcommand execution.
#[derive(Debug, Error)]
pub enum CliError {
    /// Config load / defaults / validation failure (main preset, onnx.yaml,
    /// ontology XML).
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    /// Database open / migration / query failure.
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// Embedding pipeline failure (ONNX runtime library, model download,
    /// session, tokenizer).
    #[error("embedding: {0}")]
    Embedding(#[from] EmbeddingError),
    /// Vector index failure (vector engine, dimension mismatch, fixture
    /// format).
    #[error("vectors: {0}")]
    Vectors(#[from] VectorsError),
    /// I/O failure outside the storage layer (binding the listen address,
    /// dropping the stored vector table on a dimension-mismatch rebuild).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Knowledge-graph failure (index build / reload).
    #[error("graph: {0}")]
    Graph(#[from] GraphError),
    /// A configuration value the build does not support (e.g.
    /// `embeddings.mode: api`, a duplicate domain name).
    #[error("{0}")]
    Unsupported(String),
}

impl CliError {
    /// The process exit code for this error.
    ///
    /// Every fatal startup error exits 1; success exits 0.
    /// Kept as a method so a future variant can map to a distinct code
    /// without touching the call sites.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        ExitCode::FAILURE
    }
}
