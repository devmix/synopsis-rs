# Tasks: finish-migration-cleanup

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. The Go
project at `../synopsis` will be deleted, so the repo must be fully autonomous.
This change finishes the files that were outside `cleanup-oracle-references`'s
`3.1` scope and removes two migration-era artifacts. **REMOVE all
migration-provenance (oracle / Go / ported narrative), KEEP the design rationale
reframed as native Rust decisions.** `openspec/changes/archive/**` is NOT
touched (historical audit trail). The full REMOVE/KEEP rule + acceptance pattern
+ per-task scope specifics are in `design.md` — read it.

## Rule (summary — full detail in design.md)

- **REMOVE:** `oracle`, `the original`, `Go original/code/binary/service/project`,
  `../synopsis/...`, `Go oracle`, and port language (`ported`, `re-architected`,
  `not transcribed`, `functional copy/rewrite`, "deviations from the oracle",
  "verified against the oracle", "the Go oracle's make build-all", "oracle
  style").
- **KEEP (reframe):** the design rationale / WHY as a native decision; behavioral
  + algorithm descriptions; the project's own `D1…D8` / `ADR 0001…0005` refs;
  wire-format versions (`mcp-go v0.57.0`, minus "the oracle's"). "the oracle v5
  schema" → "the legacy v5 schema" (the shape, not the project).
- **DO NOT remove** the legitimate **DB-migration** concept (`migrations`,
  `PRAGMA user_version`, "the v5 schema shape").
- **Acceptance pattern:** `rg -i '\.\./synopsis|oracle|Go (original|code|binary|service|project)|\bported\b|re-architected|not transcribed|functional (copy|rewrite)' <scope>` → **0**.
  `\bported\b` is word-bounded (do NOT rename `supported_extensions` / `reported`
  / `supported`). `cargo fmt/clippy/test` stay green.

## 1 — Cleanup

- [ ] 1.1 Remove the v5 parity fixture

**Goal.** Delete the gitignored copy of the Go v5 knowledge base and its only
test consumers. It can no longer be regenerated once `../synopsis` is gone, ships
in no checkout, and its tests skip cleanly when absent; FTS5 is verified
independently elsewhere.

**Scope (exact files).**
- `fixtures/` — delete the whole directory (`knowledge.db` gitignored binary +
  `README.md` tracked provenance).
- `crates/db/src/test_util.rs` — remove `fixture_db()`, `fixture_path()`,
  `repo_root()` (only used by `fixture_path`), the `fixture_db_opens_read_only`
  self-test, and the now-unused `use rusqlite::OpenFlags;` import; update the
  module doc (line 3) to drop "and the read-only v5 parity fixture". KEEP
  `in_memory_db()`, `TempDb`, `temp_file_db()`, and the `use std::path::{Path,
  PathBuf};` import (`Path` is still used by the `temp_file_db` test at lines
  ~221/225).
- `crates/db/tests/fts5_parity.rs` — delete the file (3 tests, all skip-guarded).
- `.gitignore` — remove the fixtures block (the two comment lines + `fixtures/*`
  + `!fixtures/README.md` + the trailing blank line) AND reframe the Cargo.lock
  comment: "reproducible builds matter for parity testing against the Go
  oracle" → "reproducible builds matter for parity testing against recorded
  fixtures".
- `crates/config/tests/data/README.md` — the "Note on location" (~lines 104-107)
  claims "the root `.gitignore` ignores `fixtures/*` at any depth"; that rule no
  longer exists after this task. Drop the stale gitignore clause, keep the design
  decision: → "> Note on location: fixtures live under `tests/data/`, not a
  top-level `fixtures/` directory (design D10)."

**Acceptance.** `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace` all green. `rg -n
'fixture_db|fixture_path|repo_root' crates/db/src/test_util.rs` → the only
remaining hits are the `temp_file_db`-related ones (no `fixture_*`, no
`repo_root`). `rg -n 'fixtures/knowledge\.db|fixtures/README\.md' crates/
AGENTS.md README.md openspec/config.yaml .github/ .gitignore` → **0**.
`git ls-files fixtures/` → empty (directory gone; the deletion is staged by the
orchestrator, not this agent). `rg -n 'gitignore.*fixtures/\*|ignores
fixtures' crates/config/tests/data/README.md` → **0** (stale note fixed). No
code logic, public API, or dependency changed.

- [ ] 1.2 Reframe stale oracle/Go comments (comments only)

**Goal.** Remove the migration-provenance framing from the 4 remaining
comment-bearing files, keeping every design decision.

**Scope (exact files + line counts as of writing).**
- `migrations/knowledge/1-init/up.sql` (~8 refs) — "the oracle" / "the Go
  oracle" → "the legacy v5 schema" / "the legacy implementation"; keep every
  deliberate-deviation rationale + `D`/`ADR` ref. SQL statements unchanged.
- `Cargo.toml` (workspace root, ~7 refs) — "the Go oracle's <X>" → neutral
  ("the legacy transport", "the recorded wire format", "the legacy build
  target"); keep every dependency, version, and rationale.
- `.github/workflows/ci.yml` (~3 refs) — "the Go oracle's make build-all" /
  "oracle style" → neutral ("the legacy build matrix", "cancel in-progress");
  keep every job, step, and matrix entry. YAML stays valid.

**Acceptance.** the acceptance pattern over
`migrations/knowledge/1-init/up.sql Cargo.toml .github/workflows/ci.yml` →
**0**. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace` green. Comments only: no SQL statement, no
dependency/version, and no CI job/step/matrix entry changed (verify with
`git diff` that only comment lines differ).

- [ ] 1.3 Delete the port-verification report

**Goal.** Remove the one-time migration milestone report; its deliberate
decisions are already recorded in the archived OpenSpec changes + ADRs.

**Scope (exact file).** `docs/port-verification-report.md` — delete the file.

**Acceptance.** `docs/port-verification-report.md` is gone. `rg -n
'port-verification-report' . -g '!node_modules/**' -g '!openspec/changes/archive/**'`
→ **0** (no dangling links). Gates green (fmt/clippy/test — this is a docs file,
so behavior is unaffected).

- [ ] 1.4 Reframe the two `workspace/` READMEs (oracle/Go → legacy; keep commands as historical)

**Goal.** `workspace/README.md` and `workspace/configs/README.md` document how the
workspace data (models, ONNX runtime, ontology, demo corpus, config presets) was seeded
from the legacy Go project. Remove the oracle/Go provenance wording, keeping every fact
(data present, origin, sizes, sha256, byte-identical `cmp` notes, model names, path
adaptation, design D1 refs). The one-time `cp` seeding commands are kept as historical
notes, but the literal `../synopsis/...` **source** paths are neutralized (the legacy
repo is being deleted), so no live dependency on that path remains. The `data/...`
**destination** paths are kept as-is.

**Scope (exact files + line counts as of writing).**
- `workspace/README.md` (~25 refs) — reframe "the oracle" / "the Go oracle" /
  "Go-created" / "the Go downloader" / "the read-only Go oracle" / "Source in oracle" /
  "identical to the oracle" → neutral ("the legacy project" / "the legacy repo" / "the
  legacy config" / "byte-identical to the legacy file"). The `cp ../synopsis/...` command
  blocks (lines ~72-77 and ~127) are kept as historical notes with the `../synopsis`
  **source** path neutralized (e.g. `cp <legacy repo>/data/... data/...`) and framed as a
  one-time seeding step (the artifacts are already committed; the legacy repo is no
  longer present). Keep every data property (sizes, sha256, model names, path-adaptation
  rationale, design D1 refs). "Go `text/template`" (the legacy prompt engine) may stay as
  a factual language/library reference — only the "oracle" around it is removed.
- `workspace/configs/README.md` (4 refs, lines 9-12) — reframe the config provenance
  table: "Byte-for-byte copy of the Go oracle `configs/onnx.yaml`" / "Port of the Go
  oracle `configs/config.default.yaml`" / "NOT a copy of the Go oracle's …" / "the
  oracle's prompts are Go `text/template`" → neutral ("byte-for-byte from the legacy
  config" / "deliberate deviation from the legacy default" / "re-expressed in minijinja
  (the legacy prompts were Go `text/template`)"). Keep every field, model name, and
  deviation rationale.

**Acceptance.** the acceptance pattern over
`workspace/README.md workspace/configs/README.md` → **0** (no `oracle`, no
`../synopsis`, no `Go original/code/...`). `cargo fmt/clippy/test` green (docs only —
behavior unaffected). No data file, config value, or dependency changed; only README
prose. The `cp` command structure is preserved (historical) with the `../synopsis` source
path neutralized, not deleted.

## 2 — Verification

- [ ] 2.1 Final whole-repo verification

**Goal.** Confirm the repo is fully autonomous: zero migration-provenance
references outside the untouched archive, and all gates green.

**Acceptance.**
- The acceptance pattern over the **whole repo** (excluding
  `.opencode/node_modules/**` (third-party), `openspec/changes/archive/**` and
  `.archive/**` (historical migration artifacts kept as-is), and
  `openspec/changes/finish-migration-cleanup/**` (this change's own planning
  artifacts, which describe the cleanup as meta and move to the archive on
  archiving)) → **0**.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` all green.
- Archive untouched: `git status --porcelain -- openspec/changes/archive/` →
  empty, and `rg -i -c '\.\./synopsis' openspec/changes/archive/ | awk -F:
  '{s+=$2} END {print s+0}'` → **297** (the post-Change-3 baseline; unchanged).
- The legitimate **DB-migration** concept is intact (not over-removed):
  `rg -c 'PRAGMA user_version' crates/ migrations/ AGENTS.md openspec/` and
  `rg -c 'rusqlite_migration' crates/ AGENTS.md openspec/` both remain
  non-zero.
