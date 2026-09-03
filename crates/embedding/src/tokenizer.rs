//! Tokenizer wrapper over the Hugging Face `tokenizers` crate (design D2).
//!
//! bge-m3 ships a `tokenizer.json`; this module loads it and exposes exactly
//! what the ONNX provider (task 1.8) needs to build its inference inputs:
//! - [`Tokenizer::tokenize`] — token IDs plus the attention mask, truncated
//!   to [`DEFAULT_MAX_LENGTH`] (512, mirroring the oracle's
//!   `DefaultMaxLength`);
//! - [`Tokenizer::decode`] — token IDs back to text, special tokens skipped.
//!
//! Re-architected, not transcribed. Deliberate deviations from the oracle:
//! - the oracle's `Tokenize` padded output to `maxLength` (with a pad id that
//!   was never set for `tokenizer.json` files — it stayed 0 by accident), and
//!   its provider then filled `attention_mask` with all ones, so padded
//!   positions were fed to the model as real tokens. Padding is deliberately
//!   NOT done here: it is a batch-shaping concern of the provider, which pads
//!   to the batch's actual max length and extends the mask with zeros. This
//!   wrapper returns the exact token sequence, truncated to the model's
//!   maximum length.
//! - `encode` is called with `add_special_tokens = true` (the HF convention);
//!   the oracle passed `false`. For bge-m3 the post-processor adds no special
//!   tokens, so the two are behaviorally identical; `true` is the correct
//!   choice if a tokenizer.json ever declares a template that does.

use std::path::Path;

use tokenizers::Tokenizer as HfTokenizer;

use crate::error::EmbeddingError;

/// Default maximum sequence length, mirroring the oracle's `DefaultMaxLength`.
pub const DEFAULT_MAX_LENGTH: usize = 512;

/// Tokenization result: token IDs and the attention mask.
///
/// Both fields have the same length; `attention_mask[i]` is 1 for a real
/// token and 0 for padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokenized {
    /// Token IDs, at most [`DEFAULT_MAX_LENGTH`] entries.
    pub ids: Vec<u32>,
    /// Attention mask, same length as [`Tokenized::ids`].
    pub attention_mask: Vec<u32>,
}

/// A thin wrapper around `tokenizers::Tokenizer` loaded from a
/// `tokenizer.json` file (design D2).
///
/// [`Tokenizer::tokenize`] truncates to [`DEFAULT_MAX_LENGTH`] and never
/// pads (see the module docs for why padding belongs to the provider).
#[derive(Debug)]
pub struct Tokenizer {
    inner: HfTokenizer,
    max_length: usize,
}

impl Tokenizer {
    /// Loads a tokenizer from the `tokenizer.json` file at `path`.
    ///
    /// A probe encoding is run right after loading so that a file which
    /// parses but cannot encode fails at load time, not on the first
    /// [`tokenize`](Self::tokenize) call (mirrors the oracle's validation
    /// encode).
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Tokenizer`] if the file is missing or malformed, or
    /// if the probe encoding fails or produces no tokens.
    pub fn from_file(path: &Path) -> Result<Self, EmbeddingError> {
        let inner = HfTokenizer::from_file(path).map_err(|err| {
            EmbeddingError::Tokenizer(format!("failed to load {}: {err}", path.display()))
        })?;
        let probe = inner.encode("test", true).map_err(|err| {
            EmbeddingError::Tokenizer(format!(
                "tokenizer at {} is unusable: probe encode failed: {err}",
                path.display()
            ))
        })?;
        if probe.get_ids().is_empty() {
            return Err(EmbeddingError::Tokenizer(format!(
                "tokenizer at {} is unusable: probe encode produced no tokens",
                path.display()
            )));
        }
        Ok(Self {
            inner,
            max_length: DEFAULT_MAX_LENGTH,
        })
    }

    /// Tokenizes `text` into IDs and an attention mask.
    ///
    /// The result is truncated to [`DEFAULT_MAX_LENGTH`] tokens and is never
    /// padded (the provider shapes batches, see the module docs).
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Tokenizer`] if encoding fails.
    pub fn tokenize(&self, text: &str) -> Result<Tokenized, EmbeddingError> {
        let encoding = self
            .inner
            .encode(text, true)
            .map_err(|err| EmbeddingError::Tokenizer(format!("failed to encode text: {err}")))?;
        let ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let len = ids.len().min(self.max_length);
        Ok(Tokenized {
            ids: ids[..len].to_vec(),
            attention_mask: attention_mask[..len].to_vec(),
        })
    }

    /// Decodes token IDs back to text, skipping special tokens.
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Tokenizer`] if decoding fails.
    pub fn decode(&self, ids: &[u32]) -> Result<String, EmbeddingError> {
        self.inner.decode(ids, true).map_err(|err| {
            EmbeddingError::Tokenizer(format!("failed to decode {} token ids: {err}", ids.len()))
        })
    }

    /// Maximum sequence length applied by [`Tokenizer::tokenize`].
    #[must_use]
    pub fn max_length(&self) -> usize {
        self.max_length
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use super::*;

    /// The committed mini BPE fixture (task 1.6): a byte-level BPE model
    /// trained on a fixed corpus, `[PAD]`/`[UNK]` at ids 0/1 like the real
    /// bge-m3 tokenizer, no padding/truncation config, 2 KB on disk.
    fn fixture_path() -> &'static Path {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tokenizer.json"
        ))
    }

    fn load_fixture() -> Tokenizer {
        Tokenizer::from_file(fixture_path()).unwrap()
    }

    /// Loading the fixture succeeds and encodes to the golden ids from the
    /// fixture vocab ("hello" = 22, "Ġworld" = 26), with a matching all-ones
    /// attention mask.
    #[test]
    fn loads_fixture_and_encodes_known_text() {
        let tokenizer = load_fixture();
        let tokenized = tokenizer.tokenize("hello world").unwrap();
        assert_eq!(tokenized.ids, vec![22, 26]);
        assert_eq!(tokenized.attention_mask, vec![1, 1]);
    }

    /// Text just under the boundary is not truncated: 170 repetitions of
    /// "hello world " encode to 3 tokens each (510 total).
    #[test]
    fn tokenize_keeps_text_below_max_length_untruncated() {
        let tokenizer = load_fixture();
        let tokenized = tokenizer.tokenize(&"hello world ".repeat(170)).unwrap();
        assert_eq!(tokenized.ids.len(), 510);
        assert_eq!(tokenized.attention_mask.len(), 510);
        assert!(tokenized.attention_mask.iter().all(|&m| m == 1));
    }

    /// Text just over the boundary is truncated to exactly `max_length`:
    /// 171 repetitions encode to 513 tokens, 512 survive.
    #[test]
    fn tokenize_truncates_at_max_length_boundary() {
        let tokenizer = load_fixture();
        assert_eq!(tokenizer.max_length(), DEFAULT_MAX_LENGTH);
        let tokenized = tokenizer.tokenize(&"hello world ".repeat(171)).unwrap();
        assert_eq!(tokenized.ids.len(), DEFAULT_MAX_LENGTH);
        assert_eq!(tokenized.attention_mask.len(), DEFAULT_MAX_LENGTH);
    }

    /// decode(tokenize(text)) reproduces the original text for fixture
    /// vocabulary (byte-level BPE round-trips exactly).
    #[test]
    fn decode_round_trips_fixture_text() {
        let tokenizer = load_fixture();
        let tokenized = tokenizer.tokenize("hello world").unwrap();
        let decoded = tokenizer.decode(&tokenized.ids).unwrap();
        assert_eq!(decoded, "hello world");
    }

    /// A missing file yields `EmbeddingError::Tokenizer` naming the path.
    #[test]
    fn missing_file_returns_tokenizer_error() {
        let path = std::env::temp_dir().join("embedding-tok-missing-tokenizer.json");
        let err = Tokenizer::from_file(&path).unwrap_err();
        match err {
            EmbeddingError::Tokenizer(msg) => assert!(
                msg.contains("missing-tokenizer.json"),
                "message should name the failed path, got: {msg}"
            ),
            other => panic!("expected Tokenizer, got: {other:?}"),
        }
    }

    /// Per-test-file counter so parallel test binaries never collide on the
    /// temp file name.
    static MALFORMED_FILE_COUNTER: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A file that is not a valid tokenizer.json yields
    /// `EmbeddingError::Tokenizer`.
    #[test]
    fn malformed_file_returns_tokenizer_error() {
        let n = MALFORMED_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "embedding-tok-malformed-{}-{n}.json",
            std::process::id()
        ));
        std::fs::write(&path, "not a tokenizer.json").unwrap();
        let err = Tokenizer::from_file(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        match err {
            EmbeddingError::Tokenizer(msg) => assert!(
                msg.contains("malformed"),
                "message should name the failed path, got: {msg}"
            ),
            other => panic!("expected Tokenizer, got: {other:?}"),
        }
    }
}
