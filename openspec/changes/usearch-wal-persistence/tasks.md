# Tasks: usearch-wal-persistence

- [x] 2.1 WAL table in SQLite + config struct (usearch_vectors_log table, UsearchConfig)
- [ ] 2.2 WAL write path (insert/delete/update → SQLite WAL + RAM index)
- [ ] 2.3 Parallel search via rayon with filtered_search
- [ ] 2.4 Global compaction (merge all segments, remove stale, slice by 1M)
- [ ] 2.5 Integration tests (persistence, compaction, config validation)

---

## 2.1 WAL table in SQLite + config struct

**Goal:** Create WAL table in SQLite and add UsearchConfig.

**Scope файлов:**
- `crates/db/src/migrations.rs` — add migration for `usearch_vectors_log` table:
  ```sql
  CREATE TABLE IF NOT EXISTS usearch_vectors_log (
      chunk_id    INTEGER PRIMARY KEY,
      flags       INTEGER NOT NULL,
      created_at  TEXT NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
  ```
- `crates/config/src/preset.rs` — add `UsearchConfig` struct:
  ```rust
  pub struct UsearchConfig {
      pub max_segment_vectors: usize,       // default 1_000_000
      pub compaction_stale_threshold: u8,   // default 30 (%)
      pub search_threads: usize,            // default 4
  }
  ```
  Add `usearch: Option<UsearchConfig>` to `VectorsConfig` with `#[serde(default)]`.
- `crates/vectors/src/lib.rs` — add `usearch` field to `VectorIndexConfig`, pass to `UsearchEngine::create`.

**Dependencies:** none.

**Критерии приёмки (машинные):**
- Migration creates table and index.
- Config parse test: `vectors.usearch.max_segment_vectors: 500000` deserializes correctly.
- Config default test: absent `usearch` section → defaults (1M, 30%, 4 threads).
- `cargo test -p db -p config` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A (new feature).

---

## 2.2 WAL write path

**Goal:** Insert/delete/update operations write to SQLite WAL + update RAM index.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add WAL methods:
  ```rust
  impl UsearchEngine {
      fn write_wal(&self, chunk_id: u32, flags: u8) -> Result<()> {
          // INSERT OR REPLACE INTO usearch_vectors_log (chunk_id, flags, created_at)
          // VALUES (?, ?, datetime('now'))
      }
      
      fn load_stale_ids(&self) -> Result<HashSet<u32>> {
          // SELECT chunk_id FROM usearch_vectors_log WHERE flags & (DEL|UPD) != 0
      }
      
      fn clear_wal(&self) -> Result<()> {
          // DELETE FROM usearch_vectors_log
      }
  }
  ```
- Modify `insert_batch` to:
  1. Write WAL record to SQLite (ADD flag)
  2. Insert vector into RAM index
- Modify `delete_by_chunk_ids` to:
  1. Write WAL record to SQLite (DEL flag)
  2. Delete from RAM index
- Modify `rebuild` to:
  1. Clear WAL table
  2. Rebuild index from SQLite chunks

**Dependencies:** 2.1.

**Критерии приёмки (машинные):**
- Insert → WAL record exists in SQLite.
- Delete → WAL record with DEL flag exists.
- Rebuild → WAL table cleared.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.3 Parallel search via rayon with filtered_search

**Goal:** Search across multiple DISK segments in parallel with WAL filtering.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add `filtered_search` and modify `search`:
  ```rust
  impl UsearchEngine {
      /// Search segment excluding stale chunk_ids.
      fn filtered_search(&self, segment: &Index, query: &[f32], k: usize, stale: &HashSet<u32>) -> Vec<(u32, f32)> {
          let mut results = segment.search(query, k)?;
          results.retain(|(id, _)| !stale.contains(id));
          results
      }
  }
  ```
- Modify `search` to use rayon:
  ```rust
  fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
      use rayon::prelude::*;
      
      let stale = self.load_stale_ids()?;
      let segments = &self.disk_segments;
      
      let results: Vec<Vec<(u32, f32)>> = segments.par_iter()
          .map(|seg| self.filtered_search(&seg.index, query, k, &stale))
          .collect();
      
      merge_results(results, k)
  }
  ```

**Dependencies:** 2.2.

**Критерии приёмки (машинные):**
- Search across 2+ segments returns correct results.
- WAL filtering excludes deleted/updated vectors.
- Parallel search via rayon works.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.4 Global compaction (merge all segments)

**Goal:** Merge all DISK segments, remove stale vectors, slice by 1M.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add compaction:
  ```rust
  impl UsearchEngine {
      fn maybe_compact(&mut self) -> Result<()> {
          let stale_pct = self.stale_vector_percentage()?;
          if stale_pct > self.usearch_config.compaction_stale_threshold as f64 {
              self.compact()?;
          }
          Ok(())
      }
      
      fn compact(&mut self) -> Result<()> {
          // 1. Load all segments + stale_ids from WAL
          // 2. For each segment, collect live vectors (exclude stale_ids)
          // 3. Create new segments, sliced by max_segment_vectors
          // 4. Replace old segments with new ones
          // 5. Clear WAL
      }
      
      fn stale_vector_percentage(&self) -> Result<f64> {
          let total = self.total_vector_count();
          let stale = self.load_stale_ids()?.len();
          Ok(stale as f64 / total as f64 * 100.0)
      }
  }
  ```

**Dependencies:** 2.2, 2.3.

**Критерии приёмки (машинные):**
- Compaction triggers when stale > compaction_stale_threshold.
- After compaction, segment count = ceil(total_vectors / max_segment_vectors).
- After compaction, WAL is cleared.
- Search results unchanged after compaction.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.5 Integration tests

**Goal:** Test persistence, compaction, and config validation.

**Scope файлов:**
- `crates/vectors/tests/wal_integration.rs` (new test file):
  - `test_wal_persistence_across_restart` — insert, restart, verify data persists
  - `test_compaction_reduces_segments` — insert enough to trigger compaction, verify segment count
  - `test_search_correctness_with_wal` — verify search results with WAL filtering
  - `test_config_validation` — invalid WAL config values rejected
  - `test_parallel_search` — verify rayon parallel search works

**Dependencies:** 2.2, 2.3, 2.4.

**Критерии приёмки (машинные):**
- All integration tests pass.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.
