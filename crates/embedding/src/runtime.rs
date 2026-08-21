//! ONNX Runtime environment and session construction.
//!
//! This module is the single isolation point for the `ort` API (design D1,
//! risk section): if the pinned pre-release `ort` churns its API, only this
//! file needs to change.
//!
//! The ONNX Runtime is an external shared library (`.so`/`.dylib`/`.dll`)
//! loaded from an explicit path (design D1/D10), never from the system search
//! path. [`init_runtime`] must be called once before any [`build_session`]
//! call; both are only meaningful after the library file has been ensured by
//! the library manager (task 1.4).

use std::path::Path;

use ort::session::Session;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};

use crate::error::EmbeddingError;

/// Intra-op thread count for inference sessions (design D7): bge-m3 int8 is a
/// small model, and 2 threads keeps a 16 GB laptop from running hot.
const INTRA_THREADS: usize = 2;

/// Inter-op thread count for inference sessions (design D7).
const INTER_THREADS: usize = 1;

/// Initializes the ONNX Runtime by dynamically loading the shared library at
/// `lib_path`.
///
/// Must be called before [`build_session`] (and any other `ort` use in this
/// process). A second call in the same process is a no-op: the library handle
/// and the environment are process-wide singletons, mirroring the oracle's
/// `IsInitialized` guard in `newONNXProviderContext`.
///
/// # Errors
///
/// [`EmbeddingError::Ort`] if the library cannot be loaded: missing file,
/// unreadable or malformed dynamic library, missing `OrtGetApiBase` export, or
/// an ONNX Runtime version older than the one `ort` was built against.
pub fn init_runtime(lib_path: &Path) -> Result<(), EmbeddingError> {
    // `init_from` performs the dlopen and validates the exported API symbol
    // and the runtime version; `commit` registers the environment options
    // (the environment itself is created lazily by `ort` on first use).
    // A `false` return means an environment was already configured in this
    // process — a no-op, not an error.
    let _ = ort::init_from(lib_path)?.commit();
    Ok(())
}

/// Builds an inference session for the ONNX model at `model_path`.
///
/// Session options are fixed by design D7: intra-op threads = 2, inter-op
/// threads = 1, graph optimization level 1. QDQ quantization fusion for the
/// int8 bge-m3 model is ONNX Runtime's default and needs no explicit option.
///
/// The model file is checked to exist before any `ort` API is touched:
/// constructing an `ort::Error` requires the runtime library to be loaded, and
/// under `load-dynamic` that panics rather than returns an error. The
/// existence check mirrors the oracle's `os.Stat` pre-check in
/// `newONNXProviderContext`.
///
/// # Errors
///
/// [`EmbeddingError::Model`] if `model_path` does not exist;
/// [`EmbeddingError::Ort`] for session creation failures.
pub fn build_session(model_path: &Path) -> Result<Session, EmbeddingError> {
    if !model_path.exists() {
        return Err(EmbeddingError::Model(format!(
            "model file does not exist: {}",
            model_path.display()
        )));
    }
    let mut builder = Session::builder()?
        .with_intra_threads(INTRA_THREADS)
        .map_err(builder_error)?
        .with_inter_threads(INTER_THREADS)
        .map_err(builder_error)?
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(builder_error)?;
    Ok(builder.commit_from_file(model_path)?)
}

/// Converts a session-builder error into [`EmbeddingError::Ort`].
///
/// `ort` builder methods return the error parameterized by the recoverable
/// builder value; only the rendered message is needed for logging.
fn builder_error(err: ort::Error<SessionBuilder>) -> EmbeddingError {
    EmbeddingError::Ort(err.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    /// A path inside the system temp dir that is guaranteed not to exist.
    fn nonexistent_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("embedding-rt-{}-{suffix}", std::process::id()))
    }

    /// `init_runtime` with a missing library file returns `EmbeddingError::Ort`
    /// instead of panicking.
    ///
    /// Deterministic only while no other test in this process has loaded the
    /// ONNX Runtime first (the library handle is a process-wide singleton);
    /// CI never runs the `#[ignore]`d test below, so this holds.
    #[test]
    fn init_runtime_missing_library_returns_ort_error() {
        let path = nonexistent_path("missing-lib.so");
        let err = init_runtime(&path).unwrap_err();
        match err {
            EmbeddingError::Ort(msg) => assert!(
                msg.contains("missing-lib.so"),
                "message should name the failed path, got: {msg}"
            ),
            other => panic!("expected Ort, got: {other:?}"),
        }
    }

    /// `build_session` with a missing model file returns `EmbeddingError::Model`
    /// before any `ort` API is touched (no panic without the runtime library).
    #[test]
    fn build_session_missing_model_returns_model_error() {
        let path = nonexistent_path("missing-model.onnx");
        let err = build_session(&path).unwrap_err();
        match err {
            EmbeddingError::Model(msg) => assert!(
                msg.contains("missing-model.onnx"),
                "message should name the failed path, got: {msg}"
            ),
            other => panic!("expected Model, got: {other:?}"),
        }
    }

    /// End-to-end with the real ONNX Runtime library and model.
    ///
    /// Run manually:
    /// `EMBEDDING_TEST_ONNXRUNTIME_LIB=/path/libonnxruntime.so \
    ///  EMBEDDING_TEST_MODEL=/path/model.onnx cargo test -p embedding -- --ignored`
    #[test]
    #[ignore]
    fn init_runtime_and_build_session_with_real_library() {
        let lib = std::env::var("EMBEDDING_TEST_ONNXRUNTIME_LIB")
            .expect("set EMBEDDING_TEST_ONNXRUNTIME_LIB to a real onnxruntime .so/.dylib");
        let model = std::env::var("EMBEDDING_TEST_MODEL")
            .expect("set EMBEDDING_TEST_MODEL to a real .onnx model");
        init_runtime(Path::new(&lib)).unwrap();
        let session = build_session(Path::new(&model)).unwrap();
        assert!(!session.inputs().is_empty());
    }
}
