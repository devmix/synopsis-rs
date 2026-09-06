# cli-surface Specification

## Purpose

The external CLI contract of Synopsis: subcommands, flags, argument order, behavior. Parity is verified by machine-diff of usage/`--help` outputs and by behavioral scenarios.
## Requirements
### Requirement: Global flags and command-line structure

Invocation format: `synopsis [--config PATH] [--preset NAME] [--dataset NAME] [--version] <subcommand> [flags...]`. Global flags precede the subcommand; per-command flags follow it. The global flags are exactly `--config` (explicit config path; wins over preset auto-search), `--preset` (default `default`), `--dataset` (dataset name; overrides `config.dataset.name`), and `--version`; there is **no `--db` flag** — the knowledge-DB path is derived from `paths.workspace_dir` + `dataset.name`. Subcommands: `serve`, `model`, `onnx-runtime`, `load-test`, `queue`, `db`.

> **Contract decision (2026-08-29):** the `sync` subcommand has been removed (see REMOVED
> Requirements); the `db` subcommand with a `clear` action has been added. Full re-ingest is
> done via `db clear` + `serve` restart (startup reconcile), not via direct ingestion.

#### Scenario: Argument order
- **WHEN** invoking `synopsis --preset default serve --port 9090`
- **THEN** the server starts on port 9090 (the global preset is applied, the per-command flag comes after the subcommand)

#### Scenario: Version
- **WHEN** invoking `synopsis --version`
- **THEN** the version is printed and the process exits with code 0

#### Scenario: Dataset override
- **WHEN** invoking `synopsis --dataset edtech serve`
- **THEN** the `--dataset` global flag overrides `config.dataset.name` for the run

### Requirement: serve subcommand

The only long-running mode: startup reconcile (enqueue diff into `queue_tasks`) + MCP over HTTP (Streamable HTTP, design D8) + file watching (enqueue diff). Flags: `--no-initial-sync` (skip startup reconcile at boot), `--port N` (default 8080, overrides server.port from config), `--auto-rebuild-vectors`. On a vector-engine dimension mismatch, serve clears the dataset DB (`clear_dataset`) and the startup reconcile re-enqueues all files — the worker re-embeds them (clear-then-queue, no direct ingestion).

#### Scenario: Port
- **WHEN** serve --port 9123
- **THEN** the HTTP server listens on 9123, GET /health returns 200

### Requirement: model and onnx-runtime subcommands

`model` manages the model registry (list/benchmark); `onnx-runtime` manages ONNX Runtime loading/state. Flags and output are fixed by this contract.

#### Scenario: usage parity
- **WHEN** machine-diffing the `model --help` / `onnx-runtime --help` outputs against recorded fixtures
- **THEN** the set of flags, default values, and description texts are identical (formatting normalization is allowed)

### Requirement: load-test subcommand

Benchmark of MCP tools on generated data. Flags: `--scale small|medium|large` (default small), `--seed N` (default 42, PRNG seed for deterministic data generation), `--iterations N` (default 100), `--json PATH`, `--no-fill` (benchmark an existing DB). Report: CALLS, AVG/P50/P95/P99/MAX ms, QPS per tool case; a human-readable report on stdout, and with --json the same report as JSON.

#### Scenario: Report
- **WHEN** load-test --scale small finishes
- **THEN** stdout contains a per-case latency table whose structure matches the recorded report fixture (machine-diff by sections)

#### Scenario: Deterministic seed
- **WHEN** load-test runs twice with the same `--seed` (default 42) and the same scale
- **THEN** the generated dataset is identical across runs (deterministic PRNG)

### Requirement: Configuration resolution

Priority: `--config` > `config.{preset}.yaml`, where the preset defaults to `default`; automatic config-file discovery — first relative to the executable's directory, then CWD. Behavior when no config is found and the error texts are fixed by this contract (accounting for the fact that the Rust binary lives in its own directory).

#### Scenario: Preset
- **WHEN** config.prod.yaml sits next to the binary and `synopsis --preset prod serve` is invoked
- **THEN** config.prod.yaml is used without an explicit --config

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

### Requirement: db subcommand

A subcommand for dataset database maintenance. It SHALL provide two actions: `db stats` — prints dataset and knowledge-DB statistics (document, chunk, entity, entity_link, fact and queue-job counts), read-only, no modification and no confirmation; `db clear` — prints the same statistics, asks for confirmation (`Confirm deletion? [y/N]` on stdin) and on `y`/`Y` deletes the entire dataset state directory (`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db` and the vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`, ignoring a missing directory). This atomically removes both the SQLite DB and the vectors in one call. The command is one-shot and does not load the embedding model / ONNX (it opens only the dataset-bound DB to print statistics, then closes it before deletion). After `db clear` a `serve` restart is required so the startup reconcile re-enqueues the files and the background worker re-embeds the documents and recreates the DB + vector index.

#### Scenario: Stats
- **WHEN** `synopsis db stats` is invoked
- **THEN** counts for documents/chunks/entities/entity_links/facts/queue are printed; the DB is not modified

#### Scenario: Clear with confirmation
- **WHEN** `synopsis db clear` is invoked and the answer is `y`
- **THEN** the whole dataset state directory (knowledge.db + vectors/) is removed from disk; a cleanup summary is printed

#### Scenario: Clear aborted
- **WHEN** `synopsis db clear` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no deletion happens; the command exits without modifying the DB

#### Scenario: Stats shown before prompt
- **WHEN** `synopsis db clear` is invoked
- **THEN** before the confirmation prompt, counts for documents/chunks/entities/entity_links/facts/queue are printed
