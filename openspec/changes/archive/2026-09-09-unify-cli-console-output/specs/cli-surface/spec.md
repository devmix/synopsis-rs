# cli-surface delta

## MODIFIED Requirements

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

A subcommand for dataset database maintenance. It SHALL provide two actions: `db stats` — prints dataset and knowledge-DB statistics (document, chunk, entity, entity_link, fact and queue-job counts) as an aligned key-value block rendered by the unified console layer, read-only, no modification and no confirmation; `db clear` — prints the same statistics, asks for confirmation (`Confirm deletion? [y/N]` on stdin) and on `y`/`Y` deletes the entire dataset state directory (`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db` and the vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`, ignoring a missing directory). This atomically removes both the SQLite DB and the vectors in one call. The command is one-shot and does not load the embedding model / ONNX (it opens only the dataset-bound DB to print statistics, then closes it before deletion). After a `db clear` a `serve` restart is required so the startup reconcile re-enqueues the files and the background worker re-embeds the documents and recreates the DB + vector index.

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

## ADDED Requirements

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
