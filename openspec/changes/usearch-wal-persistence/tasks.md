# Tasks: usearch-wal-persistence

- [ ] 2.1 WAL binary format + config struct (implement WAL record serialization, add vectors.wal config)
- [ ] 2.2 WAL layer management in UsearchEngine (RAM/DISK layers, flush, search fan-out)
- [ ] 2.3 Compaction logic (merge last two DISK layers, WAL filtering)
- [ ] 2.4 Integration tests (persistence across restart, compaction, config validation)
- [ ] 2.5 Benchmark WAL vs rebuild-only (latency, durability, SSD wear)

---

## 2.1 WAL binary format + config struct

**Goal:** Implement WAL record serialization and add WAL config to VectorsConfig.

**Scope файлов:**
- `crates/vectors/src/wal.rs` (new module, ~150 LOC):
  - `pub struct WalRecord { pub chunk_id: u32, pub flags: WalFlags, pub vector: Option<Vec<f32>> }`
  - `pub struct WalFlags { pub add: bool, pub del: bool, pub upd: bool }`
  - `impl WalRecord { pub fn encode(&self, dim: usize) -> Vec<u8>; pub fn decode(buf: &[u8], dim: usize) -> Result<Self>; }`
  - Binary format: [u32 LE chunk_id][u8 flags][f32 LE × dim] (dim only for ADD/UPD)
- `crates/config/src/preset.rs` — add `WalConfig` struct:
  ```rust
  pub struct WalConfig {
      pub ram_threshold_mb: usize,      // default 512
      pub disk_count_threshold: usize,  // default 5
      pub wal_size_threshold_pct: u8,   // default 50
  }
  ```
  Add `wal: WalConfig` to `VectorsConfig` with `#[serde(default)]`.
- `crates/vectors/src/lib.rs` — add `wal` field to `VectorIndexConfig`, pass to `UsearchEngine::create`.

**Dependencies:** none (standalone module).

**Критерии приёмки (машинные):**
- `WalRecord::encode` + `WalRecord::decode` round-trip test (1000 records, dim=1024).
- `WalFlags` bit manipulation tests (all 8 combinations).
- Config parse test: `vectors.wal.ram_threshold_mb: 1024` deserializes correctly.
- Config default test: absent `wal` section → defaults (512, 5, 50).
- `cargo test -p vectors -p config` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A (new feature; no Go equivalent).

---

## 2.2 WAL layer management in UsearchEngine

**Goal:** Implement RAM/DISK layer management with WAL tracking.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add to `UsearchEngine`:
  ```rust
  struct WalLayer {
      index_path: PathBuf,
      wal_path: PathBuf,
      wal_records: Vec<WalRecord>,
  }
  
  struct UsearchEngine {
      ram_index: Index,           // mutable, live inserts
      ram_wal: Vec<WalRecord>,    // WAL for RAM layer
      disk_layers: Vec<WalLayer>, // read-only mmap layers
      config: VectorIndexConfig,
      wal_config: WalConfig,
      path: PathBuf,
  }
  ```
- Modify `insert_batch` to:
  1. Add vectors to `ram_index`
  2. Append WAL records to `ram_wal`
  3. Check if `ram_index.size() * dim * 2 > wal_config.ram_threshold_mb * 1024 * 1024`
  4. If threshold exceeded → call `flush_ram_to_disk()`
- Implement `flush_ram_to_disk()`:
  1. Save `ram_index` to `index.usearch.<N>`
  2. Write `ram_wal` to `WAL_<N>.bin`
  3. Create new `WalLayer` with mmap view
  4. Clear `ram_wal`, recreate `ram_index`
- Modify `search` to:
  1. Search `ram_index` (unfiltered)
  2. For each disk layer, search and filter by combined WAL
  3. Deduplicate by chunk_id, keep most recent

**Dependencies:** 2.1.

**Критерии приёмки (машинные):**
- Insert 10K vectors → RAM flushes to DISK when threshold exceeded.
- Search after flush returns same results as before flush.
- WAL records are persisted and replayed on reopen.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.3 Compaction logic

**Goal:** Merge last two DISK layers when thresholds are exceeded.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — add:
  - `fn maybe_compact(&mut self)` — check thresholds, trigger compaction
  - `fn compact(&mut self, layer_a: WalLayer, layer_b: WalLayer)` — merge two layers
  - `fn merge_layers(a: &WalLayer, b: &WalLayer, dim: usize) -> WalLayer` — merge logic

**Compaction strategy:**
1. Read all chunk_ids from both WALs (merged set)
2. For each chunk_id, determine final state (add/del/upd)
3. Load vectors from SQLite (source of truth) for surviving chunk_ids
4. Create new usearch index with merged vectors
5. Write merged WAL (only ADD records for surviving vectors)
6. Delete old layers

**Dependencies:** 2.2.

**Критерии приёмки (машинные):**
- After compaction, search returns same results as before.
- Old DISK layers are deleted.
- WAL size is reduced after compaction.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.4 Integration tests

**Goal:** Test persistence across restarts and config validation.

**Scope файлов:**
- `crates/vectors/tests/wal_integration.rs` (new test file):
  - `test_wal_persistence_across_restart` — insert, restart engine, verify data persists
  - `test_compaction_reduces_layers` — insert enough to trigger compaction, verify layer count
  - `test_config_validation` — invalid WAL config values rejected
  - `test_search_fan_out_with_multiple_layers` — verify search works across RAM + N DISK layers

**Dependencies:** 2.2, 2.3.

**Критерии приёмки (машинные):**
- All integration tests pass.
- `cargo test -p vectors --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.

---

## 2.5 Benchmark WAL vs rebuild-only

**Goal:** Measure latency and durability improvements.

**Scope файлов:**
- `crates/parity-harness/tests/wal_benchmark.rs` (new test):
  - Compare insert latency: WAL vs rebuild-only
  - Measure flush overhead
  - Measure compaction time
  - Output comparison table

**Dependencies:** 2.2, 2.3.

**Критерии приёмки (машинные):**
- Insert latency with WAL ≤ 1.5× without WAL (overhead acceptable).
- Flush overhead ≤ 2 seconds for 512 MB.
- Compaction completes in ≤ 10 seconds for 1M vectors.
- `cargo test -p parity-harness --features engine-usearch` green; `clippy -D warnings`, `fmt --check` clean.

**Oracle refs:** N/A.
