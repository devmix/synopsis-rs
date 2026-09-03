# Tasks: test-hygiene-phase-2

Seven file-extraction tasks (one per file) + a final verification. Order is not
strictly dependency-critical, but they are listed 2.1 → 2.7 for a sequential
pass and 2.8 last. Each body is self-contained for a fresh agent.

## Shared pattern (applies to every extraction task 2.1–2.7)

For the named source file `F` in crate `C`:

1. Read `F` in full. Identify its `#[cfg(test)] mod tests` block.
2. For each `#[test]` / `#[tokio::test]` in that block, classify:
   - **movable** — exercises behavior through `C`'s public API, or through
     items exposable via a narrow `#[doc(hidden)] pub mod test_support`;
   - **stay-inline** — reaches private internals that cannot be exposed cheaply.
3. Move all movable tests into a new integration file
   `crates/C/tests/<name>.rs` (name per the design D5 table). The integration
   file's module doc comment must NOT contain a relative `../synopsis/…` path
   (phase-1 convention).
4. Leave the stay-inline tests in `F`'s (now reduced) `#[cfg(test)]` block. If
   no test stays, delete the block.
5. If a moved test needs `pub(crate)` items, add/extend
   `crates/C/src/test_support.rs` (`#[doc(hidden)] pub mod test_support`) using
   the phase-1 patterns: thin `pub fn` delegates for methods (E0432),
   `pub const X = crate::…::X;` references for constants (E0364). Re-export it
   from `C`'s `lib.rs`/`mod` tree.
6. Do **not** add or change any test logic; do **not** change production
   behavior (only visibility widening to feed `test_support`).

## Per-task acceptance (machine-checkable, applies to 2.1–2.7)

- `cargo fmt --all --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green, and `cargo test -p C -- --list` count for
  crate `C` is **unchanged** (relocation only). Report before/after counts.
- `F`'s line count drops by roughly the relocated test-module lines.
- No new dependency in `crates/C/Cargo.toml` (a re-use of an existing
  production dependency at the same version is the only allowed exception, as
  phase-1 did for `cel` in `graph`).
- `../synopsis` untouched.

---

## 2

- [x] 2.1 Extract `config/src/preset.rs` tests → `config/tests/preset.rs`

**Goal.** Shrink `crates/config/src/preset.rs` (2,427 lines; ~1,114 inline
test lines) by relocating its movable tests to `crates/config/tests/preset.rs`.

**Scope.** `crates/config/src/preset.rs`, new `crates/config/tests/preset.rs`,
and (only if needed) `crates/config/src/test_support.rs` + its re-export in
`crates/config/src/lib.rs`.

**Dependencies.** None. (Follow the shared pattern.)

**Acceptance.** Shared per-task acceptance for crate `config`.

- [ ] 2.2 Extract `db/src/entity.rs` tests → `db/tests/entity.rs`

**Goal.** Shrink `crates/db/src/entity.rs` (1,369 lines; ~883 inline test
lines — 64%, majority-test) by relocating its movable tests to
`crates/db/tests/entity.rs`.

**Scope.** `crates/db/src/entity.rs`, new `crates/db/tests/entity.rs`, and (only
if needed) `crates/db/src/test_support.rs` + re-export.

**Dependencies.** None. (Follow the shared pattern.) Note: `crates/db/src/test_util.rs`
already exists as a `#[cfg(test)]`/test helper — reuse it if the moved tests need
the in-memory DB fixture rather than duplicating it.

**Acceptance.** Shared per-task acceptance for crate `db`.

- [ ] 2.3 Extract `mcp/src/server.rs` tests → `mcp/tests/server_units.rs`

**Goal.** Shrink `crates/mcp/src/server.rs` (1,414 lines; ~657 inline test
lines) by relocating its movable tests to
`crates/mcp/tests/server_units.rs`.

**Scope.** `crates/mcp/src/server.rs`, new `crates/mcp/tests/server_units.rs`,
and (only if needed) `crates/mcp/src/test_support.rs` (already exists from
phase-1 — extend, don't recreate).

**Dependencies.** None. (Follow the shared pattern.)

**Acceptance.** Shared per-task acceptance for crate `mcp`.

- [ ] 2.4 Extract `cli/src/serve/server.rs` tests → `cli/tests/serve_server.rs`

**Goal.** Shrink `crates/cli/src/serve/server.rs` (1,430 lines; ~650 inline
test lines) by relocating its movable tests to
`crates/cli/tests/serve_server.rs`.

**Scope.** `crates/cli/src/serve/server.rs`, new
`crates/cli/tests/serve_server.rs`, and (only if needed)
`crates/cli/src/test_support.rs` + re-export.

**Dependencies.** None. (Follow the shared pattern.)

**Acceptance.** Shared per-task acceptance for crate `cli`.

- [ ] 2.5 Extract `mcp/src/tools/dossier.rs` tests → `mcp/tests/dossier.rs`

**Goal.** Shrink `crates/mcp/src/tools/dossier.rs` (1,287 lines; ~645 inline
test lines) by relocating its movable tests to `crates/mcp/tests/dossier.rs`.

**Scope.** `crates/mcp/src/tools/dossier.rs`, new `crates/mcp/tests/dossier.rs`,
and (only if needed) `crates/mcp/src/test_support.rs` (exists from phase-1 —
extend).

**Dependencies.** None. (Follow the shared pattern.)

**Acceptance.** Shared per-task acceptance for crate `mcp`.

- [ ] 2.6 Extract `vectors/src/usearch/mod.rs` tests → `vectors/tests/usearch_units.rs`

**Goal.** Shrink `crates/vectors/src/usearch/mod.rs` (1,258 lines; ~408 inline
test lines) by relocating its movable tests to
`crates/vectors/tests/usearch_units.rs`.

**Scope.** `crates/vectors/src/usearch/mod.rs`, new
`crates/vectors/tests/usearch_units.rs`, and (only if needed)
`crates/vectors/src/test_support.rs` + re-export.

**Dependencies.** None. (Follow the shared pattern.) Note: usearch tests may
need a temp index/engine; reuse the crate's existing test fixtures where
present.

**Acceptance.** Shared per-task acceptance for crate `vectors`.

- [ ] 2.7 Extract `config/src/ontology.rs` tests → `config/tests/ontology.rs`

**Goal.** Shrink `crates/config/src/ontology.rs` (1,320 lines; ~318 inline
test lines) by relocating its movable tests to
`crates/config/tests/ontology.rs`.

**Scope.** `crates/config/src/ontology.rs`, new
`crates/config/tests/ontology.rs`, and (only if needed)
`crates/config/src/test_support.rs` + re-export (may already exist from 2.1 —
extend, don't recreate).

**Dependencies.** Task 2.1 (if both add `config` `test_support`, 2.7 extends
what 2.1 created). (Follow the shared pattern.)

**Acceptance.** Shared per-task acceptance for crate `config`.

- [ ] 2.8 Final verification: before/after report + full gates

**Goal.** Produce a machine-verified before/after report for the change and
confirm the full workspace is green.

**Scope.** Read-only verification (no source edits). If a discrepancy is found
(test count changed, a file not shrunk, a gate failing), report it — do not
silently fix; the orchestrator routes the fix back to the owning task.

**Dependencies.** Tasks 2.1–2.7 complete.

**What to produce.** A `results.md` in the change directory with:
1. Per-file before/after line counts (git-show-parent vs current) and the
   reduction, for all 7 extracted files.
2. Per-crate test-count reconciliation via `cargo test -p <crate> -- --list`
   (before = the pre-change baseline; after = current), confirming zero delta.
3. Gate outputs: `cargo fmt --all --check`, `cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace` (pass/fail + counts).
4. The residual inline-test inventory: which files retained a `#[cfg(test)]`
   block (stay-inline tests) and why (private-item reason per test).

**Acceptance.** All three gates pass; every crate's test count is unchanged;
`results.md` written.
