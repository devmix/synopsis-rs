# Design: enforce the `merge-entities` positional

## Reference

- Contract: `openspec/specs/cli-surface/spec.md` — the "db subcommand"
  requirement documents `db merge-entities <id> --into <id>` (both arguments
  required).
- Code: `crates/cli/src/cli.rs`, `build_db_command()` (the `from_id` and `into`
  args) and the `db` dispatch parse fall-through.
- No recorded fixtures are the reference (none exist for this command's help).

## D1 — Make `from_id` required (chosen)

Add `.required(true)` to the `from_id` `Arg` in `build_db_command()`.

- Why: clap then rejects `merge-entities --into <id>` (missing positional) with
  a standard usage error (non-zero exit, no panic). This is the minimal change
  that aligns the parser to the documented signature.
- Why not the alternatives:
  - *Leave it optional and handle `None` in the match:* would require replacing
    the `unreachable!()` with a manual error path (e.g. `.expect()`/`.unwrap()`
    or a hand-built message). `.expect()`/`.unwrap()` are warn-level clippy
    lints that fail the `-D warnings` gate, and a manual error path duplicates
    what clap already does for a required arg. Rejected.
  - *Change the arg to a `--from` flag:* would change the documented
    `merge-entities <id> --into <id>` signature (a frozen-contract change).
    Rejected.

## D2 — Keep the defensive `unreachable!()` fall-through (chosen)

After D1, clap guarantees both `from_id` and `into` are present, so the
`(Some, Some)` match arm is the only reachable one and the `_ => unreachable!()`
arm is correctly unreachable. Its message ("clap enforces the required ID
positional and --into flag") is now accurate.

- Why not remove it: removing it would force `.expect()`/`.unwrap()` on the
  `get_one` calls, which fail the `-D warnings` gate. Keeping it is idiomatic
  (documents the clap invariant) and gate-safe.

## D3 — Regression test (chosen)

Add a binary-level test in `crates/cli/tests/db_cli.rs` (the existing home for
`db` subcommand tests; it invokes the compiled `synopsis` binary via `Command`
and inspects exit code + stdout/stderr). The missing-positional clap error fires
before any config/DB access, so the test needs no fixture:

- `db merge-entities --into 42` (no positional) → non-zero exit, no `panicked`
  in stdout/stderr, and a clap usage error naming the missing `<ID>`.
- The valid `merge-entities <id> --into <id>` path is already covered by the
  existing merge tests in that file (no new test needed).

- Why binary-level (not a `parse_from` unit test): the crate's `db` tests are
  binary-level by convention; the panic occurs in the real process, so testing
  the real process exit + output is the faithful regression guard.
