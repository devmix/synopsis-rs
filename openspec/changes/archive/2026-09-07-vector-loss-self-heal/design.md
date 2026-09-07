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
detects chunk rows with no corresponding vector and enqueues a `doc:index`
task carrying the `ReEmbed` op (D5) for their documents.

**Why.** This repairs the residual loss (a mid-cycle `SIGKILL` between two
saves) automatically, without a manual `rebuild`. It implements the
"consumer reconciliation" repair the `vector-index` spec already names. The
affected set is small because of D1.

**Why the repair is a targeted re-embed, not a full re-index.** The pipeline
dedups on content hash (`ingester/mod.rs`): a document whose stored hash
matches the file is **skipped entirely** — no re-embed. So a plain `doc:index`
for an unchanged document (the exact SIGKILL residual: content unchanged,
vectors lost) is a no-op. The self-heal therefore carries an explicit `ReEmbed`
op (D5) that re-embeds the existing chunk rows without re-parsing,
re-chunking, or re-running NER. A vector-aware dedup (re-checking
`vectors.chunk_ids()` for every document) was rejected: it adds work to the
common unchanged-document path for documents that are already fully
vectorized.

**Why not the alternatives.**
- *Vector-aware dedup (re-index only if some chunk vector is missing)* —
  rejected: it runs the `chunk_ids()` set-diff on **every** `doc:index`
  (including already-vectorized documents), which is the extra work the
  self-heal should avoid. The targeted `ReEmbed` op does the work only for the
  documents the self-heal already knows are missing vectors.
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
`original_path`, payload `DocIndexPayload { source_path, content_hash: None,
ops: [ReEmbed] }` (D5).

**Why.** `content_hash: None` is safe: the re-embed path does not re-hash the
file. Reusing the `doc:index` task type (no new queue row type) keeps the
self-heal on the existing queue (upsert/backoff/status semantics) and the
`queue status` CLI surface unchanged.

## D5 — Granular re-embed op (re-embed without the full pipeline)

**Decision.** `DocIndexPayload` gains `ops: Vec<ReIndexOp>` (serde default
`[Full]`). `ReIndexOp` is an enum: `Full` (the current full pipeline: parse →
chunk → NER → embed → write, with the content-hash dedup) and `ReEmbed`
(re-embed the document's **existing** chunk rows only). The worker dispatches
`doc:index` by op: `Full` (or default) → `process_document_by_path`; `ReEmbed`
→ a new `Runner::reembed_document`. `reembed_document` reads the document's
chunk rows from SQLite (they already carry `search_text`), calls the embedding
provider on them, and `insert_batch`es the vectors — no parse, no re-chunk, no
NER, no dedup. Present vectors are overwritten (idempotent).

**Why.** When only the vectors are lost (the SIGKILL residual), the chunk rows
are intact, so re-running parse/chunk/NER is pure waste. A targeted re-embed
does exactly the missing work. The op is a **set** (not a single boolean) so
future granular operations (e.g. re-run NER only) extend it without a payload
schema break. `Full` subsumes `ReEmbed`; the dispatch prefers `Full` when both
are present.

**Why not the alternatives.**
- *A single `force: bool`* — rejected: it forces the **full** pipeline (parse +
  chunk + NER + embed), which re-does work the intact chunk rows make
  unnecessary. The op set lets the self-heal request the minimal work.
- *A new `doc:reembed` queue task type* — rejected: it is a data-schema
  surface change (new `type` value, `QueueTaskType` variant, `queue status`
  output) for what is just a `doc:index` with a narrower payload. The op set
  keeps the queue schema and CLI unchanged.

## Performance (N≈1M × 1024-dim)

- Startup self-heal: `SELECT id, doc_id` ≈ 50–150 ms, `chunk_ids()` (RAM +
  disk segment manifests) ≈ 50–200 ms, in-Rust set-diff ≈ 20–50 ms →
  **~0.1–0.4 s one-time**, off the query path. The same order of work the GC
  already does each cycle.
- Per-cycle save: one `ram.usearch` write per work cycle (≤ once per ~60 s /
  watcher batch); bounded by the RAM layer size, small in the common case.
