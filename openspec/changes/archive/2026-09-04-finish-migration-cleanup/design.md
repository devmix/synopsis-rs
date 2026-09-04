# Design: finish-migration-cleanup

Companion to `cleanup-oracle-references` (archived). Same REMOVE/KEEP rule; this
change finishes the files that were outside that change's `3.1` scope and removes
two migration-era artifacts.

## Rule (REMOVE / KEEP)

- **REMOVE:** any mention of the Go project / oracle (`oracle`, `the original`,
  `Go original/code/binary/service/project`, `../synopsis/...`, `Go oracle`),
  Go source file/symbol names, and port/migration language (`ported`,
  `re-architected`, `not transcribed`, `functional copy/rewrite`, "deviations
  from the oracle", "verified against the oracle", "parity with the oracle",
  "the Go oracle's make build-all", "oracle style").
- **KEEP (reframe):** the design rationale / WHY, reframed as a native Rust
  decision (e.g. "deviation from the oracle v5 schema" → "deliberate deviation
  from the legacy v5 schema shape"; "wire compatibility with the Go oracle's
  transport" → "wire compatibility with the legacy transport"; "byte-matched to
  the Go oracle" → "byte-matched to the recorded wire format"; "the Go oracle's
  make build-all platforms" → "the legacy build matrix"). Behavioral and
  algorithm descriptions, the project's own `D1…D8` / `ADR 0001…0005` refs, and
  wire-format versions (`mcp-go v0.57.0`, minus "the oracle's") stay.
- **DO NOT remove** the legitimate **DB-migration** concept (`migrations`,
  `PRAGMA user_version`, "the v5 schema shape", "the legacy v5 schema") — that is
  a real Rust/SQLite concept, not the Go-project migration. Reframe "the oracle
  v5 schema" → "the legacy v5 schema" (the shape, not the project).

## Acceptance pattern

```
rg -i '\.\./synopsis|oracle|Go (original|code|binary|service|project)|\bported\b|re-architected|not transcribed|functional (copy|rewrite)' <scope>  →  0
```

Notes: `\bported\b` is word-bounded (the substring `ported` inside
`supported_extensions` / `reported` / `supported` is a false positive — do NOT
rename public API). `oracle` is targeted everywhere it appears.

## Scope specifics

### Task 1.1 — remove the v5 parity fixture
- `fixtures/`: delete the whole directory (`knowledge.db` is a gitignored binary;
  `README.md` is the tracked provenance record).
- `crates/db/src/test_util.rs`: remove `fixture_db()`, `fixture_path()`,
  `repo_root()` (only used by `fixture_path`), the `fixture_db_opens_read_only`
  self-test, and the now-unused `use rusqlite::OpenFlags;` import. Update the
  module doc (line 3) to drop "and the read-only v5 parity fixture". Keep
  `in_memory_db()`, `TempDb`, `temp_file_db()`, and the `use std::path::{Path,
  PathBuf};` import (`Path` is still used by the `temp_file_db` test).
- `crates/db/tests/fts5_parity.rs`: delete the file (3 tests, all skip-guarded on
  the absent fixture).
- `.gitignore`: remove the fixtures block (the comment + `fixtures/*` +
  `!fixtures/README.md` lines) AND reframe the Cargo.lock comment
  ("parity testing against the Go oracle" → "parity testing against recorded
  fixtures").

### Task 1.2 — reframe stale oracle/Go comments (comments only)
- `migrations/knowledge/1-init/up.sql` (8 refs): "the oracle" / "the Go oracle" →
  "the legacy v5 schema" / "the legacy implementation"; keep every design
  decision + `D`/`ADR` ref + the deliberate-deviation rationale.
- `Cargo.toml` (7 refs, workspace root): "the Go oracle's <X>" → neutral
  ("the legacy transport", "the recorded wire format", "the legacy build
  target"); keep the dependency rationale + versions.
- `.github/workflows/ci.yml` (3 refs): "the Go oracle's make build-all" /
  "oracle style" → neutral ("the legacy build matrix", "cancel-in-progress");
  keep every step, matrix entry, and job.

### Task 1.3 — delete the port-verification report
- `docs/port-verification-report.md`: delete the file. It is referenced nowhere
  (no dangling links). The deliberate decisions it records are already in the
  archived OpenSpec changes + ADRs.

### Task 2.1 — final verification
- The acceptance pattern returns **0** across the whole repo, excluding
  `.opencode/node_modules/**`, `openspec/changes/archive/**`, and `.archive/**`
  (the archived spikes crate — a historical migration artifact kept as-is, like
  the OpenSpec archive).
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` all green.
- The archive is untouched (`git status --porcelain -- openspec/changes/archive/`
  empty; `../synopsis` count in the archive unchanged from the post-Change-3
  baseline of 297).
