# Proposal: bound the vector loss window + self-heal lost vectors

## Why

Semantic search goes dead after an unclean shutdown. The chunk rows are
durable (SQLite), but the chunk vectors live in the `vectors` RAM layer and
are only written to disk on a **graceful** shutdown (`server.rs` calls
`vectors.build_index()` in the shutdown path) or on a RAM flush. A `SIGKILL`
(IDE "stop", `kill -9`, power loss) bypasses the shutdown handler, so the
RAM layer is lost. On the next start the content-hash reconcile
(`reconcile_source`) sees each document as **unchanged** and enqueues nothing
— the vectors are never re-embedded. Result: FTS5 (SQLite) still works, but
semantic (vector) search returns nothing. The `vector-index` spec already
documents this as "a documented window, repair — consumer reconciliation or
`rebuild`", but the consumer reconciliation for **missing** vectors was never
implemented (only the reverse direction — orphan *vectors* without chunks —
is, in `reconcile_vectors`).

This change closes the gap in two parts:

1. **Bound the window** — persist the vector RAM layer after each worker
   cycle that indexed or deleted documents, so a `SIGKILL` loses at most the
   in-progress batch instead of everything since the last flush.
2. **Self-heal the rest** — at serve startup, detect chunk rows that have no
   corresponding vector and re-embed them through the existing `doc:index`
   path, so the residual loss (a mid-cycle `SIGKILL`) is repaired
   automatically without a manual `rebuild`.

## What Changes

- **Per-cycle vector persistence (worker/serve policy):** after a
  `DocumentWorker` cycle that processed one or more `doc:*` tasks, the runner
  calls `vectors.build_index()` (the existing save point, a no-op when the RAM
  layer is empty). A cycle that did no document work persists nothing. A
  persistence failure is logged and does not abort the cycle.
- **Startup vector self-heal (serve startup):** before the worker's startup
  drain, the runner detects chunk rows without a vector (a light
  `SELECT id, doc_id FROM chunks` set-diffed against `vectors.chunk_ids()`),
  groups them by document, and enqueues one `doc:index` per affected document
  (`content_hash: None` — the worker re-hashes the file). No missing chunks →
  a no-op. The re-embedded vectors are produced by the ordinary pipeline
  (re-indexing the whole document is idempotent and the residual set is small
  because of part 1).

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `pipeline`: the `Vectors outside the transaction` requirement is
  re-specified as a **bidirectional** reconciliation — orphan vectors are
  still deleted by the orphan cleanup, and missing vectors are restored by a
  startup self-heal. A new `Per-cycle vector persistence` requirement pins
  the save-after-work-cycle behavior.

## Impact

- `crates/ingestion` — `runner/mod.rs` (new `persist_vectors` +
  `heal_missing_vectors`), `worker.rs` (save call after a work cycle).
- `crates/db` — `chunk.rs` (new light `ChunkDao::list_id_doc_id` accessor).
- `crates/cli` — `serve/server.rs` (startup self-heal call before the worker
  drain).
- `crates/vectors` — **unchanged** (`build_index` / `chunk_ids` already exist
  and are reused as-is).
- No new dependencies; no MCP tool, CLI, config-format, or data-schema
  changes.

## Frozen contracts touched

(none — no MCP tool, CLI subcommand, data schema, or config format changes.
The `vector-index` and `pipeline` per-module specs are updated; these are
behavioral contracts, not the four frozen ones.)

## Non-goals

- No change to the `vectors` engine (no new WAL journaling for RAM inserts —
  the WAL is by design a disk-segment supersession log, `wal.rs:111–116`; RAM
  is recovered via the snapshot + this self-heal).
- No new CLI subcommand or flag (no manual "re-embed" trigger — `rebuild`
  already exists for the full case; this is automatic startup repair).
- No change to the content-hash reconcile, the queue schema, the backoff
  schedule, the flush-on-overflow, or the compaction policy.
- No migration or data repair for existing dataset DBs — the self-heal runs
  on the next `serve` start and fixes whatever is missing.
- No sub-chunk-level re-embedding — a document with any missing chunk is
  re-indexed whole (idempotent; the residual set is small per part 1).
