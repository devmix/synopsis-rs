# Tasks: event-queue-incremental-linking

Read first (binding context): `openspec/config.yaml`, `AGENTS.md`, this
change's `proposal.md`, `design.md` (decisions D1–D11), and the delta specs
under `specs/` (data-schema, cli-surface, pipeline, knowledge-graph).

Order reflects dependencies: 1.1 → 1.2 → 1.3 → 1.4. Tasks 1.1–1.3 are Rust
implementation tasks (route to `rust-implementer`); task 1.4 is a
documentation task (route to `docs-writer`) and runs after 1.3 is committed.
Every Rust task must leave the workspace green (`cargo fmt --check`, `cargo
clippy --all-targets -- -D warnings`, `cargo test`). The full concept
reference for all tasks is ADR 0005 (`docs/adr/0005-event-task-queue.md`).

- [x] **1.1** — `queue_tasks` table + `QueueTaskDao` (additive)

**Goal.** Add the event-queue table to the init migration and implement the
DAO in `crates/db`. `document_jobs` and `DocumentJobDao` stay untouched in
this task (removed in 1.2) — the workspace must stay fully green with BOTH
tables present.

**File scope.**
- `migrations/knowledge/1-init/up.sql` — add (next to the existing
  `document_jobs` block): `CREATE TABLE IF NOT EXISTS queue_tasks (id
  INTEGER PRIMARY KEY AUTOINCREMENT, type TEXT NOT NULL, identity TEXT NOT
  NULL, event TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER NOT NULL DEFAULT
  3, last_error TEXT, next_attempt_at INTEGER NOT NULL DEFAULT 0, created_at
  INTEGER NOT NULL, updated_at INTEGER NOT NULL)`; `CREATE INDEX
  idx_queue_tasks_due ON queue_tasks(status, next_attempt_at)`; `CREATE
  UNIQUE INDEX idx_queue_tasks_identity ON queue_tasks(type, identity)`.
  Update the migration header comment (the table list) accordingly.
- `crates/db/src/queue_task.rs` (NEW) —
  - `QueueTask` row struct (all columns) + `row_to_task` mapper.
  - `QueueTaskType` enum: `DocIndex` / `DocDelete` / `EntityLink` with
    `AsRef<str>` values `"doc:index"`, `"doc:delete"`, `"entity:link"` and a
    `from_str` that rejects unknown values.
  - Payload structs (serde): `DocIndexPayload { source_path: String,
    content_hash: Option<String> }`, `DocDeletePayload { source_path:
    String }`, `EntityLinkPayload { entity_ids: Vec<i64> }`.
  - `QueueTaskDao` (mirrors the `DocumentJobDao` executor pattern,
    `ConnectionOrTx`):
    - `enqueue(ty: QueueTaskType, identity: &str, payload: &impl
      Serialize, now: i64)` — upsert on `(type, identity)`: if a row exists,
      reset `status='pending'`, `attempts=0`, `next_attempt_at=now`,
      `updated_at=now`, and update the payload per design D4 — `doc:*`
      REPLACE; `entity:link` MERGE `entity_ids` (union, dedup) when the
      existing status is pending/processing/error, or store only the new ids
      when it is `done`. Insert otherwise.
    - `claim_due(now: i64, batch: i64) -> Vec<QueueTask>` — atomic
      UPDATE…RETURNING flipping due `pending` rows to `processing`, inner
      `ORDER BY next_attempt_at, id LIMIT ?`; re-sort returned rows by
      `(next_attempt_at, id)` (RETURNING order is not guaranteed).
    - `mark_done(id: i64) -> bool`, `mark_failed(id: i64, err: &str, now:
      i64)` — `attempts += 1`; if `attempts >= max_attempts` → `status=
      'error'`, else `status='pending'` with `next_attempt_at = now + 30 *
      2^(attempts-1)` (same schedule as `DocumentJobDao`).
    - `reset_retries(source: Option<&str>, identity: Option<&str>) ->
      usize` — `error` → `pending`, `attempts=0`, `next_attempt_at=now`;
      `source` filters `json_extract(event, '$.source_path') LIKE ? ESCAPE
      '\\'` (doc events only), `identity` filters `identity = ?`.
    - `list(source: Option<&str>, status: Option<&str>) -> Vec<QueueTask>`
      (same filters; ordered by `id`).
    - `status_counts() -> Vec<(String, i64)>`.
    - `delete_entity_link(doc_id: i64) -> usize` — `DELETE WHERE
      type='entity:link' AND identity=?`.
- `crates/db/src/lib.rs` — export the new module/types.

**Dependencies.** None (first task). Reference: existing
`crates/db/src/document_job.rs` for the DAO/test patterns; delta spec
`specs/data-schema/spec.md` for the exact contract.

**Acceptance criteria (machine-checked).**
- Gates green (fmt, clippy `-D warnings`, `cargo test` — whole workspace).
- New tests in `queue_task.rs` (or `crates/db/tests/`): upsert-replace for
  `doc:index` (payload overwritten, row reset); upsert-merge for
  `entity:link` pending (union of ids, dedup); upsert on a `done`
  `entity:link` row (only new ids); re-enqueue moves the row to the end
  (claimed after an older pending row with smaller `next_attempt_at`);
  claim order `(next_attempt_at, id)`; backoff sequence 30/60/120 then
  `error` at `max_attempts`; `reset_retries` (by identity and by source);
  `status_counts`; `delete_entity_link`; `QueueTaskType::from_str` rejects
  unknown values.
- `document_jobs` / `DocumentJobDao` / all existing tests unchanged.

**Estimated size.** ~480 lines (code + tests).

- [x] **1.2** — Switch producer/worker/CLI to `queue_tasks`; drop `document_jobs`

**Goal.** Make `queue_tasks` the only queue: the watcher/startup producer
enqueues `doc:index` / `doc:delete` events, the worker dispatches by event
type, the CLI (`queue status`, `queue reset-retries`, `db stats`) speaks the
event queue — then remove the `document_jobs` table and `DocumentJobDao`
entirely.

**File scope.**
- `migrations/knowledge/1-init/up.sql` — remove the `document_jobs` table +
  `idx_document_jobs_due` and the header-comment references.
- `crates/db/src/document_job.rs` — DELETE; `crates/db/src/lib.rs` — drop
  its exports; fix any remaining references in `crates/db/src/connection.rs`
  (test utilities).
- `crates/ingestion/src/job_queue.rs` — producer: replace
  `DocumentJobDao` enqueues with `QueueTaskDao::enqueue` — `doc:index`
  (identity = file path, payload `source_path` + `content_hash`) and
  `doc:delete` (identity = file path, payload `source_path`); upsert
  semantics come from the DAO. Update the `DocumentJobQueue` struct docs.
- `crates/ingestion/src/worker.rs` — `run_once` claims `queue_tasks`;
  dispatch by type: `doc:index` → `runner.process_document_by_path(identity)`
  then `mark_done(id)`; `doc:delete` → `runner.delete_document_at(identity)`
  then DELETE the row (parity with the old `mark_deleted_row`); unknown type
  → error log, skip. `mark_failed` backoff as before; `log_cycle_summary`
  uses `status_counts`. Update the worker test harness (jobs → tasks) and
  all worker tests.
- `crates/ingestion/src/runner/mod.rs`, `crates/ingestion/src/lib.rs` —
  `DocumentJob` references in docs/test harnesses → `QueueTask`.
- `crates/cli/src/queue.rs` — `queue status`: columns `type, identity,
  status, attempts, last_error, next_attempt_at`; `--source` / `--status`
  filters via `QueueTaskDao::list`. `queue reset-retries`: `--source` and
  `--identity` (replaces `--path`) via `reset_retries`.
- `crates/cli/src/cli.rs` — rename the `--path` flag to `--identity`
  (help text included).
- `crates/cli/src/db.rs` — `db stats` queue count from `queue_tasks`.
- `crates/cli/src/serve/server.rs`, `crates/cli/src/serve/watcher.rs` —
  fix references (the watcher enqueues through `job_queue`; the server wires
  the worker).
- Tests: `crates/cli/tests/queue_cli.rs` (new columns, `--identity`
  filter, reset of a `doc:*` task), `crates/cli/tests/db_cli.rs` (stats),
  `crates/cli/tests/serve_server.rs`, `crates/ingestion/tests/{pipeline_e2e,runner}.rs`.

**Dependencies.** 1.1 complete. Reference: delta specs
`specs/data-schema/spec.md`, `specs/cli-surface/spec.md`; design D3–D5,
D8, D10, D11 (doc-event half).

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace).
- No remaining `document_jobs` / `DocumentJobDao` references anywhere
  (`rg` clean, except archived `openspec/changes/archive/**`).
- Worker tests: `doc:index` task → pipeline runs → row `done`; `doc:delete`
  task → document removed → row deleted; failing task → backoff → `error`;
  `reset-retries` → `pending`.
- CLI tests: `queue status` prints the six columns for mixed rows;
  `--source` / `--status` / `--identity` filters work; `queue
  reset-retries --identity <path>` resets a `doc:delete`/`doc:index` error
  row; `db stats` prints the queue count.
- `pipeline_e2e` still passes end-to-end (index → repeat → delete).

**Estimated size.** ~550 lines (net new/changed; the `document_job.rs`
deletion is not counted).

- [x] **1.3** — `entity:link` end-to-end: resolver report, emission, incremental linking

**Goal.** Wire incremental entity linking through the queue: the resolver
reports created/updated entity ids per document, the runner enqueues an
`entity:link` event when the set is non-empty, the worker processes it with
the graph linker's new incremental mode, and document deletion cascades to
the link task.

**File scope.**
- `crates/ingestion/src/entities/resolver.rs` — report the per-run change
  set: ids CREATED (the `create_entity` path) and ids UPDATED (the
  name-promotion path in `resolve_one`, i.e. `dao.update_name` fired).
  Extend `add_entities` (the pipeline's entry) — and `lookup_or_create*` if
  the pipeline uses them — to return the change set alongside the resolved
  entities (a small struct, e.g. `EntityChanges { created: Vec<i64>,
  updated: Vec<i64> }` with a merged `ids()`). Update all call sites and
  resolver tests (a created entity appears in `created`; a name-promoted
  existing entity appears in `updated`; an unchanged existing entity appears
  in neither).
- `crates/ingestion/src/runner/mod.rs` (or the ingester module where the
  per-document transaction commits) — after a successful index, if the
  change set is non-empty: `QueueTaskDao::enqueue(EntityLink,
  identity = doc_id.to_string(), EntityLinkPayload { entity_ids }, now)`;
  empty set → no event. Log the emitted count.
- `crates/graph/src/linker.rs` — incremental mode: extend
  `build_entity_links` with `candidates: Option<&[i64]>` — `None` = full
  rebuild (current behavior, all existing tests pass unchanged when they
  pass `None`), `Some(ids)` = consider only pairs with at least one member
  in the set. Update the doc comment (the "No incremental mode" deviation
  note is retired) and every call site/test.
- `crates/ingestion/src/worker.rs` — dispatch `entity:link`: parse the
  payload, call a new `runner.link_entities(doc_id, &entity_ids)`,
  `mark_done` / `mark_failed` as with doc events.
- `crates/ingestion/src/runner/cleanup.rs` (or a new `runner/linking.rs`) —
  `link_entities(doc_id, entity_ids)`: no `cross_domain_links` config →
  return Ok (skip, log); else call `graph::build_entity_links(...,
  Some(entity_ids))` with the cache Db and prompts path as
  `build_entity_links_locked` already does; record the `last_linking_run`
  app_kv marker (observability only, design D6); per-pair errors from
  `LinkResult.errors` → warn log, still Ok; task-level failure → Err (the
  worker applies backoff).
- `crates/ingestion/src/runner/mod.rs` — `delete_document_at` (and the prune
  path): after the document is deleted, `QueueTaskDao::delete_entity_link
  (doc_id)`.
- Tests: resolver change-set report; emission (non-empty → one task with
  exactly the delta ids; empty → no task; re-index the same document →
  merged union per design D4); worker dispatch (task → links created;
  linker failure → `error` status after `max_attempts`); graph incremental
  (only candidate pairs considered, no duplicates on re-run, full mode
  unchanged); cascade delete (deleting the document removes its
  `entity:link` row).

**Dependencies.** 1.2 complete. Reference: delta specs
`specs/pipeline/spec.md` (`Post-processing: linking`),
`specs/knowledge-graph/spec.md` (`Cross-domain linking pipeline`,
`Incremental linking` scenario); design D6–D9, D11.

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace), including ALL pre-existing
  `crates/graph/tests/*` linker tests (now passing `candidates=None`).
- The 10-of-1000 scenario is covered by a test: a document whose entities
  are all pre-existing except N gets an `entity:link` task with exactly N
  ids.
- A test proves the merge: two successive index runs enqueue 10 then 5 ids
  → the pending task carries the union (≤15 unique ids), nothing lost.
- `synopsis queue status` shows the `entity:link` row (CLI test or
  integration test); `queue reset-retries --identity <doc_id>` resets it.
- Per-pair failure: a failing CEL/LLM pair is recorded in `LinkResult
  .errors`, the task still completes `done` with a warn log.

**Estimated size.** ~480 lines (code + tests).

- [x] **1.4** — Documentation update (after the code lands)

**Goal.** Sync the documentation site and the root README with the new
event-queue model and incremental linking. This task runs AFTER 1.3 is
committed — the docs describe the final state, and ADR 0005
(`docs/adr/0005-event-task-queue.md`) is the concept reference. Route to the
`docs-writer` agent (MDX site) — not `rust-implementer`; no Rust code is
touched.

**File scope.**
- `site/docs/concepts/job-queue.mdx` — the main rewrite: `queue_tasks`
  (not `document_jobs`), the three event types (`doc:index`, `doc:delete`,
  `entity:link`), `type`+`identity` row model, upsert semantics (doc
  replace / link merge-union), re-enqueue-to-end, the state diagram
  (unchanged lifecycle), backoff (unchanged), the CLI with the new columns
  and `--identity`, and a section on how `entity:link` events are emitted
  by the pipeline (created/updated entities only).
- `site/docs/reference/database-schema.mdx` — the `document_jobs` section
  and the `idx_document_jobs_due` index row → `queue_tasks` + both indexes.
- `site/docs/reference/cli.mdx` — `queue status` columns
  (type/identity/…), `--identity` instead of `--path`; the see-also line.
- `site/docs/concepts/pipeline.mdx` — the queue-table paragraph and the
  post-processing stage: event-driven incremental linking (the 10-of-1000
  behavior), per-pair skip-and-log, cascade delete of the link task.
- `site/docs/developer/architecture.mdx`, `site/docs/developer/testing.mdx`
  — `document_jobs` → `queue_tasks` wording.
- `site/docs/concepts/vector-rebuild.mdx`, `site/docs/roadmap.mdx`,
  `site/docs/reference/config-schema.mdx`,
  `site/docs/guides/{ingestion,configuration,troubleshooting}.mdx` —
  `document_jobs` references → the event queue (wording only where the
  behavior is unchanged).
- `README.md` — the `queue status|reset-retries` row ("inspect/repair the
  document job queue" → event task queue) and the crate-layout line; the
  README doubles as shipped end-user documentation (AGENTS.md gotcha) —
  keep it in sync with the CLI surface.
- `AGENTS.md` — the `crates/ingestion` layout row ("job queue + worker" →
  "event task queue + worker") if the wording is stale after 1.3; the
  subcommand list stays as-is.

**Dependencies.** 1.3 committed. Reference: ADR 0005 (the full concept),
the delta specs under this change's `specs/`, the final code state.

**Acceptance criteria (machine-checked).**
- `rg "document_jobs" site/ README.md AGENTS.md` returns nothing (archived
  `openspec/changes/archive/**` and ADR 0005's historical mentions are
  allowed).
- The site builds: `npm run build` in `site/` (or the site's documented
  build command) succeeds.
- `queue_tasks`, `doc:index`, `doc:delete`, `entity:link`, `--identity`
  appear in the concept + reference pages; the state diagram and backoff
  schedule are still correct (they did not change).
 - No factual drift: every statement in the updated pages matches the delta
   specs (e.g. merge-union on pending, replace on doc events, re-enqueue to
   the end, one row per (type, identity)).

**Estimated size.** ~200–300 lines of MDX/Markdown edits.

- [ ] **1.5** — Accurate `processing` state: one-at-a-time claiming + startup recovery

**Goal.** Fix two lifecycle defects of `queue_tasks` found in review after
1.3: (1) `claim_due` pre-claims up to 100 rows into `processing` in one
UPDATE, so the queue shows a whole batch as `processing` while only one
task actually executes — `processing` must mean "currently executing";
(2) rows left in `processing` by an unclean shutdown (crash, SIGKILL,
power loss) are never claimed again (`claim_due` picks only `pending`)
and `reconcile_source` skips them — on serve startup they must be reset
to `pending`.

**File scope.**
- `crates/db/src/queue_task.rs` — replace `claim_due(now, batch) ->
  Vec<QueueTask>` with `claim_one(now) -> Result<Option<QueueTask>>`
  (same WHERE clause — due `pending` rows, order `(next_attempt_at, id)`,
  `LIMIT 1`). Add `recover_stuck_processing(now) -> Result<i64>`:
  `UPDATE queue_tasks SET status='pending', updated_at=? WHERE
  status='processing'`; `attempts` and `last_error` are NOT touched
  (an interrupted attempt is not a failed one); returns the changed-row
  count. Adapt the existing claim tests (batch-claim → repeated
  `claim_one`).
- `crates/ingestion/src/worker.rs` — `run_once` claims in a loop:
  `for _ in 0..WORKER_BATCH_SIZE { match claim_one { Some(t) =>
  process_task(t), None => break } }` — the per-cycle cap (100) stays as
  the owner-thread starvation guard.
- `crates/cli/src/serve/server.rs` — at serve startup, BEFORE the
  startup reconcile (`reconcile_enabled_sources(..., "startup")`), call
  `recover_stuck_processing(now)` once and log the recovered count when
  > 0.
- Tests:
  - DAO: `claim_one` returns at most one row per call, in
    `(next_attempt_at, id)` order, due `pending` only; a second call
    returns the next row; `recover_stuck_processing` resets
    `processing`→`pending` preserving `attempts`/`last_error`, leaves
    `pending`/`done`/`error` rows untouched, returns the count.
  - Worker: with several due rows, while one task is being processed the
    remaining rows stay `pending` (at most one `processing` at any
    instant).
  - Crash simulation: a claimed row (`processing`) survives a "restart"
    — `recover_stuck_processing` (the startup path) + a fresh worker's
    `run_once` → the row reaches `done`.
  - Serve level (if the `serve_server.rs` harness allows): serve starts
    with a stuck `processing` row → after startup the row is
    `pending`/processed, and startup reconcile saw it.

**Dependencies.** 1.2, 1.3 committed. Reference: delta spec
`specs/data-schema/spec.md` (scenarios "One-at-a-time claim", "Restart
recovery"); ADR 0005 (single sequential consumer).

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace).
- A test proves: with N due rows, at most one row is `processing` at any
  instant during a worker cycle; the rest stay `pending`.
- `claim_one` order and due-filter behavior pinned by DAO tests; the
  per-cycle cap still bounds one `run_once` to 100 tasks.
- `recover_stuck_processing`: `processing`→`pending` with
  `attempts`/`last_error` preserved; other statuses untouched; count
  returned.
- Crash-simulation test: a `processing` row is recovered on the startup
  path and processed to `done`.
- Serve startup performs the recovery before the startup reconcile.

**Estimated size.** ~250–350 lines (code + tests).
