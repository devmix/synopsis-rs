# Tasks — unify-cli-console-output

Order reflects dependencies: 1.1 → 1.2 → 1.3 → 1.4. Each task is executed by a
fresh agent; the body is self-contained (goal, file scope, dependencies,
acceptance criteria, references). Read `design.md` (decisions D1–D6) and
`openspec/specs/cli-surface/spec.md` (post-change contract, contract decisions
2026-09-09) before starting any task.

## 1. Console layer

- [x] 1.1 Add `tabled`/`owo-colors`/`terminal_size` dependencies and implement the `console` presentation module with unit tests

  **Goal.** Create the single presentation layer for human-readable one-shot
  CLI output (design D1–D5): a `Console` struct that renders box-drawing
  tables, kv blocks, section headers, and status lines to `String`, with
  TTY-gated color and terminal-width adaptation. No command module is
  migrated yet — this task ships the module + its tests only.

  **Dependencies.** None (first task). Verified crate facts to record in the
  workspace `Cargo.toml` comment (format: follow the existing per-dependency
  verification notes, e.g. the `clap`/`tracing` entries): `tabled` 0.22.0
  (MIT, actively developed, published 2026-09-05, pure Rust text processing,
  no known CVEs, MSRV not declared — targets recent stable, below pinned
  Rust 1.96.0; verified 2026-09-09), `owo-colors` 4.4.0 (MIT, zero
  dependencies, zero-allocation, published 2026-08-27), `terminal_size`
  0.4.4 (MIT OR Apache-2.0, minimal libc/windows-sys, published 2026-03-23).

  **File scope.**
  - `Cargo.toml` (workspace): add `tabled = { version = "0.22", default-features = false, features = ["std", "ansi"] }`, `owo-colors = "4.4"`, `terminal_size = "0.4"` with the verification comment. Do NOT enable tabled default features (`derive`/`macros`/`assert` pull a proc-macro and a test-only crate we do not use).
  - `crates/cli/Cargo.toml`: add the three as `workspace = true`.
  - `Cargo.lock`: updated by cargo.
  - New `crates/cli/src/console.rs` + module declaration in `crates/cli/src/lib.rs` (`pub mod console;`).

  **Module contract** (design D4/D5; the migration tasks 1.2/1.3 code against
  exactly this API):
  - `pub struct Console { color: bool, max_width: usize }` — all methods
    return `String` (pure formatting, no I/O, no globals).
  - `Console::stdout() -> Console` — production constructor: `color =
    std::io::IsTerminal on stdout AND NO_COLOR env var absent` (presence with
    any value disables color, per no-color.org); `max_width =
    max(terminal_size::terminal_width(), 40)` when stdout is a TTY and the
    query succeeds, else `120`.
  - `Console::plain(max_width: usize) -> Console` — `color = false` (tests +
    deterministic non-TTY rendering).
  - `table(&self, headers: &[&str], rows: &[Vec<String>], right_aligned: &[usize]) -> String` — tabled `Builder` + `Style::modern()` (box-drawing), header row, numeric columns right-aligned per `right_aligned`, whole-table `Width::wrap(self.max_width)` (content-fit when narrower, wraps when wider; no line may exceed `max_width` display columns).
  - `kv(&self, pairs: &[(&str, &str)]) -> String` — aligned key-value block, no borders, label column auto-width (tabled `Table::kv()`; if its default look does not fit design D5, use borderless `Builder` + `Style::empty()` — decide at implementation time, keep the output shape: `Label:   value` rows).
  - `header(&self, title: &str) -> String` — single section-title line, bold when `color` (the old dash separators are gone).
  - `success|warn|fail|info(&self, msg: &str) -> String` — prefixed status lines: `✓ ` (green), `⚠ ` (yellow), `✗ ` (red), `• ` (blue) + message; prefix uncolored when `!color`.
  - `style(&self, text: &str, c: Color) -> String` + `pub enum Color { Green, Red, Yellow, Blue, Dim, Bold }` — owo-colors styling, no-op when `!color` (callers use this for per-cell colors, e.g. the STATUS column).
  - Public items carry doc comments (`missing_docs = "deny"` workspace lint); no `unsafe` (workspace `unsafe_code = "forbid"`); no new `unwrap`/`expect` outside `#[cfg(test)]` modules (clippy `unwrap_used`/`expect_used` are warn-level, gate runs `-D warnings`).

  **Acceptance criteria.**
  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test -p cli` green.
  - Unit tests in `console.rs` (`#[cfg(test)]`, may opt out of `unwrap_used`/`expect_used` locally like other cli test modules):
    - `plain` console: `table` output contains box-drawing chars (`┌`, `┬`, `┐`), header cells, and **no** ANSI escape bytes (`\x1b[`) for both `color=false` and `color=true`-disabled styling; `kv` output aligns values (same label padding); `header`/`success`/`warn`/`fail`/`info` produce the expected plain lines.
    - `color=true`: `success("x")` contains `\x1b[32m` (green) and `style("s", Color::Red)` contains `\x1b[31m`; `table` with a pre-styled cell still reports correct column alignment (tabled `ansi` feature measures through escapes — assert the second column starts at the same byte offset in styled and unstyled rows of equal plain length).
    - Wrapping: a row with a 300-char cell rendered with `max_width = 60` produces lines whose display width (excluding ANSI) ≤ 60, and the cell text is fully present (wrapped, not truncated).
    - `Width` floor: `table` with `max_width = 30` behaves like 40 (constructor floor applies at `Console::stdout()` level; `plain(30)` renders without panic).
  - No changes to any file outside the listed scope.

## 2. Migration — `model` and `onnx-runtime`

- [x] 1.2 Migrate `model.rs` and `onnx_runtime.rs` to the console layer; add the `NAME` column to `model list`; update tests and docs

  **Goal.** Route all human-readable output of the `model` and `onnx-runtime`
  subcommands through the `console` module (design D5); change `model list`
  to a box-drawing table with columns `NAME` (registry identifier),
  `DISPLAY` (display name — the former `NAME` column, renamed), `DIM`,
  `STATUS` (contract decision 2026-09-09, see
  `openspec/changes/unify-cli-console-output/specs/cli-surface/spec.md`);
  `model info` and `onnx-runtime status` become kv blocks; status messages
  use the `success`/`fail` helpers.

  **Dependencies.** Task 1.1 complete (the `console` module API from its
  body).

  **File scope.**
  - `crates/cli/src/model.rs`: `list_models` (table via `Console::table`, STATUS cell styled: `installed ✓` green, `not installed` dim), `print_model_info` (kv block: Name, Display Name, Description, Version, Vector Dim, Source, Repository, Installed At when installed, Status; the `Files:` list stays a plain indented line list rendered via `Console::line`-style plain writes), `benchmark` (section header via `header`, hardware lines plain), `download_model`/`delete_model` (`✓ …` via `success`). The `&mut dyn Write` seam and `CliError` handling stay as-is; build `Console::stdout()` once per flow (in `model_flow`) and pass it down.
  - `crates/cli/src/onnx_runtime.rs`: `status` (kv block + `Supported Platforms:` list), `install`/`uninstall` status lines via `success`.
  - Tests inside both modules: update exact-string assertions to the new layout. Assertions shift from exact spacing to content + structure + width bound: header/title lines still present (`Available Models:` etc. — integration tests rely on these), all four column headers `NAME`/`DISPLAY`/`DIM`/`STATUS` present, every line's display width ≤ 120 (the non-TTY `max_width`). Add a registry fixture entry with a 30+ char display name (e.g. `Paraphrase Multilingual MiniLM`) and assert the table stays aligned (no line exceeds 120, both NAME and DISPLAY values present).
  - `crates/cli/tests/cli.rs`: keep/adjust the `Available Models:` and `ONNX Runtime Status` title assertions (titles survive the migration).
  - `site/docs/guides/model-management.mdx`: replace the sample `model list` output block with the new box-drawing table (plain, non-TTY rendering, all three shipped registry entries incl. `Paraphrase Multilingual MiniLM`), and mention the new `NAME` column in the surrounding sentence.

  **Acceptance criteria.**
  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test -p cli` green (including `tests/cli.rs`).
  - `model list` with the three shipped registry entries (`workspace/configs/onnx.yaml`: `bge-m3-int8`, `bge-small-en-v1.5`, `paraphrase-multilingual-MiniLM-L12-v2`) renders a box-drawing table where the NAME column shows the registry identifiers and no line exceeds 120 display columns.
  - No ANSI escape bytes in any test-captured output (tests render non-TTY).
  - Exit codes, error messages (e.g. `model {name:?} not found in registry`), and the `Files:` content of `model info` unchanged.
  - No changes to `queue.rs`, `db.rs`, `loadtest/`, or files outside the listed scope.

## 3. Migration — `queue` and `db`

- [x] 1.3 Migrate `queue.rs` and `db.rs` to the console layer; update tests

  **Goal.** Route all human-readable output of the `queue` and `db`
  subcommands through the `console` module (design D5): `queue status`
  becomes a box-drawing table (columns unchanged: TYPE, IDENTITY, STATUS,
  ATTEMPTS, LAST_ERROR, NEXT_ATTEMPT_AT; IDENTITY/LAST_ERROR wrap instead of
  overflowing; STATUS cell colored: `pending` yellow, `processing` blue,
  `error` red, `done` green), `db stats` becomes a kv block (Documents,
  Chunks, Entities, Entity links, Facts, Queue tasks), `queue
  reset-retries` result lines use `success`.

  **Dependencies.** Task 1.1 complete. (Ordered after 1.2 for review
  cadence; no code dependency on 1.2.)

  **File scope.**
  - `crates/cli/src/queue.rs`: `print_status` (table via `Console::table`, right-aligned ATTEMPTS + NEXT_ATTEMPT_AT, `N tasks` footer line kept), `reset_retries` result message via `success`. Build `Console::stdout()` once per flow.
  - `crates/cli/src/db.rs`: `print_stats` (kv block via `Console::kv` under the `Dataset Statistics:` header). `confirm_deletion` is UNCHANGED — the `Confirm deletion? [y/N]` prompt text and stdin read are a contract (cli-surface spec, db subcommand).
  - Tests inside both modules: update exact-string assertions to the new layout (content + structure + width bound, as in task 1.2). Add a queue fixture task with a 300-char `last_error` and assert every rendered line ≤ 120 display columns and the error text is fully present (wrapped, not truncated).
  - `crates/cli/tests/queue_cli.rs` / `crates/cli/tests/db_cli.rs`: keep/adjust the `Event Queue:` / `Dataset Statistics:` title assertions (titles survive).

  **Acceptance criteria.**
  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test -p cli` green (including `tests/queue_cli.rs`, `tests/db_cli.rs`).
  - `queue status` column set and order unchanged; `--source`/`--status`/`--identity` filters and `reset-retries` behavior unchanged; exit codes unchanged.
  - The `Confirm deletion? [y/N]` prompt byte-identical (asserted by existing tests).
  - No ANSI escape bytes in any test-captured output.
  - No changes to `model.rs`, `onnx_runtime.rs`, `loadtest/`, or files outside the listed scope.

## 4. Wrap-priority fix (follow-up from 1.3)

- [x] 1.5 Fix width-wrap priority in `Console::table`/`kv`: the widest column wraps first

  **Goal.** When one cell dominates the width budget (e.g. a 300-char
  `LAST_ERROR` in `queue status`), the other columns must keep their
  content-fit width and the excess must be absorbed by wrapping the widest
  column. The default `Width::wrap` (tabled `Priority::none()`) peaks
  columns left-to-right, which collapses the earlier columns to minimal
  width in that row (task 1.3 deviation report).

  **Dependencies.** Tasks 1.1 (console module) and 1.3 (queue migration that
  exposed the issue) completed.

  **File scope.**
  - `crates/cli/src/console.rs`:
    - `table()`: replace `Width::wrap(self.max_width)` with
      `Width::wrap(self.max_width).priority(Priority::max(false))` (tabled
      `peaker::Priority::max` peaks the currently widest column first; the
      bool prefers the left column on ties).
    - `kv()`: same change (the value column is the wide one).
    - Verify the peaker semantics against the tabled 0.22.0 source in the
      cargo registry (`src/settings/peaker/`) before applying; if
      `Priority::max` does not achieve "widest wraps first, others keep
      width", choose the correct peaker and document the choice in the code
      comment.
    - Update the doc comments of `table()`/`kv()` to describe the priority.
    - Add a unit test: a 6-column table (five short
      TYPE/IDENTITY/STATUS/ATTEMPTS/NEXT_ATTEMPT_AT-style columns + one
      300-char LAST_ERROR-style cell), `max_width = 120` → assert (a) every
      line ≤ 120 display columns, (b) the 300-char text is fully present,
      (c) every short column's cell value is visible in full on its row
      (not collapsed to empty).
  - `crates/cli/src/queue.rs` (test-only, revision 2026-09-09): the 1.3
    test `status_long_last_error_wraps_within_width` extracts ALL
    non-empty cells of the wrapped lines — written against the pre-fix
    collapsed rendering. After the fix the first wrapped line contains the
    short-column values, so change the extraction to the LAST_ERROR cell
    only (`.split('│').nth(5)` over the same z-line filter), keeping the
    `concat == long_error` assertion and the test's documented intent
    (wrapped, not truncated). No production code in queue.rs changes.

  **Revision history.**
  - 2026-09-09 (rev 1): orchestrator scope decision — the original file
    scope ("console.rs only" + "queue tests unchanged") was based on the
    wrong assumption that existing tests bound only total line width. The
    1.3 wrap test's extraction is mutually exclusive with the fix's goal
    (short columns visible); a test-only hunk in queue.rs is now in scope.

  **Acceptance criteria.**
  - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
    clean; `cargo test --workspace` green (queue/db/model tests passing,
    the 1.3 wrap test with the revised LAST_ERROR-only extraction).
  - No changes to files outside the listed scope; no production code in
    `queue.rs` changes.

## 5. Records

- [x] 1.4 Record the user-approved stack extension in `openspec/config.yaml`

  **Goal.** Add the three new dependencies to the frozen-stack section of
  `openspec/config.yaml`, following the format of the existing
  user-approved-extension entries (precedent: the `clap 4.6.6` and
  `tracing 0.1.44` bullets): `tabled 0.22` (feature `ansi`, default features
  off), `owo-colors 4.4`, `terminal_size 0.4` — CLI presentation layer
  (change `unify-cli-console-output`, user-approved 2026-09-09):
  box-drawing tables/kv blocks, TTY-gated color, terminal-width adaptation.

  **Dependencies.** Tasks 1.1–1.3 and 1.5 complete (all code merged).

  **File scope.** `openspec/config.yaml` only (the frozen-stack `context`
  section).

  **Acceptance criteria.**
  - The new bullet matches the existing entry style (crate + version + role + change name + approval date).
  - No other file changed; `openspec validate unify-cli-console-output` still passes.
