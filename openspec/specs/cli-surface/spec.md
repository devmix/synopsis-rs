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

Shutdown is signal-driven and cooperative. The first SIGINT or SIGTERM starts a graceful stop: the in-flight queue task (if any) runs to completion and is recorded, the worker stops claiming new tasks, the HTTP server drains in-flight requests, the vector RAM layer is saved, and the process exits with code 0. This applies to every worker cycle, including the startup drain (the server is stoppable while the initial sync is still processing the queue). As part of the graceful stop the server terminates its own long-lived legacy SSE sessions (the streams end, connected MCP clients see a clean EOF) — an open SSE stream is an in-flight request that would otherwise keep the HTTP drain open until the 10 s bound, so the drain completes promptly even with connected clients. A second SIGINT or SIGTERM at any time forces an immediate exit with code 130 (128 + SIGINT), abandoning the in-flight work — the restart recovery (stuck `processing` rows reset to `pending`) and the startup vector self-heal repair any residual state, so the forced exit is safe.

#### Scenario: Port
- **WHEN** serve --port 9123
- **THEN** the HTTP server listens on 9123, GET /health returns 200

#### Scenario: Graceful stop mid-ingestion
- **WHEN** SIGINT is received while the worker is processing a queue task and more tasks are pending
- **THEN** the in-flight task completes and is recorded, no further tasks are claimed, the server stops, the process exits with code 0, and the unclaimed tasks remain `pending` for the next startup

#### Scenario: Stop during the startup drain
- **WHEN** SIGINT is received during the startup worker drain (before the owner loop serves traffic)
- **THEN** the drain stops after the in-flight task, the serve flow proceeds to the bounded shutdown, and the process exits with code 0

#### Scenario: Graceful stop with an active SSE session
- **WHEN** SIGINT is received while a legacy SSE MCP session is connected (no queue task in flight)
- **THEN** the server terminates the SSE session (the stream ends, the client sees a clean EOF), the HTTP drain completes within the bounded shutdown (well under the 10 s bound), and the process exits with code 0

#### Scenario: Second signal forces exit
- **WHEN** a second SIGINT or SIGTERM is received after the first one (while the graceful stop is still finishing the in-flight task)
- **THEN** the process exits immediately with code 130

### Requirement: model and onnx-runtime subcommands

`model` manages the model registry (list/download/delete/info/benchmark); `onnx-runtime` manages ONNX Runtime loading/state. Flags are fixed by this contract. Human-readable output is rendered by the unified console layer (see "console output presentation"): box-drawing tables and aligned key-value blocks. `model list` prints a table with the columns NAME (registry identifier from `onnx.yaml`, the value used by `download`/`delete`/`info`), DISPLAY (human-readable display name), DIM (vector dimensionality), STATUS (installation state, `installed ✓` or `not installed`). `model info` prints a key-value block (name, display name, description, version, vector dim, source, repository, installation status, installation date when installed, and the expected files with sizes). `onnx-runtime status` prints the version, installation state, library path, cache directory, and supported platforms.

> **Contract decision (2026-09-09):** the visual layout of `model list` /
> `model info` / `onnx-runtime` output changed from fixed-width dash-separated
> lines to box-drawing tables / key-value blocks rendered by the unified
> console layer, and `model list` gained the `NAME` column (the former `NAME`
> column, which carried the display name, was renamed to `DISPLAY`). Human
> decision: tabled-style box-drawing output with terminal-width adaptation,
> TTY-only color, and wrapping of long values. Every field, flag, filter,
> exit code, and the `--help` contract are unchanged.

#### Scenario: usage parity
- **WHEN** machine-diffing the `model --help` / `onnx-runtime --help` outputs against recorded fixtures
- **THEN** the set of flags, default values, and description texts are identical (formatting normalization is allowed)

#### Scenario: model list shows registry name and display name
- **WHEN** invoking `synopsis model list` with a registry entry whose `name` and `display_name` differ
- **THEN** the table row shows the registry `name` in the NAME column and the `display_name` in the DISPLAY column, alongside vector dim (DIM) and installation status (STATUS); the version is not in the list (it remains in `model info`)

#### Scenario: long values do not misalign the table
- **WHEN** a registry entry's display name is longer than any fixed column width (e.g. "Paraphrase Multilingual MiniLM")
- **THEN** the table columns stay aligned (the column grows to fit the value, or the value wraps to additional lines) and no row overflows the table border

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

The CLI SHALL provide a subcommand for inspecting and maintaining the event queue (`queue_tasks`). Subcommands: `queue status [--source PATH] [--status NAME]` — tabular output of task state (columns: type, identity, status, attempts, last_error, next_attempt_at) rendered by the unified console layer as a box-drawing table; the free-form columns identity and last_error wrap to additional lines instead of overflowing; `queue reset-retries [--source PATH] [--identity VALUE]` — resets the retry counter for tasks in `error` status (status→`pending`, attempts→0, next_attempt_at→now) across all event types, after which the background worker re-processes them. The subcommand is additive to the existing ones (`serve`, `db`, `model`, `onnx-runtime`, `load-test`).

> **Contract decision (2026-08-29):** the subcommand was originally designed as `index` but was renamed to `queue` by a human — the name `queue` more accurately reflects the entity (the single state table, shared by watcher/startup/worker) rather than the indexing process. This is an explicit deviation from the original naming in task 1.7.
>
> **Contract decision (2026-09-06):** the queue became a generic event queue (`queue_tasks`, human decision — event-queue-and-incremental-linking change). The `--path` filter was renamed to `--identity` because the row key is now the event identity (path for `doc:*` events, document id for `entity:link`), and `queue status` prints the event `type` column. The `queue` subcommand name, actions, and `--source`/`--status` filters are preserved.
>
> **Contract decision (2026-09-09):** the `queue status` visual layout changed from fixed-width dash-separated lines to a box-drawing table rendered by the unified console layer; long identity paths and last_error texts wrap to additional lines. The column set, filters, and reset behavior are unchanged.

#### Scenario: Status
- **WHEN** invoking `synopsis queue status`
- **THEN** a table of all tasks in the `queue_tasks` queue is printed with columns type/identity/status/attempts/last_error/next_attempt_at; the `--source` filter narrows the output to a single source (doc events), `--status` — to a single status

#### Scenario: Status of a link task
- **WHEN** an `entity:link` task exists for a document
- **THEN** `synopsis queue status` prints a row with type `entity:link` and identity equal to the document id

#### Scenario: Long error text wraps
- **WHEN** a task's last_error text is longer than the table's wrap width
- **THEN** the LAST_ERROR cell wraps to additional lines within the table border and the remaining columns of that row stay aligned

#### Scenario: Reset retries
- **WHEN** invoking `synopsis queue reset-retries --identity <doc-id>` for an `entity:link` task in `error` status
- **THEN** the task moves to `pending` with attempts=0; the next background-worker cycle re-processes it (the row is no longer in `error` status)

### Requirement: db subcommand

A subcommand for dataset database maintenance. It SHALL provide three actions: `db stats` — prints dataset and knowledge-DB statistics (document, chunk, entity, entity_link, fact and queue-job counts) as an aligned key-value block rendered by the unified console layer, read-only, no modification and no confirmation; `db clear` — prints the same statistics, asks for confirmation (`Confirm deletion? [y/N]` on stdin) and on `y`/`Y` deletes the entire dataset state directory (`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db` and the vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`, ignoring a missing directory). This atomically removes both the SQLite DB and the vectors in one call. `db merge-entities <id> --into <id>` — merges one entity into another: verifies that both ids exist and share the same `type` and `domain` (otherwise a clear error, no modification), asks for confirmation (`Confirm merge? [y/N]` on stdin), and on `y`/`Y` performs the transactional merge (see the data-schema spec: facts, chunk links, sources, and links re-pointed; both names recorded as aliases of the surviving entity; the duplicate row deleted) and prints a summary of re-pointed row counts and the surviving name; any non-`y` input aborts without changes. Both `db clear` and `db merge-entities` are one-shot and do not load the embedding model / ONNX (they open only the dataset-bound DB, then close it before any deletion or write). After `db clear` a `serve` restart is required so the startup reconcile re-enqueues the files and the background worker re-embeds the documents and recreates the DB + vector index.

> **Contract decision (2026-09-09):** the `db stats` / `db clear` statistics block changed from fixed-width dash-separated lines to an aligned key-value block rendered by the unified console layer. The set of printed counts and the confirmation prompt are unchanged.

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

#### Scenario: Merge with confirmation
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked for two existing entities of the same type and domain and the answer is `y`
- **THEN** the transactional merge is applied, a summary (re-pointed fact/chunk/source/link counts, the surviving name, the recorded aliases) is printed, and the duplicate row no longer exists

#### Scenario: Merge precondition failure
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked with a nonexistent id, or for entities whose types or domains differ
- **THEN** a clear error is printed and the DB is not modified

#### Scenario: Merge aborted
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no merge happens; the command exits without modifying the DB

### Requirement: console output presentation

All human-readable one-shot CLI output (tables and key-value blocks of `model`, `queue`, `db`, and `onnx-runtime`) SHALL be rendered by a single presentation layer with a consistent look: box-drawing tables (with a header row and border), aligned key-value blocks, and section headers. Output MUST be deterministic and free of ANSI escape sequences when stdout is not a terminal or when the `NO_COLOR` environment variable is set. Color (success/error/warning/status accents) MAY be emitted only when stdout is a terminal and `NO_COLOR` is not set. When stdout is a terminal, table width SHALL adapt to the terminal width (long values wrap to additional lines); when stdout is not a terminal, table width SHALL be a fixed content-fit width with the same wrap behavior. The `load-test` report and `serve` log output are outside this requirement.

#### Scenario: Piped output is plain
- **WHEN** `synopsis model list` (or `queue status`, `db stats`, `onnx-runtime status`) is piped to a file or another process
- **THEN** the output contains no ANSI escape sequences and is byte-identical across runs for the same input data

#### Scenario: NO_COLOR disables color
- **WHEN** the command runs with a TTY stdout and `NO_COLOR=1` in the environment
- **THEN** the output contains no ANSI escape sequences

#### Scenario: TTY output is colored
- **WHEN** the command runs with a TTY stdout and no `NO_COLOR` variable
- **THEN** success/error/status indicators are emitted with ANSI color codes (e.g. the installed status in `model list`)

#### Scenario: Narrow terminal wraps instead of overflowing
- **WHEN** the command runs with a TTY whose width is narrower than the table's natural width
- **THEN** long values wrap to additional lines and no line exceeds the terminal width
