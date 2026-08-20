//! Embedding pipeline: ONNX Runtime lifecycle, model and library management,
//! tokenization, in-memory caching, and the bge-m3 int8 embedding provider.
//!
//! Oracle mapping: `../synopsis/internal/embedding` + `../synopsis/internal/onnx`
//! (design.md D1/D5). Per the migration principle the Go oracle is a reference for
//! behavior and contracts only — this crate is re-architected for Rust, not
//! transcribed (see design decisions D1–D10).
//!
//! Current public API (task 1.1): [`EmbeddingProvider`] and [`EmbeddingError`].
//! The runtime, downloader, library/model managers, tokenizer, cache and the ONNX
//! provider land in tasks 1.2–1.9.

pub mod error;

pub use error::EmbeddingError;

/// Generates vector embeddings for batches of texts.
///
/// Implementations are `Send + Sync` so a single instance can be shared (e.g.
/// behind `Arc`) across threads. Methods are synchronous and may block on
/// inference or disk I/O; async callers are responsible for dispatching calls
/// onto a blocking thread pool (design D4).
///
/// # Examples
///
/// ```
/// use embedding::EmbeddingProvider;
///
/// struct ConstProvider {
///     dim: usize,
/// }
///
/// impl EmbeddingProvider for ConstProvider {
///     fn generate_embeddings(
///         &self,
///         texts: &[String],
///     ) -> Result<Vec<Vec<f32>>, embedding::EmbeddingError> {
///         Ok(vec![vec![1.0; self.dim]; texts.len()])
///     }
///
///     fn vector_dim(&self) -> usize {
///         self.dim
///     }
///
///     fn name(&self) -> &'static str {
///         "const"
///     }
/// }
///
/// fn assert_send_sync<T: Send + Sync>() {}
/// assert_send_sync::<ConstProvider>();
///
/// let provider = ConstProvider { dim: 4 };
/// let vectors = provider.generate_embeddings(&["hello".to_string()]).unwrap();
/// assert_eq!(vectors.len(), 1);
/// assert_eq!(vectors[0].len(), 4);
/// assert_eq!(provider.name(), "const");
/// ```
pub trait EmbeddingProvider: Send + Sync {
    /// Generates one embedding vector per input text, in input order.
    ///
    /// Every returned vector has the length reported by [`vector_dim`][Self::vector_dim].
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Dimensionality of the vectors this provider produces.
    fn vector_dim(&self) -> usize;

    /// Human-readable provider name, for logging and diagnostics.
    fn name(&self) -> &'static str;
}
