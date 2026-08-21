//! Crate error type.
//!
//! [`EmbeddingError`] covers the whole embedding pipeline: ONNX Runtime
//! environment/session failures, tokenization, downloads, the in-memory cache,
//! model management, filesystem I/O, and configuration problems.

use thiserror::Error;

/// Errors produced by the embedding pipeline.
///
/// String-carrying variants hold a human-readable message with enough context
/// to log; the external error types ([`ort::Error`], [`ort::LoadDynamicError`],
/// [`std::io::Error`]) are converted via `From` so call sites can use `?`.
#[derive(Debug, Error)]
pub enum EmbeddingError {
    /// ONNX Runtime environment or session failure.
    ///
    /// Carries the rendered message rather than the `ort` error value:
    /// constructing an `ort::Error` invokes the ONNX Runtime C API, which under
    /// the `load-dynamic` feature panics when the runtime library is not loaded
    /// — exactly the failure mode this variant reports (library load failures
    /// arrive as [`ort::LoadDynamicError`] and are converted via `From`).
    #[error("onnx runtime error: {0}")]
    Ort(String),

    /// Tokenizer loading or tokenization failure.
    #[error("tokenizer error: {0}")]
    Tokenizer(String),

    /// Model or runtime-library download failure (retries exhausted, SSRF
    /// rejection, or post-download size mismatch).
    #[error("download error: {0}")]
    Download(String),

    /// In-memory embedding cache failure.
    #[error("cache error: {0}")]
    Cache(String),

    /// Model management failure (unknown model name, missing files,
    /// installation not completed).
    #[error("model error: {0}")]
    Model(String),

    /// Filesystem I/O failure (read, write, archive extraction).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error (invalid onnx.yaml entry, missing platform key).
    #[error("config error: {0}")]
    Config(String),
}

impl From<ort::Error> for EmbeddingError {
    fn from(err: ort::Error) -> Self {
        Self::Ort(err.to_string())
    }
}

impl From<ort::LoadDynamicError> for EmbeddingError {
    fn from(err: ort::LoadDynamicError) -> Self {
        Self::Ort(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::error::Error;

    use super::*;

    #[test]
    fn tokenizer_variant_display_contains_message() {
        let err = EmbeddingError::Tokenizer("bad pattern".to_string());
        assert_eq!(err.to_string(), "tokenizer error: bad pattern");
    }

    #[test]
    fn download_variant_display_contains_message() {
        let err = EmbeddingError::Download("retries exhausted".to_string());
        assert_eq!(err.to_string(), "download error: retries exhausted");
    }

    #[test]
    fn cache_variant_display_contains_message() {
        let err = EmbeddingError::Cache("eviction failed".to_string());
        assert_eq!(err.to_string(), "cache error: eviction failed");
    }

    #[test]
    fn model_variant_display_contains_message() {
        let err = EmbeddingError::Model("unknown model".to_string());
        assert_eq!(err.to_string(), "model error: unknown model");
    }

    #[test]
    fn config_variant_display_contains_message() {
        let err = EmbeddingError::Config("missing platform key".to_string());
        assert_eq!(err.to_string(), "config error: missing platform key");
    }

    #[test]
    fn io_error_converts_via_from_and_keeps_source() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing model file");
        let err = EmbeddingError::from(io);
        assert_eq!(err.to_string(), "io error: missing model file");
        assert!(err.source().is_some());
    }

    // Constructing an `ort::Error` calls into the ONNX Runtime C API, so with
    // load-dynamic this needs the real .so/.dylib — ignored in CI (task rule:
    // all tests requiring the real ONNX Runtime are `#[ignore]`).
    #[test]
    #[ignore]
    fn ort_error_converts_via_from_and_renders_message() {
        let ort_err = ort::Error::new_with_code(ort::ErrorCode::GenericFailure, "boom");
        let err = EmbeddingError::from(ort_err);
        assert_eq!(err.to_string(), "onnx runtime error: boom");
    }

    #[test]
    fn implements_std_error() {
        let err = EmbeddingError::Model("x".to_string());
        let as_dyn: &dyn std::error::Error = &err;
        assert_eq!(as_dyn.to_string(), "model error: x");
    }
}
