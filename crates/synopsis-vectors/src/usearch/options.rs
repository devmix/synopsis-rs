//! Index option mapping and small shared helpers (usearch-wal-persistence
//! task 3.4 split): geometry → `IndexOptions`, quantization mapping,
//! chunk-id key conversion, and the usearch/SQLite error mappers.

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::{VectorIndexConfig, VectorsError};

/// Index options for the configured geometry: `L2sq` metric, the configured
/// scalar quantization (default `BF16`; see
/// [`VectorIndexConfig::quantization`]), HNSW `connectivity = m`,
/// `expansion_add = ef_construction`, `expansion_search = ef_search` (design.md
/// parity parameters; the IVF fields of [`VectorIndexConfig`] do not apply to
/// pure HNSW). `multi` is off: one vector per chunk id, so search results
/// never contain duplicate keys.
pub(super) fn options(config: &VectorIndexConfig) -> IndexOptions {
    IndexOptions {
        dimensions: config.dim,
        metric: MetricKind::L2sq,
        quantization: quantization(config.quantization.as_deref()),
        connectivity: config.m,
        expansion_add: config.ef_construction,
        expansion_search: config.ef_search,
        multi: false,
    }
}

/// Maps the configured quantization string to a usearch [`ScalarKind`].
/// `None` (absent) and `"bf16"` resolve to `BF16` (the engine default); an
/// unrecognized value falls back to `BF16` too — the config crate rejects
/// unknown values at parse time, so the fallback is a defense-in-depth guard.
fn quantization(kind: Option<&str>) -> ScalarKind {
    match kind.map(|k| k.to_ascii_lowercase()).as_deref() {
        Some("u8") => ScalarKind::U8,
        Some("i8") => ScalarKind::I8,
        Some("f16") => ScalarKind::F16,
        Some("f32") => ScalarKind::F32,
        // "bf16", absent (None), and unrecognized values → the BF16 default.
        _ => ScalarKind::BF16,
    }
}

/// The trait keys are `u32` chunk ids; usearch keys are `u64`. Every key in
/// this engine was inserted as a `u32`, so the conversion cannot fail in
/// practice — a failure would mean a corrupted index.
pub(super) fn key_to_chunk_id(key: u64) -> Result<u32, VectorsError> {
    u32::try_from(key).map_err(|_| {
        VectorsError::Engine(format!("usearch key {key} exceeds the u32 chunk-id range"))
    })
}

/// Maps a usearch cxx FFI exception to [`VectorsError::Engine`].
pub(super) fn map_usearch(err: cxx::Exception) -> VectorsError {
    VectorsError::Engine(format!("usearch: {err}"))
}

/// Maps a rusqlite failure (WAL table access) to [`VectorsError::Engine`].
pub(super) fn map_sqlite(err: rusqlite::Error) -> VectorsError {
    VectorsError::Engine(format!("usearch WAL: {err}"))
}

/// A fresh empty RAM-layer index (the create-state of ADR 0004 §8 step 3).
pub(super) fn empty_index(config: &VectorIndexConfig) -> Result<Index, VectorsError> {
    let index = Index::new(&options(config)).map_err(map_usearch)?;
    // The 2.26 core rejects a search that finds no reserved worker thread;
    // reserving 1 slot keeps an empty index searchable and insertable.
    index.reserve(1).map_err(map_usearch)?;
    Ok(index)
}
