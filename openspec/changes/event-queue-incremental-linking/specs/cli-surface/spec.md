# cli-surface Specification

## MODIFIED Requirements

### Requirement: queue subcommand

The CLI SHALL provide a subcommand for inspecting and maintaining the event queue (`queue_tasks`). Subcommands: `queue status [--source PATH] [--status NAME]` — tabular output of task state (columns: type, identity, status, attempts, last_error, next_attempt_at); `queue reset-retries [--source PATH] [--identity VALUE]` — resets the retry counter for tasks in `error` status (status→`pending`, attempts→0, next_attempt_at→now) across all event types, after which the background worker re-processes them. The subcommand is additive to the existing ones (`serve`, `db`, `model`, `onnx-runtime`, `load-test`).

> **Contract decision (2026-08-29):** the subcommand was originally designed as `index` but was renamed to `queue` by a human — the name `queue` more accurately reflects the entity (the single state table, shared by watcher/startup/worker) rather than the indexing process. This is an explicit deviation from the original naming in task 1.7.
>
> **Contract decision (2026-09-06):** the queue became a generic event queue (`queue_tasks`, human decision — event-queue-and-incremental-linking change). The `--path` filter was renamed to `--identity` because the row key is now the event identity (path for `doc:*` events, document id for `entity:link`), and `queue status` prints the event `type` column. The `queue` subcommand name, actions, and `--source`/`--status` filters are preserved.

#### Scenario: Status
- **WHEN** invoking `synopsis queue status`
- **THEN** a table of all tasks in the `queue_tasks` queue is printed with columns type/identity/status/attempts/last_error/next_attempt_at; the `--source` filter narrows the output to a single source (doc events), `--status` — to a single status

#### Scenario: Status of a link task
- **WHEN** an `entity:link` task exists for a document
- **THEN** `synopsis queue status` prints a row with type `entity:link` and identity equal to the document id

#### Scenario: Reset retries
- **WHEN** invoking `synopsis queue reset-retries --identity <doc-id>` for an `entity:link` task in `error` status
- **THEN** the task moves to `pending` with attempts=0; the next background-worker cycle re-processes it (the row is no longer in `error` status)
