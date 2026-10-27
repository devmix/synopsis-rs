# Tasks — rename-crates-synopsis-prefix

Context for every task in this file: see `design.md` (decisions D1–D7) and
`openspec/config.yaml` (gates, execution model). No frozen contract is touched
(proposal.md — Frozen contracts). Every rename task below ends with a green build.

## 1. Baseline

- [x] 1.1 Capture the pre-rename CLI baseline
  - Goal: record the exact `--help` output of the `synopsis` binary and all
    subcommands before any rename, for the final byte-diff (design D6).
  - File scope: none in the repo; output goes to `/tmp/opencode/synopsis-help-before.txt`.
  - Steps: `cargo build` (debug); then write to the file, one section per command,
    the output of: `synopsis --help`, `synopsis serve --help`,
    `synopsis queue --help`, `synopsis queue status --help`,
    `synopsis queue reset-retries --help`, `synopsis db --help`,
    `synopsis db stats --help`, `synopsis db clear --help`, `synopsis model --help`,
    `synopsis model list --help`, `synopsis model download --help`,
    `synopsis model delete --help`, `synopsis model info --help`,
    `synopsis model benchmark --help`, `synopsis onnx-runtime --help`,
    `synopsis onnx-runtime install --help`, `synopsis onnx-runtime status --help`,
    `synopsis onnx-runtime uninstall --help`, `synopsis load-test --help`
    (binary at `target/debug/synopsis`; separate sections with the command as header).
  - Acceptance: file exists and contains all 19 sections; no repo files modified.

## 2. Crate renames (one subtask per crate, in this exact order)

Mechanical recipe (applies to every task 2.x; `X` = the bare crate name,
`synopsis_X` = its new Rust identifier; design D1/D2/D5):

1. `git mv crates/X crates/synopsis-X`.
2. `crates/synopsis-X/Cargo.toml`: in `[package]` set `name = "synopsis-X"`.
   Never touch `[[bin]] name` (only exists in the cli crate, stays `synopsis`).
3. Root `Cargo.toml`: `members` entry `"crates/X"` → `"crates/synopsis-X"`;
   `[workspace.dependencies]` palette entry `X = { path = "crates/X" }` →
   `synopsis_X = { package = "synopsis-X", path = "crates/synopsis-X" }`.
   The explicit `package =` key is REQUIRED: cargo 1.96.0 does not normalize
   underscore/hyphen for path-dependency keys and fails with "no matching
   package named `synopsis_X`" without it (discovered in task 2.1). Skip the
   palette entry if the crate has none — only `cli` has none.
4. In every dependent crate listed in the task: `[dependencies]`/`[dev-dependencies]`
   entry `X = { workspace = true }` → `synopsis_X = { workspace = true }`
   (both sections, if present in both).
5. In all `.rs` files (`src/`, `tests/`, anywhere): replace every identifier
   reference to the crate X with `synopsis_X::`. A blanket sed
   (`rg --pcre2 -l '\bX::' -g '*.rs' | xargs sed -i 's/\bX::/synopsis_X::/g'`)
   is only safe when NO crate has an internal module named X. Several do —
   per-task exclusion lists are in the task bodies. For every match of
   `\bX::` resolve whether it refers to the crate or to an internal module
   (check where the referenced item is defined; `crate::X::` and
   `parent::X::` paths are always internal) and rename ONLY the crate
   references. Prose like "the db crate", error strings, and config keys are
   never touched (the pattern still requires the `::` suffix).
6. Gates (all must pass before staging): `cargo fmt`, then
   `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
7. Staging: stage exactly the expected files (root `Cargo.toml`,
   `crates/synopsis-X/**`, the dependent crates' `Cargo.toml` + touched `.rs`
   files, `Cargo.lock`). Verify with `git diff --staged --name-status`.

Per-task acceptance (in addition to the recipe gates):
- `rg '\bX::' -g '*.rs'` returns no matches repo-wide.
- `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'`
  contains `synopsis-X` and does NOT contain `X`.
- Reference counts below are sanity checks (measured pre-rename): the replacement
  must touch exactly those lines (± comment/doc lines containing `X::`).

- [x] 2.1 Rename `utils` → `synopsis-utils`
  - Dependencies: task 1.1.
  - File scope: root `Cargo.toml`; `crates/utils/**` → `crates/synopsis-utils/**`;
    dependents with a Cargo.toml entry: `crates/embedding`, `crates/ingestion`,
    `crates/graph`, `crates/search`, `crates/cli`; all `.rs` files containing
    `utils::` (~36 lines across search, db, ingestion, graph, embedding, and the
    utils crate's own tests; cli has only a Cargo.toml entry).
  - Exclusions (internal `utils` module of the db crate — NOT the utils crate,
    must NOT be renamed): `crate::utils::` in `crates/db/src/**` (7 lines:
    entity.rs ×3, document.rs, queue_task.rs, fact.rs ×2) and `db::utils::`
    (6 lines: `crates/search/src/lib.rs` ×4, `crates/graph/src/linker.rs` ×1,
    a `db::{…, utils::normalize}` grouped import in `crates/graph/src/graph.rs`
    ×1).
  - Acceptance: recipe gates + NO crate-reference `utils::` remains (the 13
    internal-module lines above are expected to remain) + metadata check.
  - Revision history: implemented 2026-10-27. Deviations recorded: (1) palette
    entry required the `package = "synopsis-utils"` key (cargo path-dep
    underscore/hyphen normalization) — recipe step 3 updated for all tasks;
    (2) the 13 db-internal `utils::` lines were correctly left untouched (a
    blanket sed would have broken the build); (3) the 23 genuine crate
    references were replaced; gates green.

- [x] 2.2 Rename `vectors` → `synopsis-vectors`
  - Dependencies: task 2.1.
  - File scope: root `Cargo.toml`; `crates/vectors/**` → `crates/synopsis-vectors/**`;
    dependents with a Cargo.toml entry: `crates/ingestion`, `crates/search`,
    `crates/cli` (`vectors` appears in BOTH cli `[dependencies]` and
    `[dev-dependencies]` — update both); all `.rs` files containing `vectors::`
    (~43 lines).
  - Acceptance: recipe gates + `rg '\bvectors::' -g '*.rs'` empty + metadata check.
  - Revision history: implemented 2026-10-27, zero deviations (43 references,
    27 files; 5 config doc-comments renamed; 3 rustfmt rewraps; gates green).

- [x] 2.3 Rename `llm` → `synopsis-llm`
  - Dependencies: task 2.2.
  - File scope: root `Cargo.toml`; `crates/llm/**` → `crates/synopsis-llm/**`;
    dependents with a Cargo.toml entry: `crates/ingestion`, `crates/graph`;
    all `.rs` files containing `llm::` (19 pre-rename matches).
  - Exclusions (the ingestion crate has an INTERNAL module `ner::llm` at
    `crates/ingestion/src/ner/llm.rs` — NOT the llm crate):
    `crates/ingestion/src/ner/mod.rs:46` `pub use llm::LlmNer;` resolves to the
    internal module (LlmNer is defined in `ner/llm.rs`) and must NOT be
    renamed. All other matches ARE crate references and must be renamed:
    `crates/llm/tests/client.rs` (11 self-references),
    `crates/ingestion/src/ner/llm.rs` (5: `use llm::LlmClient;`,
    `use llm::LlmError;`, 3 intra-doc links `llm::LlmClient…`),
    `crates/ingestion/src/error.rs` (1: `llm::LlmError`),
    `crates/graph/src/linker.rs` (1: `use llm::LlmClient;`).
    Resolve every match by the item's definition (recipe step 5).
  - Acceptance: recipe gates + NO crate-reference `llm::` remains (the 1
    internal-module line above is expected to remain) + metadata check.
  - Revision history: implemented 2026-10-27, zero deviations (18 of 19
    references renamed; the ner/mod.rs:46 internal-module re-export preserved;
    6 rustfmt re-wraps in synopsis-llm/tests/client.rs; gates green).

- [x] 2.4 Rename `config` → `synopsis-config`
  - Dependencies: task 2.3.
  - File scope: root `Cargo.toml`; `crates/config/**` → `crates/synopsis-config/**`;
    dependents with a Cargo.toml entry (8): `crates/db`, `crates/llm`,
    `crates/embedding`, `crates/ingestion`, `crates/graph`, `crates/search`,
    `crates/mcp`, `crates/cli`; all `.rs` files containing `config::`
    (~214 lines).
  - Acceptance: recipe gates + `rg '\bconfig::' -g '*.rs'` empty + metadata check.
  - Revision history: implemented 2026-10-27, zero deviations (212 references,
    78 files; 2 synopsis-vectors doc-comment-only renames in scope; 3 stale
    crates/config prose paths deferred to 3.1 and listed there; gates green).

- [x] 2.5 Rename `db` → `synopsis-db` (user-approved diff exception, design D4)
  - Dependencies: task 2.4.
  - File scope: root `Cargo.toml`; `crates/db/**` → `crates/synopsis-db/**`;
    dependents with a Cargo.toml entry (5): `crates/ingestion`, `crates/graph`,
    `crates/search`, `crates/mcp`, `crates/cli`; all `.rs` files containing `db::`
    (419 pre-rename matches: 416 crate references + 3 internal-module lines).
  - Exclusions (INTERNAL modules named `db` — NOT the db crate):
    (a) the cli crate's `db` subcommand module at `crates/cli/src/db.rs`:
    `crates/cli/src/serve/server.rs:105` `use crate::db::clear_dataset_tables;`
    and `crates/cli/src/main.rs:15` `use cli::db::{DbRequest, run_db};` must NOT
    be renamed (the `main.rs` line is renamed in task 2.11 as a `cli::`
    self-reference, keeping the internal module name `db`);
    (b) the config crate's internal `db` module (re-exported at its root):
    `crates/synopsis-config/src/lib.rs:35` `pub use db::{ID_BATCH_SIZE,
    LINK_BATCH_SIZE};` must NOT be renamed (discovered in implementation —
    a third internal line beyond the cli pair).
    A blanket sed of `\bdb::` would corrupt all three — resolve every match
    by the referenced item's definition (recipe step 5).
  - Note: this single atomic task produces ~850–900 diff lines — explicitly
    approved by the user (2026-10-27) as a mechanical-rename exception to the
    ~500-line rule. The diff MUST be 1:1 line replacements only (no logic).
  - Acceptance: recipe gates + NO crate-reference `db::` remains (the 3
    internal-module lines above are expected to remain) + metadata check.
  - Revision history: implemented 2026-10-27. Deviations recorded: (1) the
    third exclusion line (b) above was discovered during implementation and
    correctly left untouched (net diff of that file zero); (2) rustfmt
    re-sorted import groups and re-wrapped >100-char lines (same class as
    tasks 2.2/2.3) — reverse-transform diff against HEAD confirmed the whole
    diff is mechanical. 416 references renamed across 86 files; gates green.
    Environment note: the disk was 100% full before the test gate; the
    regenerable target/debug/incremental cache (~21 GB) was deleted to unblock
    linking. One flaky cli serve_server test failed under that disk pressure
    and passed on re-run.

- [x] 2.6 Rename `embedding` → `synopsis-embedding`
  - Dependencies: task 2.5.
  - File scope: root `Cargo.toml`; `crates/embedding/**` →
    `crates/synopsis-embedding/**`; dependents with a Cargo.toml entry (3):
    `crates/ingestion`, `crates/search`, `crates/cli`; all `.rs` files containing
    `embedding::` (~44 lines).
  - Acceptance: recipe gates + `rg '\bembedding::' -g '*.rs'` empty + metadata check.
  - Revision history: implemented 2026-10-27. 43 references renamed across
    26 files; rustfmt re-sorted import groups in ~22 files and re-wrapped one
    line (cli loadtest/mod.rs:166, >100 chars after the longer identifier) —
    the same mechanical class as tasks 2.2/2.3/2.5 (the implementer's
    zero-deviations claim was corrected by the reviewer's reverse-transform
    check). Gates green (one flaky first full test run under disk pressure;
    two clean 1606/1606 runs).

- [x] 2.7 Rename `ingestion` → `synopsis-ingestion`
  - Dependencies: task 2.6.
  - File scope: root `Cargo.toml`; `crates/ingestion/**` →
    `crates/synopsis-ingestion/**`; dependent with a Cargo.toml entry (1):
    `crates/cli`; all `.rs` files containing `ingestion::` — 52 matches
    across 50 lines: 21 in cli, 29 self-references (27 lines) in the
    ingestion crate's own src/tests, and 2 doc-comment references in
    `crates/synopsis-db/src/app_kv.rs` and `crates/graph/src/linker.rs`
    (`ingestion::ner::llm_cache::LlmNerCache`).
    (search's only `ingestion` mention is a prose comment in its Cargo.toml —
    never touched per recipe step 5.)
  - Acceptance: recipe gates + `rg '\bingestion::' -g '*.rs'` empty + metadata check.
  - Revision history: implemented 2026-10-27. The file-scope line was
    corrected from the planning estimate "~52 lines, in cli" to the actual
    distribution (52 matches / 50 lines: 21 + 29 + 2) after implementation;
    the orchestrator's
    subagent reminder wrongly listed search as a dependent — the task body
    (1 dependent: cli) was followed. rustfmt re-sorted import groups in a few
    test files (same mechanical class as tasks 2.2/2.3/2.5/2.6). Gates green
    (1606/1606).

- [x] 2.8 Rename `graph` → `synopsis-graph`
  - Dependencies: task 2.7.
  - File scope: root `Cargo.toml`; `crates/graph/**` →
    `crates/synopsis-graph/**`; dependents with a Cargo.toml entry (4):
    `crates/synopsis-ingestion`, `crates/search`, `crates/mcp`, `crates/cli`;
    all `.rs` files containing `graph::` — 38 crate references across 28
    files (the planning estimate was 46 matches / 43 crate references; the
    measured distribution is below).
  - Exclusions (NOT the graph crate — resolve every match by the referenced
    item's definition, recipe step 5; a blanket sed of `\bgraph::` would
    corrupt all eight):
    (a) the graph crate's INTERNAL module `graph` at
    `crates/graph/src/graph.rs`: `crate::graph::` in `cel.rs:183`,
    `metrics.rs:64`, `traverser.rs:63` (3 lines, listed at planning);
    (b) the EXTERNAL `petgraph` crate: `petgraph::graph::` in
    `traverser.rs:59`, `metrics.rs:62`, `lib.rs:51`, `graph.rs:74` (4 lines,
    discovered in implementation — grep false positives);
    (c) the INTERNAL re-export `pub use graph::{...}` in `lib.rs:33` (resolves
    to the `pub mod graph;` declared at `lib.rs:21`; discovered in
    implementation — a rename would break compilation).
  - Acceptance: recipe gates + NO crate-reference `graph::` remains (the 8
    exclusion lines above are expected to remain) + metadata check.
  - Revision history: implemented 2026-10-27. 38 crate references renamed
    across 28 files; the 5 additional survivor lines (b)+(c) discovered
    during implementation were correctly left untouched (gates green confirm).
    rustfmt re-sorted import groups in 22 files (same mechanical class as
    tasks 2.2/2.3/2.5/2.6); reverse-transform diff confirmed the whole
    diff is mechanical. Gates green (1606/1606; one flaky cli serve_server
    failure on the first parallel run, clean on re-runs).

- [x] 2.9 Rename `search` → `synopsis-search`
  - Dependencies: task 2.8.
  - File scope: root `Cargo.toml`; `crates/search/**` →
    `crates/synopsis-search/**`; dependents with a Cargo.toml entry (2):
    `crates/mcp`, `crates/cli`; all `.rs` files containing `search::` — 18
    matches: 16 crate references across 12 files + 2 internal-module lines
    below. (Planning estimate was ~19 lines.)
  - Exclusions (NOT the search crate — the planning premise "no internal
    module named `search`" did not hold; discovered in implementation,
    resolved by the referenced item's definition per recipe step 5):
    (a) mcp's INTERNAL module `tools::search` at
    `crates/mcp/src/tools/search.rs`: the dispatch arm
    `"search" => tools::search::handle_search(...)` at
    `crates/mcp/src/server.rs:211`;
    (b) synopsis-vectors' INTERNAL module `usearch::search` at
    `crates/synopsis-vectors/src/usearch/search.rs`: `use
    search::{add_rows, build_search_pool};` at
    `crates/synopsis-vectors/src/usearch/mod.rs:150` (`mod search;` declared
    in the same file).
    Renaming either breaks compilation.
  - Acceptance: recipe gates + NO crate-reference `search::` remains (the 2
    internal-module lines above are expected to remain) + metadata check.
  - Revision history: implemented 2026-10-27. 16 crate references renamed
    across 12 files; the 2 internal-module exclusions (a)+(b) discovered
    during implementation were correctly left untouched (gates green
    confirm). rustfmt re-sorted import groups in 8 files and re-wrapped one
    >100-char signature line in mcp tools/search.rs (same mechanical class
    as tasks 2.2/2.3/2.5/2.6/2.8). Gates green (1606/1606).

- [ ] 2.10 Rename `mcp` → `synopsis-mcp`
  - Dependencies: task 2.9.
  - File scope: root `Cargo.toml`; `crates/mcp/**` → `crates/synopsis-mcp/**`;
    dependent with a Cargo.toml entry (1): `crates/cli`; all `.rs` files
    containing `mcp::` (31 matches / 29 lines across 10 files: 8 in cli,
    1 doc comment in synopsis-mcp's own lib.rs, 22 in its tests).
  - Acceptance: recipe gates + `rg '\bmcp::' -g '*.rs'` empty + metadata check.
  - Revision history: implemented 2026-10-27. Pre-rename verification
    confirmed no internal `mcp` module exists (no `mod mcp`, no
    `crate::mcp::` / `::mcp::` anywhere); the `\b` boundary protected the
    23 `rmcp::` references (external rmcp crate, confirmed untouched in
    HEAD and after). rustfmt re-sorted import groups in 4 files (3
    synopsis-mcp test files + cli loadtest/mod.rs) — same mechanical class
    as tasks 2.2/2.3/2.5/2.6/2.8/2.9; reverse-transform diff confirmed the
    whole diff is mechanical. Gates green (1606/1606).

- [ ] 2.11 Rename `cli` → `synopsis-cli`
  - Dependencies: task 2.10.
  - File scope: root `Cargo.toml` (`members` only — `cli` has NO
    `[workspace.dependencies]` palette entry); `crates/cli/**` →
    `crates/synopsis-cli/**` (`[package] name = "cli"` → `"synopsis-cli"`;
    `[[bin]] name = "synopsis"` MUST stay unchanged).
  - Identifier replacements: no other crate depends on `cli`, but the cli
    crate SELF-REFERENCES its own crate name in 24 places — integration tests
    (`crates/cli/tests/**`: `use cli::error::CliError;`, `use cli::serve::…`,
    doc comments) and `crates/cli/src/main.rs` (`use cli::db::{…}` etc.).
    Replace every `cli::` self-reference with `synopsis_cli::` (24 lines).
    Internal module names inside the paths (e.g. `cli::db::` →
    `synopsis_cli::db::`) stay as-is — only the leading crate segment changes.
  - Acceptance: recipe gates + `cargo build --release` produces
    `target/release/synopsis` + `rg '\bcli::' -g '*.rs'` empty + metadata check
    (`synopsis-cli` present, `cli` absent).

## 3. Documentation and agent definitions

- [ ] 3.1 Update docs, comments, and agent definitions to `synopsis-*` names
  - Dependencies: tasks 2.1–2.11.
  - Goal: remove all remaining bare-name references in prose/comments (design D7).
    `openspec/changes/archive/**` is a historical record — do NOT modify it.
  - File scope (update every bare reference, both `crates/<name>` paths and
    `name::`/`name crate` prose):
    - `AGENTS.md` (Layout table, dependency-graph line, Commands table, Gotchas)
    - `README.md`
    - `site/docs/**` (~30 lines: workspace-layout, architecture, extending,
      migrations, configuration, config-schema, database-schema,
      cross-domain-linking)
    - `docs/adr/**` (2 files)
    - `openspec/specs/utils/spec.md`
    - `workspace/configs/README.md`
    - `.opencode/agents/site-implementer.md`, `.opencode/agents/rust-implementer.md`,
      `.opencode/agents/docs-writer.md`, `.opencode/agents/ontology-author.md`
    - Prose comments in `Cargo.toml` files (root palette comment block lines
      ~53–71, and per-crate comments such as the `# D1 edges` block in
      `crates/synopsis-cli/Cargo.toml`)
    - Stale `crates/<name>` path mentions in code comments and test data known
      from the rename subtasks: `crates/synopsis-config/tests/data/README.md`
      (sha256sum examples), `crates/synopsis-config/tests/preset.rs` (comment),
      `crates/ingestion/src/ner/prompts.rs` (comment), and any other hit of the
      acceptance rg below (the acceptance sweep is repo-wide — fix every hit)
  - Do NOT touch: `Cargo.lock` (no paths inside), `.github/workflows/**`,
    `scripts/release.sh` (no crate paths — verified), `openspec/changes/archive/**`.
  - Acceptance:
    - `rg 'crates/(cli|config|db|embedding|graph|ingestion|llm|mcp|search|utils|vectors)\b'`
      returns no matches outside `openspec/changes/archive/**` and `target/`.
    - `rg '\b(config|db|vectors|utils|llm|embedding|ingestion|graph|search|mcp|cli)::' -g '*.md'`
      returns no matches outside `openspec/changes/archive/**`.
    - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test` still green (comments only, but gates are mandatory).

## 4. Final verification

- [ ] 4.1 Parity and full-gate verification
  - Dependencies: tasks 2.11, 3.1.
  - File scope: none (verification only; no source edits expected — if any gate
    fails, fix within the failing area and re-run).
  - Steps:
    1. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
       `cargo test` (the full suite includes the MCP differential tests against
       recorded fixtures — the fixtures themselves must be unchanged).
    2. For every bare name X in (db, config, vectors, utils, llm, embedding,
       ingestion, graph, search, mcp, cli): every remaining `X::` match in
       `.rs` files must be an INTERNAL module path, never a crate reference.
       Known expected survivors (documented in tasks 2.1/2.3/2.5/2.8/2.11):
       `crate::utils::` ×7 + `db::utils::` ×6 (db's internal utils module),
       `llm::LlmNer` re-export in `crates/synopsis-ingestion/src/ner/mod.rs`
       ×1 (the internal ner::llm module), `crate::db::` ×1 +
       `synopsis_cli::db::` ×1 (cli's internal db subcommand module),
       `db::{ID_BATCH_SIZE, LINK_BATCH_SIZE}` re-export in
       `crates/synopsis-config/src/lib.rs` ×1 (config's internal db module),
       `crate::graph::` ×3 (graph's internal graph module),
       `petgraph::graph::` ×4 (external petgraph crate, grep false positives),
       `pub use graph::{...}` re-export in `crates/synopsis-graph/src/lib.rs`
       ×1 (graph's internal graph module), `tools::search::handle_search`
       in `crates/mcp/src/server.rs` ×1 (mcp's internal tools::search
       module), `use search::{add_rows, build_search_pool};` in
       `crates/synopsis-vectors/src/usearch/mod.rs` ×1 (vectors' internal
       usearch::search module). Verify the match set equals exactly this
       survivor list; any other `X::` match is a failure.
    3. `cargo build --release`; re-capture the 19 `--help` sections exactly as in
       task 1.1 to `/tmp/opencode/synopsis-help-after.txt`;
       `diff /tmp/opencode/synopsis-help-before.txt /tmp/opencode/synopsis-help-after.txt`
       → empty (byte-identical CLI surface).
    4. `rg 'crates/(cli|config|db|embedding|graph|ingestion|llm|mcp|search|utils|vectors)\b'`
       → no matches outside `openspec/changes/archive/**` and `target/`.
    5. `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'`
       → exactly the 11 `synopsis-*` names, no bare names.
  - Acceptance: all five checks pass; report the results compactly.
