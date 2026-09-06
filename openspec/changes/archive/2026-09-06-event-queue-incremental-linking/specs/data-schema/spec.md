# data-schema Specification

## RENAMED Requirements

- FROM: `document_jobs table (indexing queue)`
- TO: `queue_tasks table (event queue)`

## MODIFIED Requirements

### Requirement: queue_tasks table (event queue)

The Rust binary SHALL keep a generic event queue in the `queue_tasks` table (knowledge DB), created by the consolidated init migration `migrations/knowledge/1-init/up.sql`. The `document_jobs` table no longer exists (human decision 2026-09-06: the project has no deployed instances, so the table is defined directly in the init migration instead of a separate forward migration; `PRAGMA user_version` stays 1). The queue is the single state machine for the watcher, the startup scan, and the background worker, covering document operations and entity-linking tasks.

Columns: `id INTEGER PRIMARY KEY AUTOINCREMENT`, `type TEXT NOT NULL` (`doc:index` | `doc:delete` | `entity:link`), `identity TEXT NOT NULL` (the document path for `doc:*` events; the document id as a decimal string for `entity:link`), `event TEXT NOT NULL` (JSON payload — `source_path` and `content_hash` for `doc:index`, `source_path` for `doc:delete`, an `entity_ids` array for `entity:link`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER NOT NULL`, `updated_at INTEGER NOT NULL`. Indexes: `idx_queue_tasks_due (status, next_attempt_at)` for due queries; unique `idx_queue_tasks_identity (type, identity)` — at most one row per event identity.

Enqueue is an upsert on `(type, identity)`: an existing row is reset to `pending` with `attempts=0` and `next_attempt_at` set to the current time — a re-enqueued event moves to the END of the claim order — and its payload is updated. Payload update semantics depend on the type: `doc:index` and `doc:delete` REPLACE the payload (the file on disk is the source of truth); `entity:link` MERGES the `entity_ids` arrays (union) when the existing row is `pending`, `processing`, or `error`, and stores only the new ids when the existing row is `done` (the old ids were already linked).

Claim order is `(next_attempt_at, id)`; the backoff schedule is `30 * 2^(attempts-1)` seconds and a task that fails `max_attempts` times lands in `error` status. The worker claims tasks ONE AT A TIME: a claim flips a single due `pending` row to `processing`, the worker processes it, then claims the next — so `processing` means "currently executing" (at most one row at a time), and a per-cycle cap (100 tasks) bounds one worker cycle so the owner thread does not starve the HTTP server. On serve startup, before the startup reconcile, rows left in `processing` by an unclean shutdown (crash, SIGKILL, power loss) are reset to `pending` with `attempts` and `last_error` preserved — an interrupted attempt is not a failed one.

#### Scenario: Migration application
- **WHEN** the binary starts on a fresh knowledge.db
- **THEN** the init migration `1-init` creates `queue_tasks` with both indexes once; the `document_jobs` table does not exist

#### Scenario: Task state
- **WHEN** a task has not succeeded after `max_attempts` attempts
- **THEN** the row has `status='error'`, `attempts=max_attempts`, and `last_error` is populated; `queue reset-retries` moves it to `pending` with `attempts=0`

#### Scenario: Re-enqueue moves to the end
- **WHEN** an event with the same `(type, identity)` is enqueued while older pending tasks exist
- **THEN** the existing row is reset to `pending` with `next_attempt_at` = now and is claimed after the older pending tasks

#### Scenario: Link-event merge
- **WHEN** an `entity:link` event is enqueued for a document whose `entity:link` row is `pending` with ids A
- **THEN** the row's `entity_ids` becomes the union of A and the new ids; no pending candidate is lost

#### Scenario: Doc-event replace
- **WHEN** a `doc:index` event is enqueued for a path whose row already exists
- **THEN** the payload is replaced with the new `source_path`/`content_hash` and the row is reset to `pending`

#### Scenario: One-at-a-time claim
- **WHEN** several due `pending` tasks exist and the worker starts a cycle
- **THEN** exactly one row is `processing` at any instant; the remaining due rows stay `pending` until claimed one by one, and one cycle processes at most 100 tasks

#### Scenario: Restart recovery
- **WHEN** the server starts and rows are in `processing` status (left by an unclean shutdown)
- **THEN** they are reset to `pending` with `attempts` and `last_error` preserved, before the startup reconcile runs, and the worker processes them in a later cycle
