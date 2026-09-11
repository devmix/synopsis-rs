# Fix: `db merge-entities` missing positional panics instead of a usage error

## Why

`synopsis db merge-entities --into <id>` — omitting the required positional
`<id>` (the entity to merge away) — panics with `unreachable!()` instead of
producing a clean clap usage error. The `from_id` positional is not marked
required, so clap accepts the invocation and the parse fall-through hits the
defensive `unreachable!()`.

## What Changes

- Mark the `from_id` positional of `db merge-entities` as `.required(true)` so
  clap rejects its omission with a usage error (missing required argument),
  non-zero exit, no panic.
- Add a regression test asserting the missing-positional path yields a clean
  usage error (not a panic). The valid `merge-entities <id> --into <id>` path
  is already covered by existing tests.

No breaking changes. No dependency changes.

## Capabilities

None. This is a code-to-spec alignment fix: the cli-surface contract already
documents `db merge-entities <id> --into <id>` (both arguments required). The
fix makes the parser enforce what the contract already states; no requirement
text changes. Declared via `skip_specs: true`.

## Impact

- Code: `crates/cli/src/cli.rs` (`build_db_command()` — the `from_id` arg).
- Test: `crates/cli/tests/db_cli.rs` (one binary-level regression test).
- Frozen contracts touched: **CLI surface** — the `merge-entities` usage/help
  now marks the positional as required (it was rendered optional). Parity:
  there are no recorded `--help`/usage fixtures in the repo for this command
  (verified), so no fixture is affected; the behavior aligns to the documented
  `merge-entities <id> --into <id>` signature. The no-panic guarantee is pinned
  by the new regression test.

## Non-goals

- No change to the merge logic, data schema, or the `merge_entities()`
  operation.
- No change to any other `db` action or subcommand.
- No change to the `--into` flag (already required).
- No change to the defensive `unreachable!()` fall-through (it becomes
  correctly unreachable once clap enforces the positional).
