//! ONNX embedding provider (design D4): batch inference, L2 normalization, cache.
//!
//! [`OnnxProvider`] implements [`EmbeddingProvider`] on top of an `ort`
//! [`Session`]: all texts of a call are tokenized, right-padded to the batch's
//! actual max length, and fed to the model in ONE ONNX run (improvement over
//! the oracle, which ran batch=1 sequentially per text). Every resulting
//! vector is L2-normalized before it is cached and returned.
//!
//! Re-architected, not transcribed. Deliberate deviations from the oracle:
//! - **CLS pooling, as the oracle actually does.** The oracle takes
//!   `data[:vectorDim]` of the output — the first (CLS) hidden state of
//!   `last_hidden_state`, or the pre-pooled `sentence_embedding` vector. This
//!   is NOT mean pooling; matching the oracle keeps parity with fixtures
//!   recorded from the Go binary. The attention mask is still a model input
//!   (it masks the padding this provider adds, which the oracle could not do
//!   correctly — see the module docs in `tokenizer.rs`).
//! - **A text that tokenizes to zero tokens is a clear error** instead of an
//!   empty tensor that fails deep inside the ONNX Runtime with an opaque
//!   message (the oracle has no guard).
//! - **No cancellation token** (design D9): the sync API has no
//!   `context.Context` equivalent.
//!
//! The session sits behind `Arc<Mutex<Session>>` because `Session::run` takes
//! `&mut self` (ONNX Runtime's `Run` is not thread-safe) — design D4.
//! Inference is synchronous; async callers dispatch onto `spawn_blocking`.

use std::sync::{Arc, Mutex};

use ort::session::Session;
use ort::value::Tensor;

use crate::EmbeddingProvider;
use crate::cache::EmbeddingCache;
use crate::error::EmbeddingError;
use crate::tokenizer::{Tokenized, Tokenizer};

/// L2-norm threshold below which normalization is skipped (oracle: `1e-9`).
const L2_EPS: f64 = 1e-9;

/// Padding token id: bge-m3's XLM-RoBERTa vocabulary reserves id 0 for `[PAD]`.
const PAD_ID: i64 = 0;

/// Input tensors a transformer embedding export may declare, canonical order.
/// `token_type_ids` is optional (absent in bge-m3 exports).
const KNOWN_INPUTS: &[&str] = &["input_ids", "attention_mask", "token_type_ids"];

/// Output tensor names in preference order: a pooled `sentence_embedding`
/// (bge-m3) wins over raw `last_hidden_state` (e.g. BGE-small).
const PREFERRED_OUTPUTS: &[&str] = &["sentence_embedding", "last_hidden_state"];

/// The ONNX embedding provider.
///
/// Construct via [`OnnxProvider::new`] with a ready session (see
/// `crate::runtime::build_session`) and a loaded tokenizer; the factory that
/// wires all of that from configuration lands in task 1.9.
///
/// `Send + Sync`: the session is mutex-protected (D4), the tokenizer and the
/// cache are thread-safe, so one provider can be shared behind `Arc`.
pub struct OnnxProvider {
    session: Arc<Mutex<Session>>,
    tokenizer: Tokenizer,
    cache: EmbeddingCache,
    model_name: String,
    vector_dim: usize,
    /// The model's declared inputs this provider binds, canonical order.
    input_names: Vec<&'static str>,
    /// The output tensor this provider reads (detected at construction).
    output_name: String,
}

impl OnnxProvider {
    /// Wraps a ready session into a provider, detecting the model's actual
    /// graph I/O (exports differ: bge-m3 has no `token_type_ids` input and a
    /// pooled `sentence_embedding` output).
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Model`] if the model lacks the required
    /// `input_ids`/`attention_mask` inputs or has no supported output tensor.
    pub fn new(
        session: Session,
        tokenizer: Tokenizer,
        cache: EmbeddingCache,
        model_name: String,
        vector_dim: usize,
    ) -> Result<Self, EmbeddingError> {
        let input_names = detect_input_names(&input_names_of(&session))?;
        let output_name = detect_output_name(&output_names_of(&session))?;
        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer,
            cache,
            model_name,
            vector_dim,
            input_names,
            output_name,
        })
    }

    /// Tokenizes `texts` (one per element), pads the batch to its actual max
    /// length, runs the model once, and returns one L2-normalized vector per
    /// text, in input order.
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Tokenizer`] for tokenization failures or a text that
    /// produced no tokens; [`EmbeddingError::Ort`] for tensor construction or
    /// session-run failures; [`EmbeddingError::Model`] for an output tensor
    /// narrower than [`vector_dim`][Self::vector_dim].
    fn infer_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let tokenized: Vec<_> = texts
            .iter()
            .map(|text| self.tokenizer.tokenize(text))
            .collect::<Result<_, _>>()?;
        if let Some(pos) = tokenized.iter().position(|t| t.ids.is_empty()) {
            return Err(EmbeddingError::Tokenizer(format!(
                "text at index {pos} tokenized to zero tokens; nothing to embed"
            )));
        }

        let batch = pad_batch(&tokenized);
        let shape: [i64; 2] = [batch.batch_size as i64, batch.seq_len as i64];
        let mut inputs: Vec<(String, Tensor<i64>)> = Vec::with_capacity(self.input_names.len());
        for name in &self.input_names {
            let data: Vec<i64> = match *name {
                "input_ids" => batch.input_ids.clone(),
                "attention_mask" => batch.attention_mask.clone(),
                "token_type_ids" => vec![0; batch.input_ids.len()],
                // detect_input_names only returns names from KNOWN_INPUTS.
                _ => unreachable!("unvalidated input name {name:?}"),
            };
            let tensor = Tensor::from_array((shape, data)).map_err(|err| {
                EmbeddingError::Ort(format!("failed to build {name} tensor: {err}"))
            })?;
            inputs.push((name.to_string(), tensor));
        }

        // Session::run takes &mut self (ONNX Runtime Run is not thread-safe);
        // a poisoned lock means another thread panicked mid-run, and the
        // session itself is still valid — recover it (same policy as the cache).
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outputs = session.run(inputs)?;
        let output = outputs.get(&self.output_name).ok_or_else(|| {
            EmbeddingError::Ort(format!(
                "model produced no output named {}",
                self.output_name
            ))
        })?;
        let (out_shape, data) = output.try_extract_tensor::<f32>()?;

        let mut vectors = Vec::with_capacity(tokenized.len());
        for row in 0..tokenized.len() {
            let mut vector = embedding_row(data, out_shape, row, self.vector_dim)?.to_vec();
            l2_normalize(&mut vector);
            vectors.push(vector);
        }
        Ok(vectors)
    }
}

impl EmbeddingProvider for OnnxProvider {
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        generate_batch(
            &self.cache,
            &self.model_name,
            self.vector_dim,
            texts,
            |batch| self.infer_batch(batch),
        )
    }

    fn vector_dim(&self) -> usize {
        self.vector_dim
    }

    fn name(&self) -> &'static str {
        "onnx"
    }
}

/// Batch orchestration shared by [`OnnxProvider::generate_embeddings`]:
/// per-text cache lookup, ONE inference call for the misses (in original
/// order), cache fill, and order-preserving merge.
///
/// Kept free of `ort` types so the caching behavior is unit-testable with a
/// mock `infer` closure (CI has no ONNX Runtime library).
///
/// # Errors
///
/// [`EmbeddingError::Config`] if `texts` is empty; the `infer` error
/// otherwise, plus [`EmbeddingError::Ort`] if `infer` returns a vector count
/// that does not match the batch.
fn generate_batch(
    cache: &EmbeddingCache,
    model_name: &str,
    vector_dim: usize,
    texts: &[String],
    infer: impl for<'a> FnOnce(&'a [String]) -> Result<Vec<Vec<f32>>, EmbeddingError>,
) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    if texts.is_empty() {
        return Err(EmbeddingError::Config(
            "texts must not be empty".to_string(),
        ));
    }

    let mut results: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    let mut to_infer: Vec<(usize, String)> = Vec::new();
    for (slot, text) in texts.iter().enumerate() {
        match cache.get(model_name, vector_dim, text) {
            Some(cached) => results.push(cached),
            None => {
                // Placeholder; replaced by the inference result below.
                results.push(Vec::new());
                to_infer.push((slot, text.clone()));
            }
        }
    }

    if !to_infer.is_empty() {
        let batch: Vec<String> = to_infer.iter().map(|(_, text)| text.clone()).collect();
        let inferred = infer(&batch)?;
        if inferred.len() != to_infer.len() {
            return Err(EmbeddingError::Ort(format!(
                "inference returned {} vectors for {} texts",
                inferred.len(),
                to_infer.len()
            )));
        }
        for ((slot, _), vector) in to_infer.into_iter().zip(inferred) {
            cache.set(model_name, vector_dim, &texts[slot], &vector);
            results[slot] = vector;
        }
    }

    Ok(results)
}

/// Selects the model's declared inputs to bind, in canonical order.
///
/// Fails when either required input (`input_ids`, `attention_mask`) is
/// missing. `token_type_ids` is bound only if the model declares it.
fn detect_input_names(names: &[&str]) -> Result<Vec<&'static str>, EmbeddingError> {
    let has = |name: &str| names.contains(&name);
    if !has("input_ids") || !has("attention_mask") {
        return Err(EmbeddingError::Model(format!(
            "model lacks required inputs input_ids and attention_mask (declared: {names:?})"
        )));
    }
    Ok(KNOWN_INPUTS
        .iter()
        .copied()
        .filter(|name| has(name))
        .collect())
}

/// Picks the embedding output tensor: a pooled `sentence_embedding` when
/// available, otherwise `last_hidden_state`, otherwise the single declared
/// output.
fn detect_output_name(names: &[&str]) -> Result<String, EmbeddingError> {
    for name in PREFERRED_OUTPUTS {
        if names.contains(name) {
            return Ok((*name).to_string());
        }
    }
    if names.len() == 1 {
        return Ok(names[0].to_string());
    }
    Err(EmbeddingError::Model(format!(
        "no supported embedding output found (declared: {names:?})"
    )))
}

/// Collects the session's declared input names.
fn input_names_of(session: &Session) -> Vec<&str> {
    session.inputs().iter().map(|input| input.name()).collect()
}

/// Collects the session's declared output names.
fn output_names_of(session: &Session) -> Vec<&str> {
    session
        .outputs()
        .iter()
        .map(|output| output.name())
        .collect()
}

/// A right-padded inference batch: row-major `[batch_size, seq_len]` data for
/// `input_ids` and `attention_mask`.
struct PaddedBatch {
    input_ids: Vec<i64>,
    attention_mask: Vec<i64>,
    batch_size: usize,
    seq_len: usize,
}

/// Right-pads `tokenized` to the batch's actual max length (the tokenizer
/// never pads — see `tokenizer.rs`): missing positions get [`PAD_ID`] and a
/// zero attention mask, so the model masks them out.
fn pad_batch(tokenized: &[Tokenized]) -> PaddedBatch {
    let batch_size = tokenized.len();
    let seq_len = tokenized.iter().map(|t| t.ids.len()).max().unwrap_or(0);
    let mut input_ids = Vec::with_capacity(batch_size * seq_len);
    let mut attention_mask = Vec::with_capacity(batch_size * seq_len);
    for tokens in tokenized {
        input_ids.extend(tokens.ids.iter().map(|id| i64::from(*id)));
        attention_mask.extend(tokens.attention_mask.iter().map(|m| i64::from(*m != 0)));
        let pad = seq_len - tokens.ids.len();
        input_ids.resize(input_ids.len() + pad, PAD_ID);
        attention_mask.resize(attention_mask.len() + pad, 0);
    }
    PaddedBatch {
        input_ids,
        attention_mask,
        batch_size,
        seq_len,
    }
}

/// One embedding from a model output, by row.
///
/// `shape` is `[batch, dim]` for a pooled `sentence_embedding` (the whole row
/// is the vector) or `[batch, seq_len, dim]` for `last_hidden_state` (the CLS
/// — first — token of the row, the oracle's behavior).
///
/// # Errors
///
/// [`EmbeddingError::Ort`] for negative dimensions, an unsupported rank, or a
/// row outside the batch; [`EmbeddingError::Model`] if the width is smaller
/// than `dim`.
fn embedding_row<'a>(
    data: &'a [f32],
    shape: &[i64],
    row: usize,
    dim: usize,
) -> Result<&'a [f32], EmbeddingError> {
    let dim_of = |axis: usize, what: &str| -> Result<usize, EmbeddingError> {
        usize::try_from(shape[axis]).map_err(|_| {
            EmbeddingError::Ort(format!(
                "output {what} dimension is negative: {}",
                shape[axis]
            ))
        })
    };
    match shape.len() {
        2 => {
            let batch = dim_of(0, "batch")?;
            let width = dim_of(1, "width")?;
            check_row_and_width(row, batch, width, dim)?;
            let start = row * width;
            Ok(&data[start..start + dim])
        }
        3 => {
            let batch = dim_of(0, "batch")?;
            let seq_len = dim_of(1, "seq_len")?;
            let width = dim_of(2, "width")?;
            check_row_and_width(row, batch, width, dim)?;
            // CLS: the first token of the row.
            let start = row * seq_len * width;
            Ok(&data[start..start + dim])
        }
        rank => Err(EmbeddingError::Ort(format!(
            "unsupported output rank {rank} (expected 2 or 3)"
        ))),
    }
}

fn check_row_and_width(
    row: usize,
    batch: usize,
    width: usize,
    dim: usize,
) -> Result<(), EmbeddingError> {
    if row >= batch {
        return Err(EmbeddingError::Ort(format!(
            "output batch is {batch} but row {row} was requested"
        )));
    }
    if width < dim {
        return Err(EmbeddingError::Model(format!(
            "output width {width} is smaller than the expected vector dim {dim}"
        )));
    }
    Ok(())
}

/// L2-normalizes `vector` in place; a (near-)zero vector is left untouched.
///
/// The norm accumulates in `f64` and the division happens in `f32`,
/// bit-for-bit the oracle's `L2` normalization.
fn l2_normalize(vector: &mut [f32]) {
    let norm: f64 = vector.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
    let norm = norm.sqrt();
    if norm > L2_EPS {
        let scale = norm as f32;
        for v in vector.iter_mut() {
            *v /= scale;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    /// A mock inference: one vector per text, `i`-th vector is all `(i+1)`.
    fn mock_infer(dim: usize) -> impl Fn(&[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        move |texts: &[String]| {
            Ok((0..texts.len())
                .map(|i| vec![(i + 1) as f32; dim])
                .collect())
        }
    }

    fn tokenized(ids: &[u32]) -> Tokenized {
        Tokenized {
            ids: ids.to_vec(),
            attention_mask: vec![1; ids.len()],
        }
    }

    /// Function items (unlike call-site closures) are HRTB over the input
    /// lifetime, so they satisfy `for<'a> FnOnce(&'a [String])`.
    fn never_infer(_texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::Ort("infer must not be called".to_string()))
    }

    fn failing_infer(_texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::Ort("boom".to_string()))
    }

    fn short_infer(_texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![vec![1.0]])
    }

    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OnnxProvider>();
    }

    // --- generate_batch (cache orchestration, mock inference) ---

    #[test]
    fn generate_batch_empty_texts_returns_config_error() {
        let cache = EmbeddingCache::new();
        let err = generate_batch(&cache, "m", 4, &[], mock_infer(4)).unwrap_err();
        assert!(matches!(err, EmbeddingError::Config(_)));
    }

    #[test]
    fn generate_batch_all_cached_skips_inference() {
        let cache = EmbeddingCache::new();
        cache.set("m", 4, "a", &[1.0, 2.0, 3.0, 4.0]);
        cache.set("m", 4, "b", &[5.0, 6.0, 7.0, 8.0]);
        let texts = vec!["a".to_string(), "b".to_string()];
        let results = generate_batch(&cache, "m", 4, &texts, never_infer).unwrap();
        assert_eq!(
            results,
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]
        );
    }

    #[test]
    fn generate_batch_partial_cache_preserves_order_and_caches_results() {
        let cache = EmbeddingCache::new();
        cache.set("m", 4, "b", &[9.0, 9.0, 9.0, 9.0]);
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let results = generate_batch(&cache, "m", 4, &texts, mock_infer(4)).unwrap();

        // Order preserved: a -> [1,..] (1st in the infer batch), cached
        // b -> [9,..], c -> [2,..] (2nd in the infer batch).
        assert_eq!(results[0], vec![1.0; 4]);
        assert_eq!(results[1], vec![9.0; 4]);
        assert_eq!(results[2], vec![2.0; 4]);
        // The misses were cached under their original texts.
        assert_eq!(cache.get("m", 4, "a"), Some(vec![1.0; 4]));
        assert_eq!(cache.get("m", 4, "c"), Some(vec![2.0; 4]));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn generate_batch_infer_error_propagates() {
        let cache = EmbeddingCache::new();
        let texts = vec!["a".to_string()];
        let err = generate_batch(&cache, "m", 4, &texts, failing_infer).unwrap_err();
        assert!(matches!(err, EmbeddingError::Ort(msg) if msg == "boom"));
    }

    #[test]
    fn generate_batch_infer_count_mismatch_errors() {
        let cache = EmbeddingCache::new();
        let texts = vec!["a".to_string(), "b".to_string()];
        let err = generate_batch(&cache, "m", 4, &texts, short_infer).unwrap_err();
        assert!(matches!(err, EmbeddingError::Ort(_)));
    }

    // --- pad_batch ---

    #[test]
    fn pad_batch_pads_to_max_length_with_pad_id_and_zero_mask() {
        let batch = pad_batch(&[tokenized(&[2, 3]), tokenized(&[9, 8, 7, 6])]);
        assert_eq!(batch.batch_size, 2);
        assert_eq!(batch.seq_len, 4);
        assert_eq!(batch.input_ids, vec![2, 3, PAD_ID, PAD_ID, 9, 8, 7, 6]);
        assert_eq!(batch.attention_mask, vec![1, 1, 0, 0, 1, 1, 1, 1]);
    }

    #[test]
    fn pad_batch_single_sequence_is_unpadded() {
        let batch = pad_batch(&[tokenized(&[5, 6, 7])]);
        assert_eq!(batch.input_ids, vec![5, 6, 7]);
        assert_eq!(batch.attention_mask, vec![1, 1, 1]);
        assert_eq!(batch.seq_len, 3);
    }

    #[test]
    fn pad_batch_equal_lengths_never_pads() {
        let batch = pad_batch(&[tokenized(&[1, 2]), tokenized(&[3, 4])]);
        assert_eq!(batch.input_ids, vec![1, 2, 3, 4]);
        assert_eq!(batch.attention_mask, vec![1, 1, 1, 1]);
    }

    // --- embedding_row ---

    #[test]
    fn embedding_row_pooled_output_takes_the_whole_row() {
        let data: Vec<f32> = (0..8).map(|i| i as f32).collect();
        assert_eq!(
            embedding_row(&data, &[2, 4], 0, 4).unwrap(),
            &[0.0, 1.0, 2.0, 3.0]
        );
        assert_eq!(
            embedding_row(&data, &[2, 4], 1, 4).unwrap(),
            &[4.0, 5.0, 6.0, 7.0]
        );
    }

    #[test]
    fn embedding_row_hidden_states_take_the_cls_token() {
        // [1, 3, 4]: the CLS of row 0 is the FIRST token, not the last.
        let data: Vec<f32> = (0..12).map(|i| i as f32).collect();
        assert_eq!(
            embedding_row(&data, &[1, 3, 4], 0, 4).unwrap(),
            &[0.0, 1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn embedding_row_width_smaller_than_dim_errors() {
        let data = vec![0.0f32; 8];
        let err = embedding_row(&data, &[2, 4], 0, 8).unwrap_err();
        assert!(matches!(err, EmbeddingError::Model(_)));
    }

    #[test]
    fn embedding_row_row_out_of_batch_errors() {
        let data = vec![0.0f32; 8];
        let err = embedding_row(&data, &[2, 4], 2, 4).unwrap_err();
        assert!(matches!(err, EmbeddingError::Ort(_)));
    }

    #[test]
    fn embedding_row_unsupported_rank_errors() {
        let data = vec![0.0f32; 8];
        assert!(matches!(
            embedding_row(&data, &[2, 2, 2, 2], 0, 2),
            Err(EmbeddingError::Ort(_))
        ));
    }

    // --- l2_normalize (golden vectors) ---

    #[test]
    fn l2_normalize_golden_3_4_vector() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-7);
        assert!((v[1] - 0.8).abs() < 1e-7);
    }

    #[test]
    fn l2_normalize_zero_vector_is_unchanged() {
        let mut v = vec![0.0f32; 4];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0f32; 4]);
    }

    #[test]
    fn l2_normalize_sub_epsilon_vector_is_unchanged() {
        let mut v = vec![1e-12f32, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![1e-12f32, 0.0]);
    }

    #[test]
    fn l2_normalize_keeps_sign_of_negative_values() {
        let mut v = vec![-3.0f32, -4.0];
        l2_normalize(&mut v);
        assert!((v[0] + 0.6).abs() < 1e-7);
        assert!((v[1] + 0.8).abs() < 1e-7);
    }

    // --- input/output detection ---

    #[test]
    fn detect_input_names_bge_m3_style() {
        assert_eq!(
            detect_input_names(&["input_ids", "attention_mask"]).unwrap(),
            vec!["input_ids", "attention_mask"]
        );
    }

    #[test]
    fn detect_input_names_keeps_canonical_order_including_token_type_ids() {
        let names = ["token_type_ids", "attention_mask", "input_ids"];
        assert_eq!(
            detect_input_names(&names).unwrap(),
            vec!["input_ids", "attention_mask", "token_type_ids"]
        );
    }

    #[test]
    fn detect_input_names_missing_required_errors() {
        assert!(matches!(
            detect_input_names(&["attention_mask"]),
            Err(EmbeddingError::Model(_))
        ));
        assert!(matches!(
            detect_input_names(&[]),
            Err(EmbeddingError::Model(_))
        ));
    }

    #[test]
    fn detect_output_name_prefers_sentence_embedding() {
        assert_eq!(
            detect_output_name(&["last_hidden_state", "sentence_embedding"]).unwrap(),
            "sentence_embedding"
        );
    }

    #[test]
    fn detect_output_name_falls_back_to_last_hidden_state() {
        assert_eq!(
            detect_output_name(&["last_hidden_state"]).unwrap(),
            "last_hidden_state"
        );
    }

    #[test]
    fn detect_output_name_accepts_a_single_unknown_output() {
        assert_eq!(
            detect_output_name(&["pooler_output"]).unwrap(),
            "pooler_output"
        );
    }

    #[test]
    fn detect_output_name_rejects_multiple_unknown_outputs() {
        assert!(matches!(
            detect_output_name(&["a", "b"]),
            Err(EmbeddingError::Model(_))
        ));
    }

    /// End-to-end with the real ONNX Runtime library and model.
    ///
    /// Run manually:
    /// `EMBEDDING_TEST_ONNXRUNTIME_LIB=/path/libonnxruntime.so \
    ///  EMBEDDING_TEST_MODEL=/path/model.onnx \
    ///  EMBEDDING_TEST_TOKENIZER=/path/tokenizer.json \
    ///  EMBEDDING_TEST_DIM=1024 cargo test -p embedding -- --ignored`
    #[test]
    #[ignore]
    fn real_inference_produces_normalized_vectors() {
        let lib = std::env::var("EMBEDDING_TEST_ONNXRUNTIME_LIB")
            .expect("set EMBEDDING_TEST_ONNXRUNTIME_LIB to a real onnxruntime .so/.dylib");
        let model = std::env::var("EMBEDDING_TEST_MODEL").expect("set EMBEDDING_TEST_MODEL");
        let tokenizer_path =
            std::env::var("EMBEDDING_TEST_TOKENIZER").expect("set EMBEDDING_TEST_TOKENIZER");
        let dim: usize = std::env::var("EMBEDDING_TEST_DIM")
            .unwrap_or_else(|_| "1024".to_string())
            .parse()
            .unwrap();

        crate::runtime::init_runtime(std::path::Path::new(&lib)).unwrap();
        let session = crate::runtime::build_session(std::path::Path::new(&model)).unwrap();
        let tokenizer = Tokenizer::from_file(std::path::Path::new(&tokenizer_path)).unwrap();
        let provider = OnnxProvider::new(
            session,
            tokenizer,
            EmbeddingCache::new(),
            "test".into(),
            dim,
        )
        .unwrap();

        let texts = vec!["hello world".to_string(), "second text".to_string()];
        let vectors = provider.generate_embeddings(&texts).unwrap();
        assert_eq!(vectors.len(), 2);
        for vector in &vectors {
            assert_eq!(vector.len(), dim);
            let norm: f64 = vector
                .iter()
                .map(|v| f64::from(*v) * f64::from(*v))
                .sum::<f64>()
                .sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "expected unit norm, got {norm}");
        }
        // A repeat call is served from the cache (same values, no error).
        let again = provider.generate_embeddings(&texts).unwrap();
        assert_eq!(again, vectors);
    }
}
