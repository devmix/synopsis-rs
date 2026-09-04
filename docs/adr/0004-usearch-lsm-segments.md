# ADR 0004 — usearch: a two-tier RAM/DISK architecture with per-segment WAL

**Status:** accepted (target architecture decisions — the human, 2026-08-31; details — below).
**Date:** 2026-08-31 · **Change:** usearch-wal-persistence

## Question

`UsearchEngine` must provide durable and correct search under unbounded corpus growth: a RAM tier (working data) + DISK[n] tiers (historical, read-only), parallel search across all tiers with merging, a per-segment WAL in SQLite, RAM overflow → a new DISK[n], and background compaction (vacuum) when the stale-vector threshold is exceeded. The current partial implementation (commits up to 708f76c) is not the truth — it is audited below and rewritten per this ADR.

## Audit of the current state (why the current code is rewritten)

| # | Defect | Where |
|---|---|---|
| 1 | **The DISK tier is dead code**: `disk_segments` is always empty (`Vec::new()` in every constructor), no RAM→DISK[n] flush path exists, `open` does not restore segments, no segment files exist | `usearch_engine.rs:162,192,226` |
| 2 | **WAL is not wired in production**: `with_wal_db` has no callers; the `create_vector_engine` factory passes neither WAL nor `UsearchConfig` | `usearch_engine.rs:246`, `lib.rs:428-476`, `cli/src/serve/bootstrap.rs:418-432` |
| 3 | **`cargo test -p db` is red**: migration 4 was added, but the tests expect `user_version = 3` and insert rows without `segment_id` (no DEFAULT in the new table) — 6 failures | `db/src/connection.rs:347-491`, `db/src/test_util.rs` |
| 4 | **`maybe_compact(&mut self)`** is not callable through `Arc<dyn VectorIndex>`; there are no callers | `usearch_engine.rs:587` |
| 5 | **`count()`/`chunk_ids()` see only RAM** — DISK segments are invisible to reconcile/GC | `usearch_engine.rs:410,434` |
| 6 | **`compact` does not persist new segments to disk** (only the in-memory `Index`); it does not remove old files; the dedup comment "later segment wins" contradicts the code (the **earlier** segment wins) | `usearch_engine.rs:601-654` |
| 7 | **`load_live_keys_per_segment` is always empty**: it looks for ADD rows with `segment_id > 0`, but the write path never writes them (ADDs only in segment 0) → compaction, if it had run, would have **erased all data** | `usearch_engine.rs:664-702` |
| 8 | **`rebuild` leaves old DISK segments and wipes the whole WAL** → after a rebuild, search would have returned stale vectors as live | `usearch_engine.rs:472-510` |
| 9 | **Search: a SQL query to the WAL on every search** (a full DEL-row scan); the UPD flag is defined but never written or read anywhere; `search_threads` from the config is ignored | `usearch_engine.rs:321-367,749-779` |
| 10 | **Not a single test** of the WAL/compaction/search paths (commit 2063842 claims "5 compaction tests" — they are not in the tree) | `crates/vectors/` |
| 11 | **No crash recovery**: no WAL replay, no segment reconciliation on start, no segment-file naming/registry | — |
| 12 | **Doc comment is wrong**: `Index::restore` is **mmap read-write**, not "loads the file into memory". Verified experimentally: `add` through mmap `restore` **does not persist to the file** without an explicit `save()` (size after a re-restore = 0) | `usearch_engine.rs:20-25` |

Conclusion: the target architecture (RAM + DISK[n] + WAL + vacuum) does not exist in the current code; only WAL writes to segment 0 and a parallel merge over an empty segment list exist. We rewrite `UsearchEngine` wholesale, preserving the public `VectorIndex` trait contract and the WAL table schema.

## Key facts about usearch 2.26 (verified experimentally)

- `Index::restore(path)` — mmap **read-write**; `Index::restore_view(path)` — mmap **read-only** (view); `Index::restore_from_buffer(bytes)` — **an in-memory copy**, fully mutable.
- `add`/`remove` through mmap `restore` **do not persist to the file** without an explicit `save()` → persistence only via `save()`.
- `remove` on a read-only view silently returns `Ok(0)` (no error) — DISK segments are never mutated, which is safe.
- `filtered_search(query, k, |key| bool)` — the filter is called inside the HNSW walk (not a post-filter) ✓.
- `contains(key)` — O(1), works on a view ✓. `get(key)` — works on a view ✓.
- There is **no key-enumeration API** (no `exact_search` hacks; keys are stored in sidecar manifests, see below).
- `Index` methods take `&self` (the C++ core is concurrent) → no `Mutex` around the index is needed.

## Options

1. **A global cumulative WAL** (the current design.md change): PK `(segment_id, chunk_id)`, ADD/DEL/UPD, DISK[n] is filtered by rows with `segment_id <= n`, on flush — "rebinding" of segment 0 rows into the new segment, the key manifest — inside the WAL ADD rows themselves.
   *Rejected.* (a) the WAL grows over the whole corpus (1M+ ADD rows) and balloons with every insert; (b) the flush-time "rebinding" is fragile SQL dance logic with many crash cases; (c) cumulative `segment_id <= n` queries on the search path; (d) the UPD flag does not cover the re-insert case (an old copy in DISK[n] "comes back to life" because there is no DEL row); (e) a key manifest in the WAL requires `NOT IN` subqueries and does not survive partial crashes.
2. **Self-contained per-segment WAL (DEL only) + sidecar key manifests** — **accepted** (below).
3. **Per-segment binary WAL files** (instead of SQLite). *Rejected:* human decision — WAL in SQLite (transactional integrity with chunks, a single store, the migration already in place).
4. **Periodic full saves** (no segments). *Rejected:* O(N²) write amplification — already rejected in the proposal.
5. **mmap `restore` for the RAM tier** ("the file persists itself"). *Rejected:* the experiment showed that `add` through mmap does not reach the file — a false sense of persistence. RAM = `restore_from_buffer` (an in-memory copy) + explicit `save()`.

## Decision

### 1. On-disk layout

```
<vectors_path>/usearch/
├── ram.usearch              # RAM segment file (snapshot)
├── ram.keys                 # RAM key sidecar manifest
└── segments/
    ├── segment-1.usearch    # DISK[n], n grows monotonically, 1 = the earliest
    ├── segment-1.keys
    ├── segment-2.usearch
    └── ...
```

- **RAM**: `restore_from_buffer(ram.usearch)` — an in-memory copy, read-write. Persisted by explicit `save()`: (a) on `create` — an empty snapshot (the file header carries the dimension: an empty index is distinguishable by dim before the first insert — without a snapshot an 8-dim and a 4-dim index are byte-for-byte indistinguishable on disk and the dim check on `open` did not fire; clarification 2026-08-31, task 3.3 revision); (b) on flush — the content becomes the new DISK[n] file; (c) on graceful shutdown / `build_index()` — a snapshot of the current state.
- **DISK[n]**: `restore_view` — read-only mmap, zero-copy, page cache. Never mutated.
- **Sidecar `.keys`** (format): `magic u32 LE = 0x534B4559`, `count u32 LE`, `count × chunk_id u32 LE`. Written atomically (tmp + rename) next to the index file. The key manifest is moved out of the WAL into a sidecar — the WAL stays small (invalidations only), and key enumeration does not require `exact_search`.
- Naming is fixed: `segment-<n>.usearch`/`.keys`, n = decimal. The segment registry = a directory scan (the db crate's D3 protocol: self-recovery from files, no external registry).

### 2. Segment numbering

- `0` = RAM (the freshest tier). DISK: `1..N`, **monotonically increasing, no renumbering**: flush appends `N+1`; compaction creates new `N+1..N+M` (old ids disappear with the files). Higher id = fresher. Renumbering is rejected: it breaks the correspondence of WAL rows to files inside the crash window (see section 6).
- Merging results: a duplicate chunk_id from multiple tiers is resolved **in favor of the freshest tier** (RAM > higher id). Duplicates are possible only in a crash window (see invariant 3) — this is a read-path safeguard.

### 3. WAL: semantics

Table `usearch_vectors_log` (migrations 3+4) **with no schema change**: `PK (segment_id, chunk_id)`, `flags`, `created_at`. Semantics are fixed:

- **Only `flags = DEL (2)` is written** — "the key is invalid in this segment" (deleted **or** replaced by a newer version). `ADD (1)` and `UPD (4)` are reserved, not written (the manifest is in the sidecar).
- **Each segment is self-contained**: search on segment s filters only rows with `segment_id = s`. Cumulative "over previous WALs" queries are not needed: replacement/deletion **immediately** writes `(s, k, DEL)` to the WAL of every older segment containing k (invariant 3). The target architecture's wording "discarding those deleted or updated in previous WALs" is satisfied by this invariant.
- **Invariant 3 (atomicity of supersession):** one operation (insert/delete) writes all its DEL rows in **one SQLite transaction** (atomically), **before** the physical mutation of RAM (WAL-first). A crash between the transaction and the mutation → the key is temporarily invisible (self-healing: consumer reconciliation); "half the rows" is impossible.
- **Write rules:**
  - `insert(k)`: for every DISK segment with `contains(k)` — upsert `(s, k, DEL)`. (One transaction per batch.)
  - `delete(k)`: upsert `(0, k, DEL)` (the RAM snapshot file may contain k — see section 5) + for every DISK segment with `contains(k)` — upsert `(s, k, DEL)`. (One transaction.)
  - `flush`: `DELETE FROM usearch_vectors_log WHERE segment_id = 0` (all segment-0 rows become redundant: DEL rows are for keys physically absent from the flushed file; supersession of older segments is already recorded in their own rows).
- **In-memory stale-set cache:** `HashMap<segment_id, HashSet<u32>>` + an `AtomicU64` version. Reloaded with a single SQL select (`WHERE flags = 2`) only when the version changes (every WAL write goes through the engine → version bump). **In steady state the search path issues zero SQL.**

### 4. Write paths

- **`insert_batch(rows)`** (RAM): a WAL transaction of supersession rows → `add` to the RAM index → `ram_keys.insert` (an in-memory `HashSet`, needed for the sidecar and `chunk_ids`). After the batch: if `RAM.size() ≥ max_segment_vectors` → **flush** (section 5).
- **`delete_by_chunk_ids(ids)`**: a WAL transaction (`(0,k,DEL)` + supersession) → `remove` from RAM → `ram_keys.remove`. DISK files are not touched (read-only; `remove` on a view is silent — we do not call it).
- **`rebuild(rows)`** (ultimate repair): layout lock → clear the WAL entirely → delete all DISK files (recreate the `segments/` directory) → RAM = `rows` → `save()` ram + sidecar. A full state swap; no old tiers remain (current bug #8 is closed).
- **`build_index()`** (trait) = `save()` of the RAM snapshot + sidecar (the public persistence point; the consumer calls it on shutdown).

### 5. Flush (RAM → DISK[n])

Trigger: `RAM.size() ≥ max_segment_vectors` inside `insert_batch`. Procedure under the layout lock (serialized with compaction):

1. `n = max(id) + 1`;
2. `save()` the RAM index → `segments/segment-n.usearch` (tmp+rename) + sidecar `segment-n.keys` from `ram_keys`;
3. WAL transaction: `DELETE ... WHERE segment_id = 0`;
4. RAM: `reset()`, `ram_keys.clear()`, `save()` an empty `ram.usearch` + an empty sidecar;
5. bump the cache version.

**Loss window (honest semantics):** a WAL without vectors cannot replay inserts. A crash loses RAM inserts since the last flush/shutdown-save (≤ `max_segment_vectors`). The sidecar manifest + the chunks table let the consumer find the lost chunk_ids (SQLite − index) and re-ingest them; the ultimate repair is `rebuild`. This is a conscious "WAL without vectors" limitation, not a defect.

### 6. Search

1. Reload the stale-set cache if needed (the version changed);
2. Clone the `Arc<Index>` of the current DISK segments (a short read lock on `RwLock<Vec<DiskSegment>>`);
3. **In parallel** (a dedicated `rayon::ThreadPool` from `search_threads`) over `[RAM] + DISK[n]`: `filtered_search(query, k, |key| !stale[s].contains(key))`; if a segment's stale set is empty — a plain `search` without a filter (less overhead);
4. Merge: duplicate → the freshest tier; sort by distance; truncate to k.

`count()` and `chunk_ids()` — **without index scans**: `ram_keys − stale[0]` ∪ ⋃(`keys(n) − stale[n]`) over the sidecar manifests (the reconcile primitive for GC now sees all tiers; bug #5 is closed).

### 7. Compaction (vacuum)

- **Trigger** (`maybe_compact`, a new additive trait method with a default no-op — the only contract extension; all existing implementations keep compiling): `stale_total / total_disk * 100 > compaction_stale_threshold`, where `stale_total` = the number of DEL rows with `segment_id > 0` (from the cache), `total_disk` = Σ `size()` of the DISK files. The percentage is the semantics of the already-fixed config (1..=100).
- **Execution:** a background `std::thread` (fire-and-forget under an `AtomicBool` "compact in progress"); search continues on the old segments (Arc clones), pausing only at the moment of the atomic list swap.
- **Procedure** (under the layout lock):
  1. each segment's live keys = `keys(n) − stale[n]` (sidecar + cache, no scans);
  2. gather vectors via `get(key)` (in parallel across segments); dedup: the freshest tier wins;
  3. new indices in chunks of `max_segment_vectors` → `segments.tmp/segment-<N+i>.usearch` + `.keys` (**new ids**, section 2);
  4. directory swap: `segments → segments.old`, `segments.tmp → segments`, delete `segments.old` (a directory rename is atomic on one FS);
  5. WAL transaction: `DELETE ... WHERE segment_id > 0` (the new segments are fresh — no rows);
  6. swap the segment list (write lock), bump the version.
- **Crash matrix** (the "directory → WAL" order is mandatory):
  - before the directory swap: old files + `segments.tmp` garbage → on start delete `segments.tmp`;
  - after the swap, before the WAL DELETE: new files (ids N+1..N+M) + old rows (ids 1..N) → on start delete rows whose file does not exist (section 8). Monotonic ids rule out "an old row aliasing a new file".
  - the reverse order (WAL → directory) would let "old keys come back to life" — rejected.
- After a successful completion the DISK segments' WAL is clean (section 7.5) — consistent with the target architecture.

### 8. Start (`open`) and recovery

1. Remove garbage: `segments.tmp/`, `segments.old/`, `*.usearch.tmp`; the special case "`segments/` is missing but `segments.tmp/` exists" (a crash between the two renames) → `segments.tmp → segments`.
2. Scan `segments/`: `restore_view` every `segment-n.usearch` + read `segment-n.keys` (sidecar missing/corrupt → fallback: one-time enumeration via `exact_search` at start — a rare path).
3. RAM: `ram.usearch` → `restore_from_buffer` (no file → an empty index); `ram.keys` (same fallback).
4. **WAL reconciliation:** `DELETE FROM usearch_vectors_log WHERE segment_id != 0 AND segment_id NOT IN <existing files>`.
5. Check the dim of every file (mismatch → `DimensionMismatch`, as now).
6. Load the stale-set cache.

### 9. Wiring (closing hole #2)

- The `create_vector_engine` factory receives the WAL: a new signature with `wal_db: Option<&Path>` (the path to knowledge.db) — the engine opens its **dedicated** long-lived `rusqlite::Connection` itself (SQLite's WAL mode allows it; the db crate's pool is not needed for this). `VectorIndexConfig.usearch` is finally passed to the engine (previously ignored).
- `search_threads` → the engine's dedicated `rayon::ThreadPool` (the previous global pool + ignoring the config — bug #9).
- Consumer integration (cli/ingestion): `maybe_compact()` — after an ingestion batch (in the cleanup phase next to `reconcile_vectors`); `build_index()` — on serve graceful shutdown.

### 10. Config and schema

- The `vectors.usearch` section is **unchanged** (`max_segment_vectors`, `compaction_stale_threshold`, `search_threads`) — all three fields now get a real meaning.
- The WAL table schema is **unchanged** (migrations 3+4 stay; migration 5 is not needed). The db crate's **tests** are fixed (they expect `user_version = 4`; the insert test gets `segment_id`) — defect #3.

## Rejected alternatives

- **A cumulative WAL + rebinding on flush** (option 1) — see above.
- **The UPD flag as a separate state** — not needed: "replaced" ≡ "invalid in this segment" = DEL; a separate flag only complicates the queries (the current code defined UPD and never uses it — bug #9).
- **A key manifest in the WAL (ADD rows)** — balloons the WAL to the corpus size and breaks the crash semantics (bug #7: a query over ADD rows is always empty).
- **Renumbering segments on compaction** — WAL-row aliasing in the crash window; monotonic ids solve it for free.
- **`&mut self` for compaction** — incompatible with `Arc<dyn VectorIndex>` (bug #4); internal synchronization (`RwLock`/layout lock) + a background thread.
- **mmap `restore` for RAM** — `add` does not persist (verified experimentally).
- **A full RAM save on every batch** — O(N²) write amplification.

## Open questions / residual risks

1. **RAM loss window** (section 5): a crash loses up to `max_segment_vectors` inserts. Mitigation: consumer reconciliation SQLite−index + re-ingest / `rebuild`. Extending `reconcile_vectors` to "missing vectors" — a separate ingestion-crate task (not in this ADR).
2. **Default `max_segment_vectors = 1M`**: the RAM tier ≈ 2–2.5 GB (bf16 + HNSW graph) on a 16 GB laptop — tight. It is configurable; if needed the default is lowered by a separate decision (a config change — a frozen contract).
3. **Recall under filtering:** `filtered_search` excludes stale keys inside the walk, but HNSW navigation over a "holey" graph theoretically loses recall at a high stale fraction — compaction (30% threshold) keeps the fraction in check; the recall@k/p95 gates are checked by the parity harness after implementation.
4. **`remove` on a view is silent (`Ok(0)`)**: the safeguard — DISK segments are not mutated by construction; an assert test pins the behavior.

## Implementation plan (tasks.md update for the change)

1. Fix the db tests (user_version 4, segment_id) — unblock a green workspace.
2. `UsearchEngine` core: layout (ram/segments), the sidecar codec, `create`/`open` + recovery (section 8).
3. WAL write paths: transactional DEL rows, the stale cache + version (sections 3–4).
4. Search: the pool, per-segment filtering, "freshest wins" merge; `count`/`chunk_ids` over the manifests (section 6).
5. Flush + layout lock (section 5).
6. Compaction: `maybe_compact` (an additive trait method), a background thread, monotonic ids, crash safety (section 7).
7. Wiring: the factory (+WAL path, +UsearchConfig), bootstrap, the shutdown save, the `maybe_compact` call in ingestion cleanup.
8. Tests: unit (sidecar, WAL invariants, flush, compaction, crash scenarios by file manipulation) + integration (restart, search correctness, concurrency) + fixes to layout-dependent tests (the factory in `lib.rs`, cli fixtures).
