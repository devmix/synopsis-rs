# Proposal: event-based `queue_tasks` + incremental entity linking

## Why

Cross-domain entity linking is implemented (`graph::build_entity_links`,
`Runner::build_entity_links`) but **never executes in production**: the queue
worker (`DocumentWorker::run_once`) only calls `cleanup_orphaned_data()` after
a batch, and `git log -S` confirms the linking call was never wired (the Go
oracle had it in `ingest_all`, dropped by the queue-only model). Even if
wired naively, the linker always does a **full rebuild** (recorded YAGNI
deviation, `linker.rs:47`), re-processing every entity on every run — wrong
for a document update that touches 10 of 1000 entities.

This change replaces the document-only `document_jobs` queue with a generic
event queue (`queue_tasks`) and drives **incremental** linking through
`entity:link` events emitted during document indexing.

## What Changes

- **BREAKING (data schema)**: the `document_jobs` table is replaced by
  `queue_tasks` — a generic event queue with `type` (`doc:index` | `doc:delete`
  | `entity:link`), `identity` (path for doc events, doc id for link events),
  `event` (JSON payload), and the existing status/backoff columns. The table
  is defined directly in the init migration `migrations/knowledge/1-init/up.sql`
  (human decision 2026-09-06: the project has no deployed instances, so no
  separate forward migration and no data migration; `PRAGMA user_version`
  stays 1).
- **Worker event model**: the queue producer (watcher, startup reconcile)
  enqueues `doc:index` / `doc:delete` events; the worker processes all event
  types sequentially from one queue. Upsert semantics per type: doc events
  replace (latest state wins), `entity:link` **merges** (`entity_ids` union)
  so pending candidates are never lost; a re-enqueued event moves to the END
  of the queue (`next_attempt_at := now`).
- **Incremental linking**: during document indexing the entity resolver
  reports the ids it **created or updated**; the runner enqueues one
  `entity:link` event per document (identity = doc id) carrying exactly those
  ids (no event when the set is empty). The worker processes it with a new
  incremental entry in `graph` — the candidates from the event are linked
  against the remaining cross-domain entities (equals/expression/llm methods,
  idempotent inserts, LLM decision cache unchanged). Per-pair errors are
  logged and skipped; a failed task retries with backoff and lands in
  `error` status like document jobs.
- **Cascade delete**: processing a `doc:delete` removes the document's
  `entity:link` queue row.
- **CLI (frozen contract)**: `queue status` gains the event `type` column and
  filters by `identity`; `queue reset-retries` and `db stats` work for all
  event types.
- **Full rebuild retained**: `graph::build_entity_links` (full rebuild) stays
  implemented and tested but is NOT wired into the worker — the worker links
  only via events.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `data-schema`: the `document_jobs table (indexing queue)` requirement is
  replaced by a `queue_tasks table (event queue)` requirement — new columns
  (`type`, `identity`, `event`), new unique index, same status lifecycle and
  backoff semantics.
- `cli-surface`: the `queue subcommand` requirement changes — status output
  columns and filters cover mixed event types; reset-retries applies to all
  types; `db stats` counts queue tasks.
- `pipeline`: the `Post-processing: linking` requirement changes — linking is
  event-driven and incremental (created/updated entities per document),
  errors skip-and-log, cascade delete of link tasks on document deletion.
- `knowledge-graph`: the `Cross-domain linking pipeline` requirement gains the
  incremental candidate mode (link a candidate entity set against the rest).

## Impact

- `migrations/knowledge/1-init/up.sql` — table replacement (init migration
  edited per the human decision above).
- `crates/db` — `DocumentJobDao` → `QueueTaskDao` (event model, upsert with
  per-type merge, claim_due ordering, reset_retries, status_counts, delete by
  doc id); `db stats` plumbing.
- `crates/ingestion` — `job_queue.rs` (producer), `worker.rs` (event
  dispatch), `runner/` (resolver created/updated report, `entity:link`
  emission, cascade delete), resolver API change.
- `crates/graph` — incremental entry for the linker (candidate set
  parameter); full rebuild unchanged.
- `crates/cli` — `queue status` / `queue reset-retries` / `db stats` output
  for the event queue.
- No new dependencies; no MCP tool or config-format changes.

## Frozen contracts touched

- **data-schema** — `document_jobs` → `queue_tasks` (table rename + schema
  change). Justification: human decision 2026-09-06 — a unified event queue
  is required for incremental linking with per-task status/retry visibility;
  no deployed instances exist, so the init migration is edited directly.
  Parity: the queue is internal state; no recorded fixture references
  `document_jobs` rows.
- **cli-surface** — `queue status` columns/filters and `db stats` counts
  change shape. Justification: same human decision; the `queue` subcommand
  surface (name, flags, actions) is preserved, only the output columns adapt
  to event types.
- **pipeline / knowledge-graph** — linking requirement re-specified from
  "post-ingestion full rebuild with app_kv window" to "event-driven
  incremental linking". Justification: the old requirement was never
  implemented; the event model is its correct implementation.

## Non-goals

- No new CLI subcommand for on-demand full linking (a separate contract
  decision if ever wanted).
- No change to the linking methods (equals/expression/llm), their config, the
  LLM decision cache, or the `llm_linker_cache` / `last_linking_run` app_kv
  keys.
- No change to MCP tools, config formats, embedding, or vector storage.
- No `updated_at` column on `entities` — incrementality comes from the
  resolver's per-run created/updated report, not from entity timestamps.
- No migration of existing dataset DBs (none exist in use; a stale
  `knowledge.db` with `document_jobs` is simply rebuilt by clearing the
  dataset state).
