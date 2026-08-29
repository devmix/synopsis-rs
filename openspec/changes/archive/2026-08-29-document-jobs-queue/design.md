# Design: Unified document-jobs queue

## Architecture

```
producers (source-aware, ONLY directory readers):
  file watcher  ─┐
  startup scan  ─┼─► walk source tree, enumerate file paths,
  CLI index     ─┘    compute content_hash (NO Document built),
                     write/enqueue rows into document_jobs
                         │  (decoupled: NO in-memory handoff to worker)
                         ▼
document_jobs (knowledge DB) — sole shared state
   path PK, source_path, op, status, content_hash, attempts,
   max_attempts, last_error, next_attempt_at, created_at, updated_at
                         │  status: pending | processing | done | error | delete
                         ▼
background worker (serve owner thread, polls due rows):
   for each due job (by path):  read ONE file via Parser::parse_file(path)
     → Document → chunk → NER → embed → link → store   (sequential, Phase 1)
   after draining all due jobs in the cycle → GC (orphan cleanup)
                         │
   CLI: index status / index reset-retries  ── read / mutate document_jobs
```

`Runner` is `!Send + !Sync` (owns `Box<dyn Source>`), so the worker runs on the serve
owner thread — exactly like today's `IngestChangeHandler`. The worker reads a SINGLE file
by path (lookup parser via `find_source_for_path`); it does NOT walk any source directory
and does not know about source-tree structure. `Runner::process_document_at` (whole-tree
parse) is REMOVED; the worker uses `Parser::parse_file` (single-file read) instead. The
heavy collaborators (`EmbeddingProvider`, `VectorIndex`, `NerProvider` — all `: Send + Sync`)
are captured by reference only inside the (future) parallel stage; Phase 1 keeps the
pipeline sequential and reuses the existing `Ingester::process_document`.

Producer and worker are FULLY DECOUPLED: all state flows through `document_jobs`; there is
no direct data transfer (no `Document` passed in memory) between them. The producer only
writes the table; the worker only reads due rows and reads the referenced file itself.

## State machine (document_jobs)

- `enqueue_index(path, source, hash)` → `INSERT OR REPLACE` row as `pending`,
  `next_attempt_at = now`, `attempts = 0` (idempotent: one pending job per path).
- `enqueue_delete(path)` → upsert row as `pending` with op=`delete`.
- worker picks due: `UPDATE ... SET status='processing' WHERE status='pending'
  AND next_attempt_at <= ? ORDER BY next_attempt_at LIMIT batch` (atomic claim).
- on success: `delete` op → remove document + chunks + entities, delete job row;
  `index` op → mark `done`.
- on failure: `attempts += 1`; if `attempts < max_attempts` → `pending` +
  `next_attempt_at = now + backoff(attempts)`; else → `error` (last_error recorded).
- `reset_retries(path)` (CLI): `error` → `pending`, `attempts = 0`,
  `next_attempt_at = now`.
- index on `(status, next_attempt_at)` for the due-query.

Backoff (recommended): exponential base 30s → 30s / 60s / 120s for attempts 1→2→3
(`max_attempts` default 3). After the 3rd failure → `error`.

## Task breakdown (each ≤ ~500 LOC)

1. **1.1** Migration `2-document-jobs/up.sql` + `DocumentJobDao` + state-machine helpers.
2. **1.2** Config: `ingestion.max_retries` (3), `auto_update.retry_failed`.
3. **1.3** Producer API (`enqueue_index` / `enqueue_delete` / `reconcile_source`): enumerates source file paths + content_hash, writes `document_jobs` rows. Does NOT parse content or build `Document`.
4. **1.4** Background worker: claims due rows, reads the single file via `Parser::parse_file`, runs chunk→NER→embed→link→store (reuses `Ingester::process_document`), updates row state; GC after cycle. `Runner::process_document_at` removed.
5. **1.5** Watcher → producer; startup reconcile replaces initial sync.
6. **1.6** Owner-loop wiring (spawn worker, `RetryBatch` arm; drop standalone orphan scheduler).
7. **1.7** CLI `index status` / `index reset-retries`.
8. **1.8** Tests (DAO, state machine, worker+retry+GC, watcher-producer, CLI).

## Frozen-contract deltas (approved 2026-08-29)

- **cli-surface:** ADDED `index` subcommand (`status`, `reset-retries`).
- **config-format:** MODIFIED preset — `ingestion.max_retries`; `auto_update.retry_failed`.
- **data-schema:** ADDED `document_jobs` table (migration `2-document-jobs`).

## Risks & mitigations

- **Write contention** on `document_jobs` → SQLite WAL (already on, design D8); idempotent
  `INSERT OR REPLACE`; single writer per path.
- **Retry storm after restart** → staggered exponential backoff + per-path dedup.
- **Double-processing** → atomic `processing` claim (`UPDATE ... WHERE status='pending'`
  returning the row); a `processing` row older than a timeout is reset to `pending` at
  startup.
- **`!Send` Runner** → worker on owner thread (Phase 1 sequential); parallelism deferred.

## Future work (separate change)

- Parallel pipeline: run embedding + NER in `spawn_blocking` (collaborators are
  `Send + Sync`), converge linking on the owner thread. Extract the chunker from
  `Box<dyn Source>` so parsing can also move off-thread.
- True multi-worker pool (Option B) only if laptop throughput demands it.

## Correction (2026-08-29, post-1.4-review)

The first 1.4 implementation had `Runner::process_document_at` re-parse the WHOLE source
tree on every job (`enriched.parse(src.path)` + `.find()`). That was rejected: the worker
must not read source directories. Fix — the worker reads ONE file by path via the new
`Parser::parse_file(path, root)` (markdown already had `read_file`; mediawiki/webpage/
unstructured/json gain an equivalent per-file read). The producer no longer builds
`Document`s either: `reconcile_source` enumerates paths + hashes via `walk_matched_files`
+ `compute_content_hash`, writing only `document_jobs` rows. Producer and worker share
state exclusively through the table.
