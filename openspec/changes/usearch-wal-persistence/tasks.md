# Tasks: usearch-wal-persistence

- [ ] 2.1 WAL table in SQLite + config struct (usearch_vectors_log table, WalConfig)
- [ ] 2.2 WAL write path (insert/delete/update → SQLite WAL + RAM index)
- [ ] 2.3 Parallel search across DISK segments + WAL filtering
- [ ] 2.4 Global compaction (merge all segments, remove stale, slice by 1M)
- [ ] 2.5 Integration tests (persistence, compaction, config validation)

---

## 2.1 WAL table in SQLite + config struct

**Goal:** Create WAL table in SQLite and add WAL config to VectorsConfig.

**Scope файлов:**
- `crates/db/src/migrations.rs` — add migration for `usearch_vectors_log` table:
  ```sql
  CREATE TABLE IF NOT EXISTS usearch_vectors_log (
      chunk_id    INTEGER PRIMARY KEY,
      flags       INTEGER NOT NULL,
      vector      BLOB,
      created_at  TEXT NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
  ```
- `crates/config/src/preset.rs` — add `WalConfig` struct:
  ```rust
  pub struct WalConfig {
      pub max_segment_vectors: usize,       // default 1_000_000
      pub compaction_stale_threshold: u8,   // default 30 (%)
      pub search_threads: usize,            // default 4
  }
  ```
  Add `wal: WalConfig` to `VectorsConfig` with `#[serde(default)]`.
- `crates/vectors/src/lib.rs` — add `wal` field to `VectorIndexConfig`, pass to `UsearchEngine::create`.

**Dependencies:** none.

**Критерии приёмки (машинные):**
- Migration creates table and index.
- Config parse test: `vectors.wal.max_segment_vectors: 500000` deserializes correctly.
- Config default test: absent `wal` section → defaults (1M, 30%, 4 threads).
- `cargo test -p db -p config` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A (new feature).

---

## 2.2 WAL write path

**Goal:** Insert/delete/update operations write to SQLite WAL + update RAM index.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add WAL methods:
  ```rust
  impl UsearchEngine {
      fn write_wal(&self, chunk_id: u32, flags: u8, vector: Option<&[f32]>) -> Result<()> {
          // INSERT OR REPLACE INTO usearch_vectors_log (chunk_id, flags, vector, created_at)
          // VALUES (?, ?, ?, datetime('now'))
      }
      
      fn apply_wal_to_index(&mut self) -> Result<()> {
          // Read WAL from SQLite, replay into RAM index
          // SELECT chunk_id, flags, vector FROM usearch_vectors_log ORDER BY created_at
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

## 2.3 Parallel search across DISK segments + WAL filtering

**Goal:** Search across multiple DISK segments in parallel with WAL filtering.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — modify `search`:
  ```rust
  fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
      let threads = self.wal_config.search_threads;
      let segments = self.disk_segments.clone();
      
      // Parallel search across segments
      let results: Vec<Vec<(u32, f32)>> = std::thread::scope(|s| {
          let handles: Vec<_> = segments.chunks(segments.len() / threads + 1)
              .map(|chunk| {
                  s.spawn(move || {
                      chunk.iter().map(|seg| {
                          let mut results = seg.search(query, k)?;
                          // Filter by WAL: exclude deleted/updated chunk_ids
                          results.retain(|(id, _)| !self.is_stale(*id, seg.id));
                          Ok(results)
                      }).collect::<Result<Vec<_>>>()
                  })
              }).collect();
          handles.into_iter().map(|h| h.join().unwrap()).collect()
      });
      
      // Merge and deduplicate
      merge_results(results, k)
  }
  ```

**Dependencies:** 2.2.

**Критерии приёмки (машинные):**
- Search across 2+ segments returns correct results.
- WAL filtering excludes deleted/updated vectors.
- Parallel search performance ≥ sequential (measured).
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
          if stale_pct > self.wal_config.compaction_stale_threshold {
              self.compact()?;
          }
          Ok(())
      }
      
      fn compact(&mut self) -> Result<()> {
          // 1. Read all segments + WAL
          // 2. Identify stale vectors (DEL flag or missing from SQLite)
          // 3. Create new segments with live vectors (slice by max_segment_vectors)
          // 4. Replace old segments with new ones
          // 5. Clear WAL
      }
      
      fn stale_vector_percentage(&self) -> Result<f64> {
          // Count DEL flags in WAL / total vectors
      }
  }
  ```

**Dependencies:** 2.2, 2.3.

**Критерии приёмки (машинные):**
- Compaction triggers when stale > 30%.
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

**Dependencies:** 2.2, 2.3, 2.4.

**Критерии приёмки (машинные):**
- All integration tests pass.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.
