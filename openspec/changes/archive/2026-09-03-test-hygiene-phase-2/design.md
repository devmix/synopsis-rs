# Design: test-hygiene-phase-2

## Context note

Pure Rust-side test hygiene. No behavior, contract, or dependency change. The
Go oracle is **not** the reference for this change (nothing is being ported);
`../synopsis` must remain untouched. This change continues the pattern and
conventions established by `test-hygiene-phase-1` (archived 2026-09-02).

## Decisions

### D1 — One extraction task per file; movable/stay-inline split is decided per test by the implementer

Each task handles exactly one source file: its inline `#[cfg(test)]` module →
one integration test file under `crates/<crate>/tests/`. Unlike phase 1 (which
baked the exact per-test classification into the task bodies), phase-2 task
bodies state the **rule** and let the fresh implementer apply it to the file it
has in full context:

- A test is **movable** if it exercises behavior through the crate's public
  API, or through items that can be exposed with a narrow
  `#[doc(hidden)] pub mod test_support`.
- A test is **stay-inline** if it reaches private internals that cannot be
  exposed cheaply (widening beyond the narrow seam would be a production-logic
  change). Those stay in the file's `#[cfg(test)]` block.

*Why not bake exact names:* 7 files × ~300–1,100 test lines each is a large
per-file analysis; the rule + a hard machine gate (see D3) is sufficient to
keep the result deterministic and safe, and matches how the implementer
naturally works (it reads the whole file).

### D2 — Pure moves are exempt from the ~500-line "new code + tests" cap

Extracting a ~1,100-line test module is a pure move (delete from source + add
to the integration file, zero new logic). Its diff exceeds the ~500-line cap on
a literal reading. The cap exists to bound *new logic* per fresh agent
(~100k context); a mechanical move is well within that budget. (Same
justification as phase-1 D2; user decision 2026-09-01.)

### D3 — Machine-checkable acceptance: test count unchanged, gates green, file shrinks

Every extraction task is gated by:
1. `cargo fmt --all --check` clean.
2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
3. `cargo test --workspace` green, with the crate's test count **unchanged**
   (relocation only — no test added or removed). The implementer reports the
   before/after `cargo test -p <crate> -- --list` count.
4. The source file's line count drops by roughly the relocated test-module
   lines (it may retain a reduced `#[cfg(test)]` block for stay-inline tests,
   as phase-1 did for `search/hybrid.rs`, `graph/linker.rs`, `sse.rs`).

If a test is mis-classified as movable but actually needs a private item, the
integration test fails to compile (E0624 / E0432 / E0364) — the gate catches
it, so the hard errors are self-enforcing.

### D4 — `test_support` seams reuse the phase-1 E0364/E0432 patterns

Integration tests link against the non-test lib build, so `pub(crate)` items and
inherent methods are unreachable. Where a moved test needs them, the task adds
a `#[doc(hidden)] pub mod test_support` using the phase-1 patterns:
- thin `pub fn` delegates for `pub(crate)` methods (E0432: `pub use` cannot
  re-export inherent methods);
- `pub const X = crate::…::X;` compile-time references for `pub(crate)`
  constants (E0364: `pub use` cannot re-export a `pub(crate)` item).

Crates that did not have a `test_support` seam in phase 1 (`config`, `db`,
`vectors`, `cli`) get one only if a moved test needs it; `mcp` already has one
(phase-1).

### D5 — Integration test file naming

Follow the phase-1 convention (module/file name, `_units` suffix where the
bare name is ambiguous or already taken):

| Source file | Integration test file |
|---|---|
| `config/src/preset.rs` | `config/tests/preset.rs` |
| `db/src/entity.rs` | `db/tests/entity.rs` |
| `mcp/src/server.rs` | `mcp/tests/server_units.rs` |
| `cli/src/serve/server.rs` | `cli/tests/serve_server.rs` |
| `mcp/src/tools/dossier.rs` | `mcp/tests/dossier.rs` |
| `vectors/src/usearch/mod.rs` | `vectors/tests/usearch_units.rs` |
| `config/src/ontology.rs` | `config/tests/ontology.rs` |

## Oracle reference

None — this is Rust-side test hygiene, not a port. `../synopsis` is not
consulted and must remain untouched.
