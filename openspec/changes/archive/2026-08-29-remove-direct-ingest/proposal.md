# Change: remove-direct-ingest

## Why

The `document_jobs` queue (change `document-jobs-queue`, archived 2026-08-29) is now
the single unified mechanism for document processing: producers (watcher, startup
reconcile) enqueue; a background `DocumentWorker` claims due rows and runs the
per-document pipeline via `Runner::process_document_by_path`. However, a legacy
direct-ingest path survived: `Runner::ingest_all` (and friends `ingest_source`,
`sync_source`, `ingest_source_by_path`, `prune_deleted`) still parse + ingest a
whole source tree inline, bypassing the queue. They are called by the `sync` CLI
subcommand, `bootstrap::initial_sync`, and the `force_rebuild` branch in
`serve/server.rs`.

This dual path is an architectural inconsistency: the queue is supposed to be the
ONLY processing path. The user decided (2026-08-29) to delete all direct
document-processing calls and their tests, leaving only background queue
processing. A new `db clear` CLI command is added so the user can still wipe a
dataset's knowledge DB (with a confirmation prompt showing stats) — which, combined
with a serve restart (startup reconcile re-enqueues everything), replaces the old
`sync` / `force_rebuild` full-re-ingest flows through the queue.

## What Changes

- **CLI surface (frozen contract change):** remove the `sync` subcommand; add a
  `db` subcommand with a single `db clear` action. `db clear` prints dataset/DB
  statistics, asks for confirmation (y/N), and on confirm deletes all rows from the
  knowledge DB (documents, chunks, entities, entity_links, facts, vectors,
  document_jobs).
- **Ingestion crate:** delete `Runner::ingest_all`, `ingest_source`, `sync_source`,
  `ingest_source_by_path`, `prune_deleted`. Keep `process_document_by_path`,
  `delete_document_at`, `cleanup_orphaned_data` (the worker uses the last one for
  GC).
- **CLI callers:** delete the `sync` subcommand module; delete the
  `crates/cli/src/serve/ingest.rs` wrapper module; delete `bootstrap::initial_sync`;
  replace the `force_rebuild` direct `ingest_all` branch with a clear-dataset +
  startup-reconcile flow (clear then the existing reconcile enqueues everything, so
  the worker re-embeds — no direct ingest).
- **Tests:** rewrite `pipeline_e2e.rs` and the `runner` unit tests that exercised the
  removed methods to go through the queue/worker (`DocumentJobQueue` +
  `DocumentWorker::run_once`); delete the `prune_deleted` test.

## Impact

- Breaking CLI change: `synopsis sync` is removed. Re-ingest is now
  `synopsis db clear` + restart `serve` (startup reconcile re-enqueues). This is a
  deliberate deviation from the Go oracle (which has `sync`); documented below.
- No data-schema change (the `document_jobs` table already exists).
- No new dependencies.

## Non-goals

- Parallelizing the worker (deferred; it still runs inline on the serve owner thread).
- Adding a `rebuild` flag to the queue (replaced by clear + reconcile).
- Changing the `queue` subcommand (already shipped in `document-jobs-queue`).

## Decision Rationale

Options considered:

- **A (chosen): delete all direct ingest; add `db clear`.** Queue is the only
  processing path. `db clear` + restart covers full re-ingest. Simplest mental
  model, no schema change, no direct-ingest code to maintain.
- B: keep `sync` but reimplement it as a producer-only enqueue (`reconcile_source`).
  Rejected: the user explicitly wants only background queue processing; `db clear`
  already provides the wipe, and startup reconcile provides the re-enqueue.
- C: keep a narrow direct `force_rebuild` for vector-engine recovery. Rejected: the
  user wants no direct processing; clear + reconcile achieves the same through the
  queue.

Oracle reference: `../synopsis/cmd/app/sync.go` (the `sync` command we are removing)
and `../synopsis/cmd/app/serve.go` (the `ReEmbedChunks`/rebuild path, now expressed
as clear + reconcile).
