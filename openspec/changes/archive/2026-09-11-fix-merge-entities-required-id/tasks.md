# Tasks

## 1. Enforce the merge-entities positional

- [x] 1.1 Mark the `from_id` positional required + regression test.
  - **Goal:** `synopsis db merge-entities --into <id>` (omitting the positional
    `<id>`) yields a clean clap usage error (non-zero exit, no panic) instead of
    the current `unreachable!()` panic; the valid `merge-entities <id> --into
    <id>` path is unchanged.
  - **File scope:** `crates/cli/src/cli.rs` (the `from_id` `Arg` in
    `build_db_command()`), and `crates/cli/tests/db_cli.rs` (one new
    binary-level regression test). No other files.
  - **Dependencies:** none — the `db merge-entities` command and the existing
    `db_cli.rs` binary-level test harness (the `run()` helper that invokes the
    compiled `synopsis` binary) already exist.
  - **Change:**
    1. In `build_db_command()` (`crates/cli/src/cli.rs`), add `.required(true)`
       to the `from_id` `Arg`. It is currently:
       `Arg::new("from_id").value_name("ID").action(ArgAction::Set)
       .value_parser(clap::value_parser!(i64)).help(...)`. The `--into` flag
       already has `.required(true)` — leave it untouched.
    2. Do NOT modify the parse fall-through match at the `db` dispatch (the
       `_ => unreachable!("clap enforces the required ID positional and --into
       flag")` arm). Once clap enforces both args it is correctly unreachable
       and its message is accurate. Do NOT introduce `.expect()`/`.unwrap()`
       (they are warn-level lints that fail the `-D warnings` gate).
    3. In `crates/cli/tests/db_cli.rs`, add one binary-level test using the
       existing `run()` helper (which runs the compiled `synopsis` binary and
       returns the `Output`). The clap error fires before any config/DB access,
       so the test needs NO fixture. The test invokes
       `run(&["db", "merge-entities", "--into", "42"])` (note: no positional)
       and asserts:
       (a) the exit status is non-success (`!out.status.success()`);
       (b) neither stdout nor stderr contains the substring `panicked` (the bug
       produced a Rust `unreachable!()` panic); and
       (c) the clap usage error is present — assert stderr contains `<ID>` or
       `required` (the missing-required-argument message).
       Follow the file's existing style (it already has
       `#![allow(clippy::unwrap_used, clippy::expect_used)]` for the harness).
  - **Acceptance:**
    - `synopsis db merge-entities --into 42` (no positional) exits non-zero with
      a usage error naming the missing `<ID>`, and does NOT panic.
    - `synopsis db merge-entities 1 --into 2` still parses and behaves as before
      (already covered by the existing merge tests — do not break them).
    - `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings`
      clean (no new warnings); `cargo test -p cli` green including the new test.
    - No new dependencies; no change to the merge logic or data schema.
  - **Reference:** `openspec/specs/cli-surface/spec.md` ("db subcommand"
    requirement — `db merge-entities <id> --into <id>`); the existing binary-level
    tests in `crates/cli/tests/db_cli.rs` for the harness pattern.
