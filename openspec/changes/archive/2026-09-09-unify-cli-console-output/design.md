# Design — unified CLI console output

## Context

See proposal.md for motivation. Current state that shapes the approach:

- All human-readable one-shot output lives in `crates/cli` and is rendered by
  four hand-rolled styles: fixed-width dash tables (`model list`,
  `queue status`), hand-padded kv blocks (`model info`, `db stats`,
  `onnx-runtime status`), and the load-test tabwriter table (fixture-pinned —
  out of scope).
- Every render function already takes `out: &mut dyn Write`; unit and
  integration tests drive it with an in-memory `Vec<u8>` and assert exact
  strings. The presentation layer must keep this seam.
- `indicatif` progress bars (embedding downloader, ingestion) and
  `tracing-subscriber` logs (`serve`) are separate channels and stay as-is.
- Toolchain pinned to Rust 1.96.0 — no MSRV pressure; cross-builds include
  `x86_64-pc-windows-gnu`, so Windows console behavior matters.
- `Cargo.toml` documents every workspace dependency with verification notes —
  new dependencies follow the same format.

## Goals / Non-Goals

**Goals:**

- One presentation module (`crates/cli/src/console.rs`) that renders all
  human-readable one-shot CLI output: box-drawing tables, kv blocks, section
  headers, status lines.
- Columns never misalign: width adapts to the terminal when stdout is a TTY,
  fixed deterministic width otherwise; long values wrap, never overflow.
- Color only on TTY without `NO_COLOR`; piped/test output is plain ASCII and
  byte-deterministic.
- `model list` gains the `NAME` column (registry identifier).

**Non-Goals:**

- No rework of the load-test report, `serve` logs, MCP JSON output, indicatif
  bars, or the `db clear` stdin prompt (see proposal Non-goals).
- No new JSON output modes, no interactive widgets, no configuration for the
  output style.

## Decisions

### D1. Table renderer: `tabled` 0.22 (not ratatui, not comfy-table)

Verified against crates.io and the upstream repo (2026-09-09): `tabled`
0.22.0, MIT, `maintenance = actively-developed` (published 2026-09-05),
pure Rust text processing (no network/crypto surface, no known CVEs);
MSRV not declared in crate metadata — the code targets recent stable, far
below our pinned 1.96.0.

Why tabled:

- `Style::modern()` gives the chosen box-drawing look out of the box.
- `Width::wrap(n)` on the whole table: content-fit when narrower, wraps to
  fit `n` when wider — exactly the "adapt to terminal width, wrap otherwise"
  policy, with no per-column width bookkeeping.
- `Table::kv()` (or a borderless `Builder` + `Style::empty()`) renders the
  kv blocks (`model info`, `db stats`, `onnx-runtime status`) with the same
  tool — the "unified approach, not just tables" requirement.
- `Alignment::right()` for numeric columns.
- The `ansi` feature makes width measurement correct for pre-styled (ANSI)
  cell strings (see D2).

Dependency form: `tabled = { version = "0.22", default-features = false,
features = ["std", "ansi"] }` — the default features (`derive`, `macros`,
`assert`) pull a proc-macro crate and a test-only crate we do not use (we
pass string rows, not `#[derive(Tabled)]` structs).

Alternatives rejected:

- **ratatui 0.30** — a TUI framework for interactive full-screen apps: it
  renders through a terminal backend (crossterm), needs terminal-size
  queries and a render loop, and typically the alternate screen. It does not
  render into a `&mut dyn Write`, so it breaks the test seam and is the wrong
  tool for one-shot output that gets piped to files. Heavy dependency tree
  (crossterm, cassowary) for a personal laptop CLI.
- **comfy-table 8.0** — solid and simpler, but the project declares itself
  "finished" (feature freeze) and is searching for a new maintainer
  (upstream issue #202); it has no kv mode, so kv blocks would stay
  hand-rolled and the "one approach" goal would not be met.
- **console 0.16** — a terminal abstraction (colors/values/spinners), not a
  table library; its spinner would overlap indicatif, and it pulls
  libc/windows-sys/terminal_size for needs we cover more cheaply (D2/D3).

### D2. Color: `owo-colors` 4.4, single styling path for all text

Verified (2026-09-09): `owo-colors` 4.4.0, MIT, zero dependencies,
zero-allocation, published 2026-08-27.

The console layer applies color itself (via owo-colors) to cell strings and
status lines — one styling system for tables and non-table text alike.
`tabled`'s `ansi` feature then measures widths through the escape sequences
correctly. Alternative rejected: tabled's native `Colorization` for cells +
a second color API for non-table text = two styling systems for one layer.

Palette (minimal, semantic): green = success/installed, red = error,
yellow = pending, blue = processing, dim = secondary (e.g. `not installed`,
borders stay uncolored), bold = section headers.

Color gating: `std::io::IsTerminal` on stdout (std since 1.70, no crate) —
color only when stdout is a TTY **and** `NO_COLOR` is not set (per
no-color.org, presence of the variable with any value disables color).
Tests (in-memory buffers) are never TTYs → plain, deterministic output with
no test-side color handling.

### D3. Width: `terminal_size` 0.4 on TTY, fixed 120 otherwise

Verified (2026-09-09): `terminal_size` 0.4.4, MIT OR Apache-2.0, 200M+
downloads, published 2026-03-23, minimal (libc/windows-sys) — the standard
small crate for this.

Policy (human decision: adapt to terminal width when possible):

- TTY stdout: `max_width = max(terminal_size::terminal_width(), 40)`;
  on query failure fall back to 120.
- Non-TTY stdout (pipe/file/test): `max_width = 120` — deterministic.
- Applied as one `Width::wrap(max_width)` on the whole table: the table is
  content-fit when narrower, wraps long values when wider; no line ever
  exceeds `max_width`.

Alternative rejected: a fixed width in all cases (loses the narrow-terminal
behavior the user asked for) or crossterm for size queries (heavy, and
comfy-table's `tty` feature is the only reason anyone pulls it in — we do
not use comfy-table).

### D4. Layer shape: `Console` struct returning `String`s

```
crates/cli/src/console.rs
  Console { color: bool, max_width: usize }
    Console::stdout()          // production: TTY detect + NO_COLOR + terminal width
    Console::plain(max_width)  // deterministic (tests, and the non-TTY production path)
    table(headers, rows, right_aligned: &[usize]) -> String   // tabled Builder + Style::modern()
    kv(pairs: &[(&str, &str)]) -> String                      // Table::kv() / borderless Builder
    header(title: &str) -> String                              // bold section title
    success|warn|fail|info(msg) -> String                      // ✓/⚠/✗/• + semantic color
    line(text) -> String                                       // plain line (files lists, bench header)
```

- Render functions return `String`; the existing `out: &mut dyn Write` seam
  and all error handling stay in the command modules (`model.rs`, `queue.rs`,
  …). The layer is pure formatting — no I/O, no config, no globals.
- `Console::stdout()` is the only place TTY/`NO_COLOR`/terminal-width are
  read, so the gating logic is testable in one spot and the command modules
  never touch it.
- Per-column color (e.g. the STATUS column) is applied by the caller as a
  styled string before passing rows in (the layer exposes `style(text, color)`
  or equivalently the status helpers), keeping `table()` generic.

### D5. Layout rules

- Tables: `Style::modern()` (box-drawing `┌─┬─┐`), header row, numeric
  columns right-aligned (DIM, ATTEMPTS, NEXT_ATTEMPT_AT).
- `model list` columns: `NAME` (registry identifier), `DISPLAY` (display
  name), `DIM`, `STATUS` — former `NAME` column renamed, all fields kept.
- Free-form columns (DISPLAY, IDENTITY, LAST_ERROR) wrap via the whole-table
  `Width::wrap`; no per-column caps (the table-wide bound subsumes them).
- kv blocks: no borders, label column auto-width, values left-aligned
  (`Table::kv()`; if its default look does not fit, borderless `Builder` +
  `Style::empty()` — same module, decided at implementation time).
- Section headers: a single bold title line (e.g. `Available Models:`); the
  old 90-dash separators are dropped — box borders replace them.
- Status text: `installed ✓` green, `not installed` dim; queue statuses
  `pending` yellow, `processing` blue, `error` red, `done` green;
  `✓ …` success lines green, `Error: …` red (stderr, TTY-gated).

### D6. Contract decision (CLI surface)

The cli-surface spec fixes `model`/`onnx-runtime` output and the `queue`
status column set. This change alters **visual layout only** (box-drawing,
wrapping, color-on-TTY) plus **adds one column** (`NAME`) to `model list`.
Every field, flag, filter, exit code, the `Confirm deletion? [y/N]` prompt
text, and the `--help` fixture contract are preserved. Recorded as contract
decisions (2026-09-09) in the spec delta. Parity confirmation:

- `--help` fixtures: untouched (clap help is not rendered by this layer).
- Behavioral scenarios (columns, filters, exit codes, prompt): preserved and
  re-verified by the updated test suite.
- load-test recorded fixture: untouched (module not migrated).
- New machine checks: every rendered line's display width ≤ `max_width`
  (overflow guard), piped output contains no ANSI escapes.

## Risks / Trade-offs

- [tabled whole-table wrap distribution can look uneven on very narrow
  terminals] → width floor of 40 columns; personal laptop target; manual
  spot-check at 80/100/120 columns in review.
- [Box-drawing glyphs and ANSI on Windows consoles] → Windows 10+ conhost
  and Windows Terminal handle VT sequences and UTF-8 by default; legacy
  Win7/8 not a target. Cross-build (windows-gnu) compiles the same code;
  worst case on a legacy console is visible escape sequences, not a crash.
- [`tabled` `ansi` feature adds `ansi-str` + `ansitok`] → both are small pure
  Rust parsing crates, no native code — the zigbuild matrix is unaffected
  (verified by the CI gate).
- [Byte-exact test churn] → many assertions in `model.rs`/`queue.rs`/`db.rs`/
  `onnx_runtime.rs`/`tests/cli.rs` are rewritten; mechanical, machine-checked
  by `cargo test`; assertions shift from "exact spacing" to "content +
  structure + width bound", which is more robust than before.
- [Color output is hard to CI-test on a real TTY] → the color path is
  deterministic given the `Console { color, max_width }` inputs; unit tests
  cover both `color=true` (asserts presence of escape sequences) and
  `color=false` (asserts none). Real-TTY behavior is a manual smoke check.
- [New dependencies in a frozen stack] → explicit user-approved extension
  (2026-09-09), recorded in the workspace `Cargo.toml` verification comments
  and `openspec/config.yaml`.

## Migration Plan

1. Task 1.1: add the three workspace deps + `console.rs` + its unit tests —
   no command module changes yet (dead-code-free via `#[allow(dead_code)]`
   is NOT used; the module is `pub(crate)` and the first callers land in
   1.2/1.3, so 1.1 ships the module with its tests only).
2. Task 1.2: migrate `model.rs` (+ `NAME` column) and `onnx_runtime.rs`,
   update their tests and `site/docs/guides/model-management.mdx`.
3. Task 1.3: migrate `queue.rs` and `db.rs`, update their tests.
4. Rollback: `git revert` of the change's commits; no data or schema
   involvement, no forward-migration concern.

## Open Questions

None — palette hues, the kv rendering variant (`Table::kv()` vs borderless
`Builder`), and exact width floor are implementation details verifiable in
review without changing specs or the task breakdown.
