# Tasks: remove-parity-harness

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml` (binding
context). This change **deletes a transitional dev-tooling crate** and removes
its references from living docs. **`../synopsis` is read-only and must stay
untouched. `openspec/changes/archive/**` must stay untouched.**

Order: **1.1 → 1.2 → 1.3**. Task 1.1 is the compile-affecting change; 1.2 and
1.3 are doc/comment-only and may only run after 1.1 lands (the crate must be
gone first).

---

## 1.1 Delete the crate + workspace member

- **Goal:** remove `crates/parity-harness/` and its workspace membership so the
  workspace no longer builds the crate.
- **Dependencies:** none.
- **Scope файлов:**
  - DELETE the entire directory `crates/parity-harness/` (20 files: `Cargo.toml`,
    `src/{lib,mcp_client,fixtures,diff,content_parity,corpus,metrics,sse_client}.rs`,
    `tests/{parity_test,content_parity,sse_parity}.rs`, `examples/record_content.rs`,
    `fixtures/**` including `vectors.bin`).
  - `Cargo.toml` (repo root): delete the two lines in `[workspace].members`:
    `    # Dev-tooling (design D6): not part of the product dependency graph.`
    and `    "crates/parity-harness",` (keep the surrounding members intact).
- **What happens to `Cargo.lock`:** running any cargo command after the member is
  removed prunes the `[[package]] name = "parity-harness"` entry. That lock-file
  change is **expected and in-scope** (design D3). Do not hand-edit the lock.
- **Критерии приёмки:**
  1. `crates/parity-harness/` no longer exists on disk.
  2. Root `Cargo.toml` `[workspace].members` has no `parity-harness` entry.
  3. `cargo fmt --check` clean.
  4. `cargo clippy --all-targets -- -D warnings` clean.
  5. `cargo test --workspace` green (no crate depends on parity-harness, so the
     suite is unaffected apart from the harness's own tests being gone).
  6. `git diff --staged --name-status` (after staging) shows the crate deletions
     + `Cargo.toml` + `Cargo.lock` only; nothing under `../synopsis`.

---

## 1.2 Remove references from AGENTS.md + README.md (+ migration status)

- **Goal:** remove every `parity-harness` mention from the two top-level docs and
  set the README migration status to **Complete**.
- **Dependencies:** 1.1 (crate already deleted).
- **Scope файлов:**
  - `AGENTS.md`:
    1. Delete the crate-table row:
       `| `crates/parity-harness` | dev-tooling: rmcp client with p50/p95 timing, fixture loader, diff utilities | none — new crate (design D6) |`
    2. In the dependency-graph sentence, delete the clause
       `; `parity-harness` is outside the product dependency graph` so it reads
       `... → search → mcp → cli`. Also: `openspec/` holds ...` (keep the rest).
    3. Delete the commands-table row:
       `| `cargo test -p parity-harness` | parity harness: percentile unit tests + in-process MCP round-trip integration test |`
    4. Delete the whole `vectors.bin fixture format` gotcha bullet (it points at the
       now-deleted harness; `native-seam-spikes` has landed and the format lives in
       `vectors::synx`).
  - `README.md`:
    1. **Done** line: remove the parity-harness mention. Reword to:
       `- **Done:** workspace skeleton — nine domain crates; CI with quality gates (fmt + clippy + test) and a 5-target cross-build matrix.`
    2. **In progress** line → **Complete**:
       `- **Complete:** all modules ported and parity-checked; every change is archived under `openspec/changes/archive/`; contract specs in `openspec/specs/`.`
    3. Delete the commands-table row:
       `| `cargo test -p parity-harness` | parity harness: percentile unit tests + in-process MCP round-trip |`
    4. **Parity** section: reword the paragraph to drop the "`parity-harness` MCP
       client" mechanism and the "harness mechanism exists now" sentence (both
       stale). State that parity was machine-checked during the migration via
       recorded fixtures (JSON diffs of `tools/list` + tool-call responses, p50/p95
       gates) and that the transitional harness has now been removed — the port is
       complete. Keep it to ~2 sentences.
- **Критерии приёмки:**
  1. `rg -n "parity-harness" AGENTS.md README.md` → **zero** matches.
  2. `README.md` "Migration status" states **Complete** (no "In progress").
  3. Both files still read coherently (tables intact, no dangling sentence
       fragments); no other content changed.
  4. No code files touched; `../synopsis` and `openspec/changes/archive/**` untouched.

---

## 1.3 Remove references from mcp crate + COVERAGE.md + port-verification-report.md

- **Goal:** remove the last living-doc/`Cargo.toml`-comment references to the
  crate (all comment-only; no behavior change).
- **Dependencies:** 1.1 (crate already deleted).
- **Scope файлов:**
  - `crates/mcp/Cargo.toml`:
    1. Line ~11 comment: drop the trailing clause `— the same set the parity-harness dev-deps pin`
       (leaves `# HTTP transport (design D8).`).
    2. Lines ~39–41 comment: remove the `the same feature set the parity-harness pins`
       parenthetical so the sentence reads
       `# rmcp's client role + reqwest-backed Streamable HTTP client transport, and`
       `# reqwest for the plain-HTTP` (re-wrap if needed to keep line length sane).
  - `crates/mcp/tests/server_integration.rs`:
    1. Module doc (lines ~3–6): reword `connects with the same rmcp SDK the
       parity-harness uses (client role + reqwest-backed streamable-HTTP transport)`
       → `connects with the rmcp SDK client role (reqwest-backed streamable-HTTP transport)`.
    2. `connect` fn doc (lines ~160–162): reword
       `(the parity-harness pattern, inlined: the test needs plain rmcp, no timing instrumentation)`
       → `(plain rmcp, no timing instrumentation)`.
  - `COVERAGE.md`: delete the row
    `| `parity-harness` | excluded | test infrastructure, not product code |`.
  - `docs/port-verification-report.md`: delete the row
    `| Parity harness (`crates/parity-harness`) | Included in `cargo test`; fixture-based tool-response parity + recall/percentile unit tests green |`.
- **Критерии приёмки:**
  1. `rg -n "parity-harness" crates/mcp COVERAGE.md docs/port-verification-report.md`
     → **zero** matches (outside `openspec/changes/archive/**`).
  2. `cargo test -p mcp` green (comment-only edits; confirms the test still
     compiles and passes).
  3. `cargo fmt --check` clean (re-wrapped comment lines are still valid).
  4. No `Cargo.toml` **dependency** changed — only comment text; `git diff` shows
     comment-only changes in `crates/mcp/Cargo.toml`.

---

## Final verification (after 1.1–1.3, before archive)

- `rg -n "parity-harness" --glob '!openspec/changes/archive/**' --glob '!target/**' .`
  → **zero** matches in living files.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace` all green.
- `crates/parity-harness/` absent; root `Cargo.toml` member absent; `Cargo.lock`
  has no `[[package]] parity-harness`.
