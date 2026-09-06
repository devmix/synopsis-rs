# ADR 0005 — Event task queue: one typed event queue for all background work

- Status: accepted
- Deciders: the human (project owner)
- Date: 2026-09-06
- Tags: architecture, queue, ingestion, entity-linking, cli

Technical Story: design of the background-work queue of the Synopsis server (knowledge DB).

## Context and Problem Statement

Synopsis performs background work: ingesting documents from watched sources (indexing, deletion) and deriving cross-domain entity links from the ingested documents. This work must survive server restarts, be retried with backoff on failure, and be inspectable and repairable from the console.

The question this ADR answers is the design of that mechanism from first principles: **how should the system queue background work in general** — which data model, which semantics for duplicates, ordering, retries, and visibility — so that every current and future kind of work shares one mechanism instead of growing one subsystem per work kind?

## Decision Drivers

- **Single consumer on the owner thread.** The ingestion `Runner` is `!Send + !Sync`; the background worker is one blocking `run_once` per poll tick of the serve loop, interleaved with shutdown. No parallel consumers are possible without a larger redesign; the queue model must fit a single sequential consumer.
- **Laptop scale, personal use (16 GB).** Simplicity and predictability beat throughput. One queue, one state machine, one CLI.
- **Every background work item must be inspectable and restartable** through the console — the same status view and the same retry reset for every kind of work, not per-kind subcommands.
- **No silent loss of pending work.** Derived work (e.g. "link these entities") emitted while an earlier emission for the same subject is still pending must accumulate, never overwrite.
- **Derived work must be incremental.** Re-ingesting a document must enqueue work for the entities that run actually created or updated — not the document's full entity set, and not the whole dataset.
- **Frozen-contract discipline.** Schema and CLI are pinned contracts; the model must keep the contract surface small (one table, one subcommand) so that future work kinds do not force contract changes.
- **Extensibility.** A new kind of background work must be a code-level addition (event type + dispatch arm), not a new table, a new worker, or a new CLI subcommand.
- **Schema stability.** The queue table lives in the knowledge DB's squashed init migration (`PRAGMA user_version` = 1, no forward migration for it); the table shape must therefore be stable by design, with per-event variability carried in a payload, not in columns.

## Considered Options

1. **Document-only queue + scheduled derived work.** The queue tracks documents only (path-keyed index/delete operations); derived work such as entity linking runs as a scheduled batch after the queue drains (e.g. post-batch, throttled).
2. **One queue table per work kind.** A table per work kind (document jobs, link tasks, …); the worker drains each; the console grows a parallel status/reset surface per table.
3. **One generic event queue.** A single `queue_tasks` table: a typed event (`type` column, `{group}:{name}` namespace), a stable dedup `identity`, a JSON residual payload, and one shared status/backoff lifecycle — one queue, one consumer, one console surface for all work kinds.

## Decision Outcome

Chosen option: **"One generic event queue" (option 3)**, because it is the only option in which per-item console visibility and retry repair hold for *every* work kind by construction, in which pending derived work can be merged instead of overwritten, and in which a future work kind costs one enum variant plus one dispatch arm — no schema change, no new table, no new subcommand.

The concept, in full:

- **Row model.** `id` (autoincrement), `type` (e.g. `doc:index` | `doc:delete` | `entity:link`), `identity` (the dedup key: the file path for `doc:*` events, the document id for `entity:link`), `event` (JSON residual payload only — `source_path`/`content_hash` or `entity_ids`), `status` (`pending` | `processing` | `done` | `error`), `attempts` / `max_attempts` (default 3), `last_error`, `next_attempt_at`, `created_at` / `updated_at`. Queue mechanics (dedup, ordering, filters, console display) use columns only; the JSON payload is parsed at dispatch time, never for queueing.
- **One row per `(type, identity)`** (unique index). Enqueue is an **upsert**: the row resets to `pending`, `attempts=0`, `next_attempt_at=now`. Payload update semantics are per type — work whose subject is externally mutable **replaces** the payload (the file on disk is the source of truth; re-indexing reads current content), while derived work **merges** its payload while the row is `pending`/`processing`/`error` (union), and stores only the new payload when the row is `done` (the old payload was already processed). This is what makes "emission #1 queues 10 entities, emission #2 queues 5" lose nothing.
- **Ordering.** Claim order is `(next_attempt_at, id)`; a re-enqueued event therefore moves to the **end** of the queue. The same column drives the backoff schedule (`30 × 2^(attempts−1)` s) — one mechanism answers "when is this task allowed to run" for both retries and re-enqueues.
- **Single sequential consumer.** The worker claims due rows (a batch) and dispatches by `type`. No concurrency is introduced; a slow task (e.g. one that calls the LLM endpoint) delays the next batch drain in the same class as the per-document pipeline work that already runs on the owner thread, and the expensive results are cached (LLM decisions in `llm_linker_cache`), so repeat candidates are cheap.
- **Producers.** Any component may enqueue through the same contract: the startup reconcile and the file watcher (doc events), and the pipeline itself, which emits derived events from inside document processing when it observes a non-empty delta (created/updated entities). A future producer follows the same enqueue contract.
- **Incremental derived work.** The emitter reports, per run, exactly the ids it created or updated and enqueues exactly those. The consumer's derived work runs in a candidate mode (only pairs involving the candidates) with the full-rebuild mode retained as a tested, unwired fallback.
- **Cascade rules.** Deleting a parent resource removes its pending child events: processing `doc:delete` deletes the document's `entity:link` row. Doc-event rows themselves are kept (one row per identity — the same "latest state per key" semantics a path primary key gives).
- **Console surface.** One `queue` subcommand for all event types: `queue status` prints `type, identity, status, attempts, last_error, next_attempt_at` (filters `--source`, `--status`, `--identity`); `queue reset-retries` applies to all types; `db stats` counts queue tasks.
- **Extensibility.** A new event type = a `QueueTaskType` variant + a payload struct + a worker dispatch arm (+ console display when the identity is not self-explanatory). The stable row model (D: schema stability) means the variability lives in the payload, so adding types never touches the schema or the contract specs.

### Positive Consequences

- One durability/retry/backoff implementation serves every work kind; new kinds inherit it.
- Per-item status and repair from the console hold for every work kind by construction.
- Incremental derived work falls out of the event model — no time windows, no extra timestamp columns on domain tables.
- The contract surface stays small: one table, one subcommand.
- The row model is stable by design: future work kinds are payload-level additions.

### Negative Consequences

- All events serialize on the owner thread: a slow task delays the next batch drain (accepted — same class as existing pipeline work; mitigated by result caching).
- One row per `(type, identity)` folds history: there is no per-key audit trail beyond the latest state (accepted for a personal tool; the same semantics a path primary key gives).
- Work-kind-specific fields live in the JSON payload: ad-hoc queries need `json_extract` (accepted — the worker and the console are the only readers, and both are typed).
- Because the table is part of the squashed init migration, any future *schema-level* change to the queue requires a new forward migration (forward-only discipline) — the design minimizes that risk by pushing variability into the payload.

## Pros and Cons of the Options

### 1. Document-only queue + scheduled derived work

- Good, because the queue stays minimal (path-keyed doc operations only).
- Good, because no event plumbing in the pipeline.
- Bad, because derived work has no per-item status/retry/console visibility — failures are only logged.
- Bad, because a scheduled batch is either a full rebuild (re-processes everything on every run) or needs its own incrementality machinery (time windows, extra columns) that the queue model does not provide.
- Bad, because no cascade of derived work on parent deletion, and every future derived kind needs its own scheduling hook.

### 2. One queue table per work kind

- Good, because each table can be shaped exactly to its work kind (typed columns, no JSON).
- Good, because the queues do not block each other.
- Bad, because durability/retry/backoff/console logic is duplicated per table — the "same status view and retry reset" requirement becomes a copy, not a consequence.
- Bad, because cross-kind invariants (cascade delete, shared ordering) are re-implemented per table.
- Bad, because every future work kind adds a table, a DAO, and a console surface — the model does not scale past two kinds.

### 3. One generic event queue (chosen)

- Good, because unified status/retry/console for all work kinds; new kinds are a code-level addition.
- Good, because per-identity upsert with per-type payload semantics (replace vs merge) makes pending derived work lossless.
- Good, because one consumer, one ordering, one backoff — minimal moving parts at laptop scale.
- Good, because the row model is stable; variability is carried in the payload, protecting the schema and the contract surface.
- Bad, because all event types share one owner-thread consumer (a slow task delays doc work).
- Bad, because work-kind-specific queries need `json_extract` (only the console uses them).
- Bad, because per-type payload semantics (replace vs merge) must be specified per event type — a new type must state which one it uses.

## Links

- Related: [ADR 0001](0001-sqlite-fts5.md) (the knowledge DB and its squashed init migration — the queue table lives there), [ADR 0004](0004-usearch-lsm-segments.md) (the worker's post-batch GC and the vector engine it maintain run in the same owner-thread poll cycle the queue feeds).
