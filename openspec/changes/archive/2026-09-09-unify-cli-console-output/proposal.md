# Unify CLI console output

## Why

Human-readable CLI output is rendered by four hand-rolled styles with hardcoded
column widths (`model list`, `queue status`, kv blocks in `db stats` /
`model info` / `onnx-runtime status`, and the load-test tabwriter table). The
hardcoded widths misalign as soon as data outgrows them — today
`model list` breaks on the registry entry `Paraphrase Multilingual MiniLM`
(30 chars in a 25-char column), and `queue status` will break on any long
path or error message. There is no single presentation approach, no color,
no terminal awareness, and `model list` does not show the registry `name`
(the identifier used by `model download/delete/info`).

## What Changes

- New presentation module `crates/cli/src/console.rs` — the single output
  layer for all human-readable CLI output: table rendering (box-drawing
  style), key-value blocks, section headers, and status text helpers.
  Renders to `String` into the existing `&mut dyn Write` seam, so tests stay
  deterministic (no TTY → no color, fixed width policy).
- **BREAKING (visual only):** `model list` output layout changes — box-drawing
  table, columns `NAME / DISPLAY / DIM / STATUS` (new `NAME` column with the
  registry identifier; former `NAME` renamed to `DISPLAY`), all fields kept.
- `queue status`, `db stats`, `model info`, `onnx-runtime status/install`
  migrate to the same layer: box-drawing tables / kv blocks, free-form
  columns (IDENTITY, LAST_ERROR) wrap to multiple lines instead of
  overflowing.
- Color (green/red/yellow/blue/dim) emitted only when stdout is a TTY and
  `NO_COLOR` is not set; piped and test output stays plain ASCII.
- Table width adapts to the terminal width when stdout is a TTY; non-TTY
  output uses a fixed content-fit width with a wrap cap (deterministic).
- New workspace dependencies (verified 2026-09-09, see design.md): `tabled`
  0.22 (feature `ansi`), `owo-colors` 4.4, `terminal_size` 0.4.
- Documentation updated in the same change: `site/docs/guides/model-management.mdx`
  sample output.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `cli-surface`: the `model` subcommand output is fixed by this contract —
  an explicit contract decision changes the `model list` layout (box-drawing
  table, new `NAME` column, `DISPLAY` rename) and the visual layout of
  `queue status` / `db stats` / `onnx-runtime` outputs; every field, column
  set, and behavior (filters, exit codes, confirmation prompt) is preserved.

## Impact

- Code: `crates/cli` only — new `src/console.rs`; `src/model.rs`,
  `src/queue.rs`, `src/db.rs`, `src/onnx_runtime.rs` migrated; their unit
  tests and `tests/cli.rs` assertions updated to the new layout.
  `src/loadtest/report.rs` is NOT touched (fixture-pinned, machine-oriented).
- Dependencies: `Cargo.toml` (workspace) + `crates/cli/Cargo.toml` +
  `Cargo.lock` — `tabled`, `owo-colors`, `terminal_size`.
- Frozen contracts: CLI surface (visual layout of four subcommands' output +
  one new column) — explicit contract decision, see specs delta. MCP tools,
  data schema, config formats, `--help` text, and the load-test report are
  unchanged. Parity: `--help` fixture diff unaffected; behavioral scenarios
  (columns, filters, exit codes, prompt text) preserved and re-verified by
  the updated test suite; the load-test recorded fixture is untouched.
- Docs: `site/docs/guides/model-management.mdx` (sample `model list` output).

## Non-goals

- No changes to the `load-test` report (its tabwriter table is pinned by the
  recorded report fixture; it also has a JSON output).
- No changes to `serve` log formatting (tracing-subscriber, stderr channel).
- No interactive prompt rework (`db clear` keeps its `Confirm deletion? [y/N]`
  stdin prompt; no `dialoguer` or similar).
- No MCP tool output changes (MCP responses are JSON, not console output).
- No restyling of `indicatif` progress bars (they stay the progress standard).
- No JSON output modes for the migrated subcommands.
