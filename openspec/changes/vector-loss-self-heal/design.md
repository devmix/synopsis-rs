# Design: vector-loss-self-heal

Reference contracts: `openspec/specs/pipeline/spec.md` (`Vectors outside the
transaction`), `openspec/specs/vector-index/spec.md` (`Index lifecycle` →
"Persistence across restart"), ADR 0004 (two-layer RAM/DISK + per-segment
WAL). The root cause is confirmed: a `SIGKILL` loses the RAM layer, the
content-hash reconcile does not re-embed unchanged documents, and only the
orphan-*vector* direction of reconciliation is implemented.

## D1 — Bound the loss window with a per-cycle RAM save

**Decision.** After a `DocumentWorker` cycle that processed ≥ 1 `doc:*` task,
the runner persists the vector RAM layer via `vectors.build_index()`.

**Why.** `build_index` is the existing save point (writes `ram.usearch` +
`ram.keys`, a no-op when the RAM layer is empty — `usearch/mod.rs:589`).
Calling it after a work cycle means a `SIGKILL` loses at most the in-progress
batch (≤ the per-cycle cap of 100 tasks), not everything since the last
flush. The save is a bounded disk write (the RAM snapshot), cheap on a 16 GB
laptop and at most once per poll tick (~60 s) or on-demand after a watcher
batch.

**Why not the alternatives.**
- *Journal RAM inserts in the WAL* — rejected: the WAL is by design a
  disk-segment **supersession** log (`wal.rs:111–116`); a `(0, key, DEL)` row
  for a RAM-only key would hide the fresh vector from search until the next
  flush. Changing the WAL contract is out of scope and riskier.
- *Save on every `run_once` (including idle cycles)* — rejected: an idle
  cycle changed nothing, so the extra disk write every ~60 s is pure I/O. The
  GC's orphan-vector deletions self-heal on the next startup via the existing
  `reconcile_vectors`, so they do not need a save either.
- *Save only on shutdown (status quo)* — rejected: that is the bug;
  `SIGKILL` never reaches the shutdown handler.

**Where.** `Runner::persist_vectors()` (new, `runner/mod.rs`) wraps
`self.vectors.build_index()`; `DocumentWorker::run_once` calls it when
`processed > 0`, after the GC phase, logging (not propagating) a failure.

## D2 — Self-heal missing vectors at startup

**Decision.** At serve startup, before the worker's startup drain, the runner
detects chunk rows with no corresponding vector and enqueues `doc:index` for
their documents.

**Why.** This repairs the residual loss (a mid-cycle `SIGKILL` between two
saves) automatically, without a manual `rebuild`. It implements the
"consumer reconciliation" repair the `vector-index` spec already names.
Re-indexing the whole document is idempotent (the pipeline clears and
rewrites the document's chunks + vectors), and the affected set is small
because of D1.

**Why not the alternatives.**
- *Sub-chunk-level re-embed (only the missing chunks)* — rejected: the
  pipeline re-indexes whole documents; a partial re-embed would need a new
  pipeline path and would still re-create chunk rows (new ids), so it buys
  little over whole-document re-index for the small residual set.
- *Continuous self-heal in every GC cycle* — rejected: during a live session
  the in-memory index already holds every vector (RAM + disk), so
  `chunks − vectors` is empty and the set-diff is wasted work every cycle.
  The missing vectors only exist **after a restart** (the RAM was lost), so a
  one-time startup pass is sufficient and cheaper.
- *A manual `re-embed` CLI command* — rejected: a new CLI surface is a frozen
  contract change; automatic startup repair is strictly better for the
  personal-use target.

**Where.** `Runner::heal_missing_vectors(now)` (new, `runner/mod.rs`);
called from `serve/server.rs` after the startup reconcile and before
`worker.run_once(now)` (the startup drain), logging the re-queued count.

## D3 — Light chunk accessor for the set-diff

**Decision.** Add `ChunkDao::list_id_doc_id() -> Vec<(i64, i64)>` (chunk id,
doc id) — a `SELECT id, doc_id FROM chunks` that loads no text.

**Why.** The self-heal needs only `(chunk_id, doc_id)` to compute the set
against `vectors.chunk_ids()` and to group by document. At N≈1M the full
`ChunkDao::list_all()` (already used by `reconcile_vectors`) would pull every
chunk's text into memory (hundreds of MB–GB) — unacceptable on a 16 GB laptop
for a startup path. The light query is O(rows) with a tiny per-row footprint.
`reconcile_vectors` is left as-is (out of scope; its `list_all` is a
pre-existing cost, and it runs in the GC, not on the query path).

**Why not the alternatives.**
- *Reuse `list_all`* — rejected: the text payload makes it memory-heavy at
  scale (see above).
- *A SQL `NOT IN` join against the index* — rejected: the index is not
  SQLite; the set-diff must happen in Rust against `vectors.chunk_ids()`.

## D4 — Enqueue semantics for the self-heal

**Decision.** One `doc:index` per affected document, identity = the document's
`original_path`, payload `DocIndexPayload { source_path, content_hash: None }`.

**Why.** `content_hash: None` is safe: the worker's `process_document_by_path`
re-reads and re-hashes the file (the payload hash is only used by the
producer's content-hash diff, not by the worker). Reusing the ordinary
`doc:index` path keeps the self-heal on the well-tested pipeline (no new
entry point) and inherits the queue's upsert/backoff/status semantics.

## Performance (N≈1M × 1024-dim)

- Startup self-heal: `SELECT id, doc_id` ≈ 50–150 ms, `chunk_ids()` (RAM +
  disk segment manifests) ≈ 50–200 ms, in-Rust set-diff ≈ 20–50 ms →
  **~0.1–0.4 s one-time**, off the query path. The same order of work the GC
  already does each cycle.
- Per-cycle save: one `ram.usearch` write per work cycle (≤ once per ~60 s /
  watcher batch); bounded by the RAM layer size, small in the common case.
