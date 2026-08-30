# Tasks: usearch-wal-persistence

- [x] 2.1 WAL table in SQLite + config struct (usearch_vectors_log table, UsearchConfig)
- [x] 2.2 WAL write path (insert/delete/update → SQLite WAL + RAM index)
- [x] 2.3 Parallel search via rayon with filtered_search
- [ ] 2.4 Global compaction (merge all segments, remove stale, slice by 1M)
- [ ] 2.5 Integration tests (persistence, compaction, config validation)

---

## 2.1 WAL table in SQLite + config struct

**Goal:** Create WAL table in SQLite and add UsearchConfig.

**Scope файлов:**
- `migrations/knowledge/3-usearch-vectors-log/up.sql` — WAL table with segment_id
- `crates/config/src/preset.rs` — UsearchConfig struct
- `crates/vectors/src/lib.rs` — VectorIndexConfig with usearch field

**Status:** ✅ Complete

---

## 2.2 WAL write path

**Goal:** Insert/delete/update operations write to SQLite WAL + update RAM index.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — WAL methods (write_wal, load_stale_ids, clear_wal)

**Status:** ✅ Complete

---

## 2.3 Parallel search via rayon with filtered_search

**Goal:** Search across multiple DISK segments in parallel with WAL filtering.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — search method with rayon + filtered_search

**Status:** ✅ Complete

---

## 2.4 Global compaction (merge all segments)

**Goal:** Merge all DISK segments, remove stale vectors, slice by 1M.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — multi-segment architecture + compaction

**Dependencies:** 2.1, 2.2, 2.3

**Key changes:**

1. **Multi-segment architecture:**
   ```rust
   pub struct UsearchEngine {
       index: Index,           // RAM layer (mutable, segment_id=0)
       disk_segments: Vec<Index>,  // DISK layers (read-only mmap)
       config: VectorIndexConfig,
       path: PathBuf,
       usearch_config: UsearchConfig,
       wal_db: Option<Mutex<Connection>>,
   }
   ```

2. **Per-segment WAL filtering:**
   ```rust
   fn load_stale_ids_for_segment(&self, segment_id: u32) -> Result<HashSet<u32>, VectorsError> {
       // SELECT chunk_id FROM usearch_vectors_log 
       // WHERE segment_id <= ? AND flags & 6 != 0
       // (cumulative: includes all WAL entries from segment 0..N)
   }
   ```

3. **Search with per-segment filtering:**
   ```rust
   fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
       use rayon::prelude::*;
       
       // Search DISK segments with cumulative WAL
       let disk_results: Vec<Vec<(u32, f32)>> = self.disk_segments.par_iter().enumerate()
           .map(|(i, seg)| {
               let stale = self.load_stale_ids_for_segment(i as u32)?;
               let matches = seg.filtered_search(query, k, |key| !stale.contains(&(key as u32)))?;
               // ... convert matches to results
           })
           .collect::<Result<Vec<_>, _>>()?;
       
       // Search RAM layer with segment_id=0 WAL
       let ram_stale = self.load_stale_ids_for_segment(0)?;
       let ram_results = self.index.filtered_search(query, k, |key| !ram_stale.contains(&(key as u32)))?;
       
       // Merge all results
       merge_results(disk_results, ram_results, k)
   }
   ```

4. **Compaction:**
   ```rust
   fn compact(&mut self) -> Result<(), VectorsError> {
       // 1. Load cumulative stale IDs across all segments
       let stale = self.load_all_stale_ids()?;
       
       // 2. Collect live vectors from all segments
       let mut live_vectors = Vec::new();
       for seg in &self.disk_segments {
           // Export all vectors, filter out stale
       }
       
       // 3. Create new segments (sliced by max_segment_vectors)
       let max = self.usearch_config.max_segment_vectors;
       let mut new_segments = Vec::new();
       for chunk in live_vectors.chunks(max) {
           let mut new_index = Index::new(...)?;
           new_index.reserve(chunk.len())?;
           for (id, vector) in chunk {
               new_index.add(*id, vector)?;
           }
           new_segments.push(new_index);
       }
       
       // 4. Replace old segments
       self.disk_segments = new_segments;
       
       // 5. Clear WAL
       self.clear_wal()?;
       
       Ok(())
   }
   ```

5. **Tests:**
   - `test_stale_vector_percentage` — verify correct percentage
   - `test_compaction_triggers_on_threshold` — verify compaction triggers
   - `test_compaction_creates_correct_segments` — verify segment count
   - `test_compaction_clears_wal` — verify WAL cleared
   - `test_search_after_compaction` — verify search unchanged

**Gates:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p vectors --features engine-usearch`

---

## 2.5 Integration tests

**Goal:** Test persistence, compaction, and config validation.

**Scope файлов:**
- `crates/vectors/tests/wal_integration.rs` (new test file)

**Dependencies:** 2.2, 2.3, 2.4

**Tests:**
- `test_wal_persistence_across_restart` — insert, restart, verify data persists
- `test_compaction_reduces_segments` — insert enough to trigger compaction, verify segment count
- `test_search_correctness_with_wal` — verify search results with WAL filtering
- `test_config_validation` — invalid WAL config values rejected
- `test_parallel_search` — verify rayon parallel search works
