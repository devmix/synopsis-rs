//! In-memory embedding cache (design D3).
//!
//! Stores previously computed embeddings so that repeated texts (e.g. the
//! same passage re-ingested) skip ONNX inference. Entries are keyed by
//! [`cache_key`] — the hex sha256 of `"{model}|{dim}|{text}"`, byte-identical
//! to the oracle's `CacheKey` — so the same text never collides across models
//! or dimensions.
//!
//! Re-architected from the oracle, not transcribed. Deliberate deviations:
//! - the oracle's DB write-through store (`NewEmbeddingCacheWithStore`) is
//!   NOT ported: persistence is a non-goal (design D3, YAGNI — a persistent
//!   cache would come later via the db crate if ever needed);
//! - the oracle cleared the whole cache on ANY `Set` at capacity, including
//!   an overwrite of an already-cached entry. Overwriting does not grow the
//!   map, so it does not evict here (see [`EmbeddingCache::set`]).
//!
//! Eviction at capacity is a full clear (oracle behavior, design D3): cheap
//! and good enough for the laptop-scale, single-pass ingestion workload; an
//! LRU would add bookkeeping without a measurable win.

use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use sha2::{Digest, Sha256};

/// Default maximum number of cached embeddings, mirroring the oracle's
/// `defaultCacheMaxSize` (`cache.go`).
pub const DEFAULT_MAX_SIZE: usize = 10_000;

/// Computes the deterministic cache key: the hex sha256 of
/// `"{model}|{dim}|{text}"`.
///
/// The format matches the oracle's `CacheKey` exactly
/// (`fmt.Sprintf("%s|%d|%s", modelName, dim, text)`), so keys produced by the
/// Go binary and by this function are byte-identical.
#[must_use]
pub fn cache_key(model: &str, dim: usize, text: &str) -> String {
    let digest = Sha256::digest(format!("{model}|{dim}|{text}"));
    to_hex(&digest)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A thread-safe in-memory cache of computed embeddings (design D3).
///
/// The cache is a pure performance optimization: no operation can fail, and
/// if another thread ever panics while holding the lock, the lock is
/// recovered rather than surfaced as an error — Rust's memory safety keeps
/// the underlying map valid, and a recomputation is an acceptable
/// degradation.
///
/// # Eviction
///
/// When the number of entries reaches the size limit, inserting a NEW entry
/// first clears the whole cache (the oracle's behavior); after such an
/// eviction the new entry is the only one present. Overwriting an existing
/// entry never evicts (see the module docs).
#[derive(Debug)]
pub struct EmbeddingCache {
    inner: RwLock<HashMap<String, Vec<f32>>>,
    max_size: usize,
}

impl EmbeddingCache {
    /// Creates an empty cache limited to [`DEFAULT_MAX_SIZE`] entries.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_size(DEFAULT_MAX_SIZE)
    }

    /// Creates an empty cache with an explicit size limit.
    ///
    /// `max_size` is the maximum number of entries; `0` means unlimited
    /// (mirroring the oracle's `maxSize` semantics).
    #[must_use]
    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            max_size,
        }
    }

    /// Returns a copy of the cached embedding for `(model, dim, text)`, or
    /// `None` on a miss.
    #[must_use]
    pub fn get(&self, model: &str, dim: usize, text: &str) -> Option<Vec<f32>> {
        let key = cache_key(model, dim, text);
        self.read().get(&key).cloned()
    }

    /// Stores `vec` under `(model, dim, text)`, replacing any existing entry
    /// for the same triple.
    ///
    /// If the cache is at its size limit and the key is not already present,
    /// the whole cache is cleared first (oracle behavior, design D3);
    /// overwriting an existing entry does not evict (see the module docs).
    pub fn set(&self, model: &str, dim: usize, text: &str, vec: &[f32]) {
        let key = cache_key(model, dim, text);
        let mut map = self.write();
        if self.max_size > 0 && map.len() >= self.max_size && !map.contains_key(&key) {
            map.clear();
        }
        map.insert(key, vec.to_vec());
    }

    /// Number of cached embeddings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The configured size limit; `0` means unlimited.
    #[must_use]
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Reads the map, recovering the guard if the lock was poisoned by a
    /// panic in another thread (see the struct docs for why that is safe).
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, Vec<f32>>> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Writes the map, recovering the guard if the lock was poisoned by a
    /// panic in another thread (see the struct docs for why that is safe).
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, Vec<f32>>> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for EmbeddingCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// A set/get round-trip returns an equal copy; a miss returns `None`.
    #[test]
    fn set_get_round_trip() {
        let cache = EmbeddingCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.get("bge-m3", 1024, "hello"), None);

        let vec = vec![0.1, 0.2, 0.3, 0.4];
        cache.set("bge-m3", 1024, "hello", &vec);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("bge-m3", 1024, "hello"), Some(vec));
    }

    /// The returned vector is a copy: mutating it does not affect the cache.
    #[test]
    fn get_returns_a_copy() {
        let cache = EmbeddingCache::new();
        cache.set("m", 4, "t", &[1.0, 2.0]);
        let mut got = cache.get("m", 4, "t").unwrap();
        got[0] = 99.0;
        assert_eq!(cache.get("m", 4, "t"), Some(vec![1.0, 2.0]));
    }

    /// Overwriting an existing entry replaces the value in place.
    #[test]
    fn set_replaces_existing_entry() {
        let cache = EmbeddingCache::new();
        cache.set("m", 4, "t", &[1.0]);
        cache.set("m", 4, "t", &[2.0, 3.0]);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("m", 4, "t"), Some(vec![2.0, 3.0]));
    }

    /// The key depends on all three components: different models, dimensions,
    /// and texts never collide.
    #[test]
    fn key_depends_on_model_dim_and_text() {
        let cache = EmbeddingCache::new();
        cache.set("model-a", 1024, "text", &[1.0]);
        cache.set("model-b", 1024, "text", &[2.0]);
        cache.set("model-a", 2048, "text", &[3.0]);
        cache.set("model-a", 1024, "other text", &[4.0]);
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get("model-a", 1024, "text"), Some(vec![1.0]));
        assert_eq!(cache.get("model-b", 1024, "text"), Some(vec![2.0]));
        assert_eq!(cache.get("model-a", 2048, "text"), Some(vec![3.0]));
        assert_eq!(cache.get("model-a", 1024, "other text"), Some(vec![4.0]));
    }

    /// The key format is byte-identical to the oracle's `CacheKey`: golden
    /// digest of "bge-m3|1024|hello" computed with `sha256sum`.
    #[test]
    fn cache_key_matches_oracle_format() {
        assert_eq!(
            cache_key("bge-m3", 1024, "hello"),
            "10ea4576a1e88eb7760edafc7b8ac1f8119857d16332e81ac631d8fea1a0dc54"
        );
    }

    /// At the size limit, inserting a NEW entry clears the whole cache
    /// (oracle behavior); the new entry is the only one left, and the limit
    /// applies again from the fresh state.
    #[test]
    fn eviction_clears_cache_at_max_size() {
        let cache = EmbeddingCache::with_max_size(3);
        cache.set("m", 1, "a", &[1.0]);
        cache.set("m", 1, "b", &[2.0]);
        cache.set("m", 1, "c", &[3.0]);
        assert_eq!(cache.len(), 3);

        cache.set("m", 1, "d", &[4.0]);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("m", 1, "d"), Some(vec![4.0]));
        assert_eq!(cache.get("m", 1, "a"), None);
        assert_eq!(cache.get("m", 1, "b"), None);
        assert_eq!(cache.get("m", 1, "c"), None);

        cache.set("m", 1, "e", &[5.0]);
        cache.set("m", 1, "f", &[6.0]);
        cache.set("m", 1, "g", &[7.0]);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("m", 1, "g"), Some(vec![7.0]));
    }

    /// Overwriting an existing entry at the size limit does NOT evict
    /// (conscious deviation from the oracle — see the module docs).
    #[test]
    fn overwrite_at_max_size_does_not_evict() {
        let cache = EmbeddingCache::with_max_size(2);
        cache.set("m", 1, "a", &[1.0]);
        cache.set("m", 1, "b", &[2.0]);
        cache.set("m", 1, "a", &[9.0]);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("m", 1, "a"), Some(vec![9.0]));
        assert_eq!(cache.get("m", 1, "b"), Some(vec![2.0]));
    }

    /// `max_size = 0` means unlimited (oracle `maxSize` semantics).
    #[test]
    fn zero_max_size_is_unlimited() {
        let cache = EmbeddingCache::with_max_size(0);
        for i in 0..(2 * DEFAULT_MAX_SIZE) {
            cache.set("m", 1, &format!("text-{i}"), &[1.0]);
        }
        assert_eq!(cache.len(), 2 * DEFAULT_MAX_SIZE);
    }

    /// `new()` and `Default` use the documented default limit.
    #[test]
    fn new_uses_default_max_size() {
        assert_eq!(EmbeddingCache::new().max_size(), DEFAULT_MAX_SIZE);
        assert_eq!(EmbeddingCache::default().max_size(), DEFAULT_MAX_SIZE);
    }

    /// Concurrent readers and writers on one shared cache: every thread's
    /// entries survive (800 total, far below the default limit), and each
    /// thread observes its own writes immediately.
    #[test]
    fn concurrent_access_is_safe() {
        let cache = Arc::new(EmbeddingCache::new());
        let handles: Vec<_> = (0u8..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0u8..100 {
                        let text = format!("thread-{t}-text-{i}");
                        let vec = vec![f32::from(i), f32::from(t)];
                        cache.set("m", 1024, &text, &vec);
                        let got = cache.get("m", 1024, &text);
                        assert_eq!(got.as_deref(), Some(vec.as_slice()));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(cache.len(), 800);
    }

    /// The cache is shareable across threads (required by task 1.8's
    /// `OnnxProvider`, which lives behind `Arc`).
    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EmbeddingCache>();
    }
}
