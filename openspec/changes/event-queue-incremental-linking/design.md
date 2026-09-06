# Design: event-based `queue_tasks` + incremental entity linking

## Problem

`Runner::build_entity_links` / `graph::build_entity_links` have no production
caller (verified: only test modules; `git log -S` shows the call was never
wired — the Go oracle had it in `ingest_all`, dropped by the queue-only
model). The linker is always a **full rebuild** (recorded YAGNI deviation,
`crates/graph/src/linker.rs:47`), and the pipeline spec's app_kv
incrementality window was never implemented. User requirement (2026-09-06):
linking must run after document indexing, be **incremental** (only newly
created/updated entities — 10 of 1000, not 1000), skip-and-log on pair
errors, and be visible/restartable through the same queue CLI as documents.

## Reference contracts

- `docs/adr/0005-event-task-queue.md` — the full event-queue concept
  (written before implementation; the authoritative design record for this
  change and for future event types)
- `openspec/specs/data-schema/spec.md` — `document_jobs table (indexing queue)` (replaced)
- `openspec/specs/cli-surface/spec.md` — `queue subcommand`, `db subcommand` (adapted)
- `openspec/specs/pipeline/spec.md` — `Post-processing: linking` (re-specified)
- `openspec/specs/knowledge-graph/spec.md` — `Cross-domain linking pipeline` (incremental mode added)
- No recorded fixtures reference queue rows or entity-link timing: the queue
  is internal state and MCP fixtures are unchanged — parity for this change
  is the workspace test suite (differential linker tests already exist in
  `crates/graph/tests/`).

## Decisions

### D1 — Generic event queue `queue_tasks` (user's design) over the alternatives

**Decision:** replace `document_jobs` with one `queue_tasks` table carrying
typed events (`doc:index`, `doc:delete`, `entity:link`); one queue, one
consumer, sequential processing.

**Alternatives considered:**
- *Separate `entity_link_tasks` table alongside `document_jobs`* (analyst's
  minimum-risk option): keeps the frozen table untouched but yields two
  queues, two CLI surfaces, and no unified retry/status story — rejected by
  the user (explicit design: a common queue with events).
- *Post-batch throttled full rebuild, no queue* (fastest): no per-task
  status/retry for linking, no cascade delete, no CLI visibility — does not
  satisfy the requirements.

**Why:** a single state machine for all background work is what makes
"linking status and restart via the console command, like for documents"
fall out for free (one `queue` subcommand, one backoff implementation, one
GC path). The user's message is the explicit human decision authorizing the
frozen-contract changes.

### D2 — Edit the init migration directly; no separate forward migration

**Decision:** `queue_tasks` is defined in
`migrations/knowledge/1-init/up.sql`; `document_jobs` is removed from it;
`PRAGMA user_version` stays 1.

**Why:** human decision 2026-09-06 — the project has **no deployed
instances**, so there is no data to migrate; a forward migration
`2-queue-tasks` with row-copy logic would add machinery for a table nobody
has. This also preserves design D6's invariant (one squashed init
migration = the whole schema). A stale dataset DB with the old table is
handled by `db clear` + re-serve (documented in the proposal Non-goals).

### D3 — `type` + `identity` as columns; JSON only for the residual payload

**Decision:** schema columns `type TEXT` (`doc:index` | `doc:delete` |
`entity:link`) and `identity TEXT` (path for doc events, doc id as decimal
string for `entity:link`); `event TEXT` JSON holds only the residual
(`source_path`/`content_hash` / `entity_ids`). Unique index
`(type, identity)`; due index `(status, next_attempt_at)`; claim order
`(next_attempt_at, id)`.

**Why:** queue mechanics (dedup, upsert, filters, CLI display) must not
parse JSON per row — the user explicitly asked for columns instead of a
`dedup_key`+JSON design. Event types are named `{group}:{name}` per user
preference. `id` autoincrement + `next_attempt_at` are required additions
over the user's draft list: backoff needs `next_attempt_at`, and the old
`path` PK cannot serve a multi-type queue.

### D4 — Upsert semantics per event type: replace (doc) vs merge (link)

**Decision:** enqueue is an upsert on `(type, identity)`. `doc:*` events
REPLACE the payload (the file on disk is the source of truth — re-indexing
reads current content). `entity:link` MERGES `entity_ids` (union) when the
existing row is `pending`/`processing`/`error`; when it is `done`, only the
new ids are stored (the old ones were already linked).

**Why:** a naive replace loses pending link candidates (user-identified
flaw: update #1 enqueues 10 entities, update #2 enqueues 5 → the 10 must not
be lost). Merge-on-pending guarantees no candidate is dropped; merge-on-done
avoids re-linking already-processed ids (idempotency would make it safe but
not free). Multiple rows per identity (no unique index) was rejected: queue
bloat and no dedup when the same file changes repeatedly.

### D5 — Re-enqueued events move to the END of the queue

**Decision:** every upsert sets `next_attempt_at := now`; claim order is
`(next_attempt_at, id)`.

**Why:** user requirement (2026-09-06). It reuses the existing backoff
column and the existing `claim_due` ordering mechanism — no new ordering
column; `id` as tie-breaker keeps insertion order deterministic (the current
`path` tie-breaker becomes `id`).

### D6 — Incrementality via the resolver's created/updated report, not entity timestamps

**Decision:** during indexing, the entity resolver reports the ids it
**created** (`create_entity` path) and **updated** (name-promotion path,
`dao.update_name`). The runner enqueues `entity:link` with exactly those
ids; empty set → no event.

**Alternatives:** an `entities.created_at > last_linking_run` window (the
old spec's app_kv idea) — rejected: `entities` has no `updated_at`, so
updates (the 10-of-1000 case) would be missed, and a time window re-links
entities unrelated to the changed document. Adding `updated_at` to
`entities` — rejected: schema growth for a signal the resolver already
computes in-run.

**Note:** the `last_linking_run` app_kv marker (cache DB) is still recorded
after each processed `entity:link` task for observability/logging; it no
longer drives an incrementality window.

### D7 — One `entity:link` event per document (identity = doc id)

**Decision:** per-document task carrying the candidate id list.

**Why:** a stable merge point (D4) and a natural cascade-delete key
(document deletion removes its task); per-entity tasks would bloat the queue
and fragment the status view.

### D8 — Worker processes all event types sequentially from one queue

**Decision:** `DocumentWorker::run_once` claims due rows of any type and
dispatches: `doc:index` → `process_document_by_path`, `doc:delete` →
`delete_document_at` (+ cascade delete of the doc's `entity:link` row),
`entity:link` → new `Runner` entry that runs the incremental linker. The
post-batch orphan sweep is unchanged.

**Why:** user requirement ("очередь обрабатывается последовательно"); the
`Runner` is `!Send + !Sync` and the worker already runs on the serve owner
thread — no new concurrency is introduced. LLM linking latency on the owner
thread is the same class of blocking as the NER/embedding pipeline that
already runs there; LLM decisions are cached in `llm_linker_cache`, so
repeat candidates are cheap.

### D9 — Full rebuild retained but not wired into the worker

**Decision:** `graph::build_entity_links` keeps its full-rebuild behavior
(candidates = all entities) and stays tested; the worker links only via
events. The graph entry gains an incremental mode: an optional candidate
set — `None` = full rebuild (existing behavior, existing tests untouched),
`Some(ids)` = only pairs with ≥1 member in the set.

**Why:** the full rebuild is a useful manual fallback and is already
implemented/tested; removing it would churn passing tests for no benefit.
Wiring it into the worker would violate the incremental requirement.

### D10 — CLI: `--path` → `--identity`, new `type` column

**Decision:** `queue status` prints `type, identity, status, attempts,
last_error, next_attempt_at`; `queue reset-retries` keeps `--source` and
renames `--path` to `--identity`; `db stats` counts queue tasks. The
`queue` subcommand name, its actions, and `--source`/`--status` are
preserved (contract decision 2026-08-29 stays valid).

**Why:** `identity` is now the row key for ALL event types; `--path` would
be a lie for `entity:link` rows. This is part of the same explicit
human decision (2026-09-06) authorizing the cli-surface change.

### D11 — Cascade delete on `doc:delete`

**Decision:** processing a `doc:delete` event (and the prune path that
deletes documents) removes the document's `entity:link` queue row.

**Why:** user requirement — deleted documents must not leave stale link
tasks. Doc-event rows (`doc:index`/`doc:delete`) themselves are kept as
history (one row per identity, like today's `path` PK semantics).

## Frozen-contract changes (explicit decisions)

1. **data-schema** — `document_jobs` → `queue_tasks` (D2, D3): human
   decision 2026-09-06, no deployed instances, init migration edited.
2. **cli-surface** — `queue status` columns + `--identity` rename (D10):
   same human decision; subcommand name and actions preserved.
3. **pipeline** — `Post-processing: linking` re-specified from app_kv-window
   full rebuild to event-driven incremental linking (D6, D7, D8): the old
   requirement was never implemented; the event model is its correct
   implementation.
4. **knowledge-graph** — incremental mode added to the linking pipeline
   requirement (D9): additive; full-rebuild scenarios unchanged.

## Risks

- LLM linking latency on the owner thread during large first-link runs
  (mitigation: decision cache; equals/expression are local and cheap).
- Resolver API change ripples through ingestion tests (contained: one crate).
- The transitional state between task 1.1 and 1.2 (both tables exist in the
  init migration) is commit-local only; 1.2 removes `document_jobs`.
