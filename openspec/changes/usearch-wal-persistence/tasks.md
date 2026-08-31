# Tasks: usearch-wal-persistence

- [x] 2.1 WAL table in SQLite + config struct (usearch_vectors_log table, UsearchConfig)
- [x] 2.2 WAL write path — **superseded** by ADR 0004 (rework 3.5)
- [x] 2.3 Parallel search via rayon with filtered_search — **superseded** by ADR 0004 (rework 3.6)
- [x] 2.4 Global compaction (merge all segments) — **superseded** by ADR 0004 (rework 3.8)
- [~] 2.5 Integration tests — **replaced** by 3.10
- [x] 3.1 Fix db-crate tests (user_version 4, segment_id) — unblock workspace
- [x] 3.2 Sidecar key-manifest codec (.keys)
- [x] 3.3 On-disk layout + create/open + crash recovery + rebuild reset
- [x] 3.4 Module split (usearch_engine.rs → vectors/src/usearch/ submodules)
- [x] 3.5 WAL write path (transactional DEL, stale cache, count/chunk_ids)
- [x] 3.6 Parallel search (rayon pool, per-segment filters, freshest-wins merge)
- [ ] 3.7 Flush on overflow + shutdown save
- [x] 3.7 Flush on overflow + shutdown save (build_index)
- [ ] 3.8 Background compaction (maybe_compact, monotonic ids, atomic swap)
- [ ] 3.9 Wiring (factory + WAL db path + UsearchConfig, bootstrap, cleanup)
- [ ] 3.10 Integration tests + fix layout-dependent tests

**Source of truth for all 3.x tasks:** `docs/adr/0004-usearch-lsm-segments.md` (architecture, crash semantics, usearch 2.26.1 API facts). Each agent MUST read the ADR, this file, `openspec/config.yaml`, and the current `crates/vectors/src/{lib.rs,usearch_engine.rs}` before coding. No Go-oracle reference exists (new operational mechanism); the only parity surface is the `VectorIndex` trait contract.

**Common gates for every 3.x task:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` green. Feature: `cargo test -p vectors --features engine-usearch`.

---

## 3.1 Fix db-crate tests (user_version 4, segment_id)

**Goal:** Make `cargo test -p db` green after migrations 3+4 (`usearch_vectors_log` with `segment_id` PK) were added: 6 failing tests.

**Scope файлов:**
- `crates/db/src/connection.rs` — test assertions only (no production code changes)
- `crates/db/src/test_util.rs` — if a test helper asserts user_version or schema state
- DO NOT touch migrations, production connection code, or other crates.

**What to fix (verify each against actual failure output first):**
1. All `user_version` assertions: expect **4** (final schema version after migration 4), not 3.
2. `usearch_vectors_log_table_and_index_exist` (or equivalent): inserts must include `segment_id` (NOT NULL with PK `(segment_id, chunk_id)`); update any `PRAGMA table_info`/schema assertions to include the `segment_id` column and the `(segment_id, chunk_id)` PK.
3. Any temp-file/fixture test in `test_util.rs` asserting the old schema.

**Dependencies:** none.

**Критерии приёмки:**
- `cargo test -p db` — 0 failures
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` — clean
- No production (non-test) code changed: `git diff --stat` shows only test code.

---

## 3.2 Sidecar key-manifest codec (.keys)

**Goal:** Implement the binary key-manifest format from ADR 0004 §2/§3: magic `0x534B4559` ("SKEY"), u32 LE count, then count × u32 LE keys. This file is the durable record of all keys in a segment (replaces the dead `load_live_keys_per_segment` scan).

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — private codec (module `keys` or free functions): `write_keys(path: &Path, keys: &[u32]) -> Result<(), VectorsError>` (write to `path.with_extension("tmp")` or `path + ".tmp"`, fsync, atomic rename — ADR §3) and `read_keys(path: &Path) -> Result<Vec<u32>, VectorsError>` (validate magic, count, exact byte length; distinct `VectorsError` variants or message for missing file / bad magic / truncated file).
- Tests in the same file (test module).

**Dependencies:** 3.1 (green workspace).

**Критерии приёмки:**
- Round-trip test: write 0, 1, and many keys → read → equal; empty manifest (count=0) works.
- Error tests: missing file, corrupted magic, truncated payload, extra trailing bytes — all return `Err`, none panic.
- Atomicity: `write_keys` leaves no `.tmp` residue on success (test: dir listing after write).
- Gates: fmt/clippy/test as in header.

---

## 3.3 On-disk layout + create/open + crash recovery + rebuild reset

**Goal:** Replace the single `index.usearch` file with the ADR 0004 §2 layout and implement `create`/`open` with startup recovery. RAM layer = in-memory copy (`restore_from_buffer`); DISK layers = read-only mmap views (`restore_view`).

**Layout (ADR 0004 §2):**
```
<vectors_path>/usearch/
├── ram.usearch + ram.keys          # RAM snapshot (saved on flush/shutdown)
└── segments/
    ├── segment-N.usearch + segment-N.keys   # DISK[n], id monotonic
    └── segment-N.tmp / segments.tmp/ / *.old/  # crash garbage
```

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — layout constants/paths, `create`, `open` (new signature: `open_with_wal(path, dim, config, wal_db_path: Option<&Path>)` or equivalent; the old `open`/factory entry must keep compiling — see lib.rs note), startup garbage cleanup, dim check, WAL reconciliation on open, `rebuild` full reset.
- `crates/vectors/src/lib.rs` — **only** if a factory call site must pass `None` for the new wal-db parameter (keep behavior identical for now; real wiring is 3.9).
- `crates/vectors/src/error.rs` (or wherever `VectorsError` lives) — new variants if needed (e.g. `CorruptManifest`, `DimensionMismatch`).
- Tests in `usearch_engine.rs` test module.

**Behavior (ADR 0004 §6, §7):**
1. `create`: parent dirs, empty RAM index (dimension from config), write empty `ram.keys` (count=0), empty `segments/`. No WAL rows.
2. `open`:
   a. garbage: delete `segments.tmp/`, `*.old/`, `*.tmp` files (ADR §7 crash matrix);
   b. scan `segments/segment-*.usearch` + `.keys` pairs (both must exist; missing sidecar for an existing index = corrupt → distinct error, ADR §3);
   c. load DISK via `Index::restore_view` (read-only mmap), keep `Arc<Index>`;
   d. RAM: if `ram.usearch` exists → `restore_from_buffer` into memory; else empty index; `ram.keys` from sidecar;
   e. dim check: segment dimension == configured → distinct error on mismatch;
   f. WAL reconciliation (if wal db present): `DELETE FROM usearch_vectors_log WHERE segment_id NOT IN (0, <existing disk ids>)` — self-heal after crashed compaction/flush (ADR §7);
   g. build the stale-set cache from WAL (see 3.5 — if cache not implemented yet, load raw sets; 3.5 refactors).
3. `rebuild(rows)`: full reset (ADR §9 D9): clear WAL (`DELETE` all rows), delete all `segments/` files, RAM = rows only, write `ram.keys`. Old DISK layers must NOT survive rebuild (defect of current impl).
4. Search/insert/delete for the RAM-only state must keep working exactly as today (no DISK segments yet — that's fine; 3.6–3.8 add them).

**Dependencies:** 3.2 (sidecar codec).

**Критерии приёмки:**
- `create` in empty dir → layout exists, `open` returns empty engine (count=0), no error.
- `open` of nonexistent path → `VectorsError` «index does not exist» (distinct variant, unchanged contract).
- Garbage cleanup: pre-place `segments.tmp/segment-9.usearch`, `segment-1.tmp`, `stale.old` → after `open` all gone; valid segments intact.
- Corrupt sidecar (bad magic) → distinct error, no panic.
- Dim mismatch (create with dim A, corrupt-tamper not needed — unit-test the check function or open a segment file built with another dim) → distinct error.
- WAL reconciliation: pre-populate WAL with rows for segment_id 5 (no segment-5 files) and segment_id 2 (segment-2 present) → after open only segment_id=2 rows remain.
- `rebuild` with 3 DISK segments + WAL rows → after: 0 DISK segments, 0 WAL rows, count() == rows.len().
- Gates: fmt/clippy/test as in header; existing `cargo test -p vectors --features engine-usearch` tests that referenced the old `index.usearch` path must be updated to the new layout (report each in the summary).

---

## 3.4 Module split (usearch_engine.rs → vectors/src/usearch/ submodules)

**Goal:** `crates/vectors/src/usearch_engine.rs` has grown to ~2000 lines and will grow further (tasks 3.5–3.8). Split it into logical submodules under `crates/vectors/src/usearch/`. **Behavior-preserving refactoring only** — no logic changes, no new features; all tests pass unchanged.

**Scope файлов:**
- `crates/vectors/src/usearch/` (new directory module) — proposed split (adjust names if a boundary is clearly better, keep ≤ ~6 files):
  - `mod.rs` — `UsearchEngine` struct + public API (create/create_with_config/open/open_with_wal/with_wal_db/insert/insert_batch/search/delete_by_chunk_ids/chunk_ids/count/build_index/rebuild/save/config/path/stale_sets) + `impl VectorIndex for UsearchEngine` + engine-level tests
  - `layout.rs` — path constants/helpers (ram_index_path, ram_keys_path, segments_dir, segment_*_path, layout_exists), `cleanup_garbage`, `load_disk_segments`, `load_ram_layer`, `DiskSegment`, layout tests
  - `keys_manifest.rs` — the sidecar codec (moved from the current `keys_manifest` module) + its tests
  - `wal.rs` — WAL helpers (reconcile_wal, load_stale_sets, write_wal*, wal_guard/mutex_guard, stale-set state) + tests
  - `options.rs` — `options()`, `quantization()`, `key_to_chunk_id`, `map_usearch`, `map_sqlite`, `empty_index`
  - merge `search`/`merge_results`/`add_rows` helpers into `mod.rs` or a `search.rs` — your call, whichever keeps files cohesive
- `crates/vectors/src/usearch_engine.rs` — **deleted** (or reduced to a deprecated re-export shim only if external code references the module path — check first)
- `crates/vectors/src/lib.rs` — `mod usearch;` + keep the public re-export `pub use ...::UsearchEngine` so `vectors::UsearchEngine` and any `vectors::usearch_engine::` paths keep resolving (verify what cli/search/ingestion actually import and preserve exactly that surface; do NOT change visibility of `UsearchEngine` or its methods)

**Dependencies:** 3.3.

**Rules:**
- Pure move + `use`/`pub(super)` wiring; every moved item keeps its doc comments and visibility (private stays private to the `usearch` module tree, `pub` surface unchanged).
- Test modules move with their code (layout_tests → layout.rs, keys_tests → keys_manifest.rs, etc.); test names unchanged.
- No new dependencies, no logic edits (if you find a bug, report it in `questions`, do not fix it here).

**Критерии приёмки:**
- `git diff --stat` shows the split as renames/moves (git rename detection) with minimal logic-diff lines.
- Public API of crate `vectors` unchanged: `cargo doc -p vectors --no-deps` builds; `vectors::UsearchEngine` resolvable from cli/search/ingestion exactly as before.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `timeout 300 cargo test --workspace` — all green, same test counts as before the split.
- No file in the new tree exceeds ~700 lines.

---

## 3.5 WAL write path (transactional DEL, stale cache, count/chunk_ids)

**Goal:** Implement the ADR 0004 §4 write semantics: WAL-first, DEL-only, one SQLite transaction per operation, versioned in-memory stale cache; `count()`/`chunk_ids()` via manifests minus stale (no `exact_search` scans).

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — `insert_batch`, `delete_by_chunk_ids`, stale cache + `AtomicU64` version, `count`, `chunk_ids`, WAL helpers (replace the dead `write_wal`/`load_stale_ids` from the superseded 2.2).
- `crates/vectors/src/error.rs` — new variants if needed (e.g. `Wal`).
- Tests in `usearch_engine.rs` test module.

**Semantics (ADR 0004 §4 — read it before coding):**
1. `insert_batch(keys, vectors)`:
   a. one transaction: for each key K present in any older segment (RAM manifest ∪ DISK manifests), insert `(s, K, DEL)` for EVERY such segment s (supersession); commit;
   b. then add to RAM index + update `ram.keys` (in-memory; sidecar written on flush/shutdown — ADR §3);
   c. WAL-first: if the transaction fails, nothing is added to RAM.
2. `delete_by_chunk_ids(keys)`: one transaction: `(0, K, DEL)` for keys in RAM + `(s, K, DEL)` for keys in each DISK segment s; commit; then `remove` from RAM (usearch `remove` on RAM is fine — RAM is in-memory).
3. Stale cache: `HashMap<u32 /*segment_id*/, HashSet<u32>>` + `Arc<AtomicU64>` version; rebuilt from WAL on open; **bumped** (version increment + set update) on every write transaction — steady-state search reads the cache, zero SQL (ADR §4.3, §8 gate).
4. `count()` = |ram.keys − stale[0]| + Σ |keys(s) − stale[s]|. `chunk_ids()` = union of the same sets. No index scans.
5. The WAL connection: engine owns a dedicated `rusqlite::Connection` (opened from the wal-db path passed at open; keep `Option` for engines constructed without a db — in that case writes are RAM-only and count/chunk_ids = manifests, as today's no-WAL path).

**Dependencies:** 3.4.

**Критерии приёмки:**
- Insert key K into RAM, then re-insert K (supersession): WAL contains `(0, K, DEL)` exactly once (not duplicated on re-insert if K was RAM-only — ADR §4: supersession rows only for keys present in a segment being superseded; verify the exact rule in ADR and test it), RAM has K once, count() unchanged.
- After a flush-like state is impossible yet (no DISK in 3.5): test the supersession SQL against a **pre-seeded** DISK segment (create engine, manually place segment-1 files + WAL state via the codec from 3.2, reopen) → re-inserting K from segment-1 yields `(1, K, DEL)` and search returns the new vector (search merge itself lands in 3.6 — test the WAL rows here, search in 3.6).
- `delete_by_chunk_ids` for a key in RAM and (pre-seeded) in segment-1 → `(0,K,DEL)` and `(1,K,DEL)` rows, RAM `contains(K)` false, count() decremented.
- Stale cache: after writes, `count()`/`chunk_ids()` return correct values; version incremented; a search-path read does not execute SQL (test by asserting the cache is hit — e.g. a counter or by closing the WAL connection after open and still getting correct count).
- Transaction failure (simulate via a second connection that locks the table, or a failing prepared statement in a unit test of the helper) → RAM unchanged (WAL-first).
- Gates: fmt/clippy/test as in header.

---

## 3.6 Parallel search (rayon pool, per-segment filters, freshest-wins merge)

**Goal:** ADR 0004 §5 search: dedicated `rayon::ThreadPool` sized by `UsearchConfig::search_threads`, per-layer `filtered_search` with the stale cache (plain `search` when a layer's stale set is empty), merge: duplicate chunk id → **freshest layer wins** (RAM > higher segment id), then distance asc, truncate to k.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — `search` rewrite, thread pool creation (in `open`/constructor), merge helper.
- Tests in `usearch_engine.rs` test module.

**Semantics (ADR 0004 §5):**
- Layers: RAM (freshest, id 0) + all DISK[n]. Search all in parallel; each layer returns its top-k (after filtering); merge resolves duplicates by layer freshness (NOT by distance — the fresh layer holds the current vector); final order by distance asc; length ≤ k.
- Empty stale set for a layer → plain `search` (no filter closure overhead).
- `search_threads` from config actually sizes the pool (defect #9 of the superseded impl).

**Dependencies:** 3.5.

**Критерии приёмки:**
- RAM-only search equals old behavior (regression: existing search tests stay green).
- Pre-seeded DISK segment + RAM: query returns union; a key present in both (stale row in DISK, live in RAM) appears **once** with the RAM vector's distance.
- A key stale in both layers (deleted) does not appear even if the index still contains it.
- k truncation: with more candidates than k, result length == k, sorted by distance asc.
- Pool sizing: constructing the engine with `search_threads: 2` uses a 2-thread pool (assert via `pool.current_num_threads()` or equivalent public accessor).
- Gates: fmt/clippy/test as in header.

---

## 3.7 Flush on overflow + shutdown save

**Goal:** ADR 0004 §5.4/§8: when RAM size reaches `max_segment_vectors`, flush RAM to a new DISK segment; persist RAM on shutdown (`build_index()` repurposed as the save point — the serving path calls it, see ADR §9).

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` — `flush_ram()` (private), overflow check in `insert_batch` (after add), `build_index()`/save method, `ram.keys` sidecar write.
- Tests in `usearch_engine.rs` test module.

**Semantics (ADR 0004 §5.4, §7):**
1. `flush_ram()`:
   a. `save()` RAM index → `segments/segment-N.usearch` (N = max existing id + 1, monotonic — ADR D4: never renumber);
   b. write `segment-N.keys` (atomic, 3.2 codec);
   c. `DELETE FROM usearch_vectors_log WHERE segment_id = 0` **in the same ordering as ADR §7** (flush ordering: file → sidecar → WAL delete → RAM reset; a crash between any two steps must be recoverable by 3.3's `open` — verify against the ADR crash matrix);
   d. reset RAM to empty, clear `ram.keys`, write empty `ram.keys` sidecar (or per ADR §3 — follow the ADR exactly).
2. Overflow: after `insert_batch` adds, if `ram.size() >= max_segment_vectors` → `flush_ram()`.
3. Shutdown save: persist current RAM (`ram.usearch` + `ram.keys`) — idempotent, no-op if RAM empty. This is what the serving path calls (wiring in 3.9).
4. DISK segment loading: flushed segments are immediately available to search (add to the `disk_segments` list with `restore_view` after flush, or per ADR §5 — follow the ADR).

**Dependencies:** 3.6.

**Критерии приёмки:**
- With `max_segment_vectors: 100`, insert 150 → one DISK segment exists (segment-1), RAM size == 50, search finds all 150.
- Flush ordering crash simulation: build the flushed state, delete the sidecar only → reopen → distinct corrupt error (ADR §3); delete the segment file only → reopen → garbage cleanup recovers (verify the exact expected behavior in ADR §7 and test that).
- Shutdown save: insert 10, save, reopen (fresh engine on same dir) → count == 10, search finds them; save again with no new inserts → no error, no duplicate.
- `count()` across RAM+DISK correct after flush (manifests − stale).
- Gates: fmt/clippy/test as in header.

---

## 3.8 Background compaction (maybe_compact, monotonic ids, atomic swap)

**Goal:** ADR 0004 §6: trigger `stale_total/total_disk*100 > compaction_stale_threshold`; background `std::thread`; repack live vectors into new segments with **new monotonic ids** (sliced by `max_segment_vectors`); atomic directory swap (`segments/` → `segments.old/`, `segments.tmp/` → `segments/`); WAL cleanup **after** the swap; exposed as additive trait method `maybe_compact()`.

**Scope файлов:**
- `crates/vectors/src/lib.rs` — `VectorIndex` trait: `fn maybe_compact(&self) -> Result<(), VectorsError> { Ok(()) }` (additive default — ALL existing impls keep compiling; LanceEngine inherits no-op).
- `crates/vectors/src/usearch_engine.rs` — `maybe_compact` impl (spawn/join-once semantics per ADR: single-flight, no concurrent compactions), compactor thread, swap logic.
- `crates/vectors/src/lance_engine.rs` — **only if** it fails to compile after the trait change (it must not need changes — verify).
- Tests in `usearch_engine.rs` test module.

**Semantics (ADR 0004 §6, §7 — read the crash matrix before coding):**
1. Trigger check: `stale_total` = DEL rows with `segment_id > 0` (from cache), `total_disk` = Σ DISK `size()`; percentage > threshold → compact. Below threshold → no-op, cheap.
2. Repack: for each DISK segment, live keys = `keys(s) − stale[s]`; collect `(key, vector)` via index `get`/exact lookup (per ADR §6); write new segments `N+1..N+M` in `segments.tmp/` (N = max current id — monotonic, ADR D4); each with sidecar.
3. Swap: rename `segments/` → `segments.old/`, rename `segments.tmp/` → `segments/`, remove `segments.old/`; update in-memory `disk_segments` (Arc clones — search continues on old segments during compaction, ADR §6).
4. WAL cleanup AFTER swap: `DELETE FROM usearch_vectors_log WHERE segment_id > 0` (old ids are gone; new segments are clean). Crash between swap and cleanup → 3.3 reconciliation on next open removes orphan rows (test this).
5. Single-flight: a second `maybe_compact()` while one runs is a no-op (or queued-once per ADR — follow the ADR).
6. Compaction runs off the caller's thread (background `std::thread`); `maybe_compact` returns promptly (ADR §6: search is not blocked).

**Dependencies:** 3.7.

**Критерии приёмки:**
- Below threshold → `maybe_compact` returns Ok, no new segments, no WAL change.
- Pre-seed: 2 DISK segments, 40% stale (threshold 30) → `maybe_compact` → new segment id(s) > old max, old ids gone, `count()` unchanged, search results identical (same live keys), WAL rows for old ids deleted.
- Slicing: live vectors > `max_segment_vectors` → M = ceil(live/max) new segments, each ≤ max (last may be smaller).
- Crash between swap and WAL cleanup: simulate (pre-place new segments as `segments/`, orphan WAL rows for old ids) → `open` → reconciliation removes orphans, engine consistent.
- Single-flight: concurrent `maybe_compact` calls → exactly one compaction (test with a slow repack or a flag).
- `cargo test --workspace` compiles all trait impls (LanceEngine untouched or trivially adjusted).
- Gates: fmt/clippy/test as in header.

---

## 3.9 Wiring (factory + WAL db path + UsearchConfig, bootstrap, cleanup)

**Goal:** ADR 0004 §9: production code actually uses the persistence — factory passes the knowledge.db path and `VectorIndexConfig.usearch`; serve calls shutdown-save; ingestion cleanup calls `maybe_compact`.

**Scope файлов:**
- `crates/vectors/src/lib.rs` — `create_vector_engine`/factory: accept + pass the WAL db path (knowledge.db) and map `VectorIndexConfig.usearch` (already a field — currently ignored, defect #3) into the engine.
- `crates/cli/src/serve/bootstrap.rs` — `vectors_index_config`: set the `.usearch` field from the preset (`config::UsearchConfig` → `vectors::UsearchConfig` field by field, dependency direction D7); pass the knowledge.db path to the factory.
- `crates/cli/src/serve/server.rs` — graceful shutdown: call the save point (3.7) on the vector engine before exit (find the existing shutdown path; the dimension-mismatch `rebuild` call site must keep working with the new layout).
- `crates/ingestion/src/runner/cleanup.rs` — after `reconcile_vectors` batch: `maybe_compact()` (fire-and-forget Ok/Err log per ADR §9 — check how the oracle's cleanup logs; keep it non-fatal).
- `crates/cli/src/db.rs` (or wherever the db fixture for tests lives) — if tests construct the engine via the factory with the old signature, update call sites.
- Tests: update affected unit tests; add one wiring test (factory with a temp knowledge.db + preset usearch section → engine has WAL + config applied).

**Dependencies:** 3.8 (all engine features exist).

**Критерии приёмки:**
- Factory test: preset with `vectors.usearch.max_segment_vectors: 42` → engine uses 42 (observable via a test seam or the overflow behavior).
- Bootstrap test: `vectors_index_config` maps all 3 fields; factory receives the db path (assert via the engine's WAL working end-to-end: insert → restart engine on same db+dir → data visible).
- `cargo test --workspace` green (all call sites compile).
- No behavior change for the LanceEngine path (its tests untouched and green).
- Gates: fmt/clippy/test as in header.

---

## 3.10 Integration tests + fix layout-dependent tests

**Goal:** End-to-end tests of the persistence architecture (replaces superseded 2.5) + fix any remaining tests that assume the old `index.usearch` single-file layout.

**Scope файлов:**
- `crates/vectors/tests/persistence_integration.rs` (new) — end-to-end scenarios on a temp dir + temp knowledge.db.
- Any existing test file in the workspace that references the old layout (`crates/vectors/src/lib.rs` factory tests, `crates/cli` db/serve tests) — update to the new layout.

**Tests (ADR 0004 §8 acceptance):**
1. **Persistence across restart:** insert 300 (max_segment_vectors=100 → 2 flushes), shutdown-save, reopen fresh engine (same dir + db) → count == 300, search returns all inserted keys for exact-match queries.
2. **Search correctness with WAL:** insert K in RAM, flush, re-insert K (supersession), delete another key → search: K present once (fresh vector), deleted key absent.
3. **Compaction end-to-end:** build stale state across ≥2 DISK segments, `maybe_compact`, reopen → consistent, WAL cleaned, no data loss.
4. **Parallel search:** 1000 vectors across RAM+DISK, `search_threads: 2`, k=10 → results match a single-threaded reference (same set, same order).
5. **Config validation:** `max_segment_vectors: 0`, `compaction_stale_threshold: 0/101`, `search_threads: 0` → rejected by the config crate (these tests may already exist from 2.1 — verify and extend, don't duplicate).
6. **Crash recovery matrix (light):** the three 3.3/3.7/3.8 crash simulations as integration-level tests if not already covered (deduplicate — reference unit tests if they exist).

**Dependencies:** 3.9.

**Критерии приёмки:**
- All tests above green; `cargo test --workspace` fully green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean.
- No test references the old `index.usearch` path (grep: `git grep 'index\.usearch'` returns nothing outside the ADR/doc text).
- Summary lists every test file touched and why.
