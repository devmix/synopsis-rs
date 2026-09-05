# cli-surface Specification

## MODIFIED Requirements

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

### Requirement: load-test subcommand

Benchmark of MCP tools on generated data. Flags: `--scale small|medium|large` (default small), `--seed N` (default 42, PRNG seed for deterministic data generation), `--iterations N` (default 100), `--json PATH`, `--no-fill` (benchmark an existing DB). Report: CALLS, AVG/P50/P95/P99/MAX ms, QPS per tool case; a human-readable report on stdout, and with --json the same report as JSON.

#### Scenario: Report
- **WHEN** load-test --scale small finishes
- **THEN** stdout contains a per-case latency table whose structure matches the recorded report fixture (machine-diff by sections)

#### Scenario: Deterministic seed
- **WHEN** load-test runs twice with the same `--seed` (default 42) and the same scale
- **THEN** the generated dataset is identical across runs (deterministic PRNG)
