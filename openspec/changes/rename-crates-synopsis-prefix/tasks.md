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
   `synopsis_X = { path = "crates/synopsis-X" }` (skip the palette entry if the
   crate has none — only `cli` has none).
4. In every dependent crate listed in the task: `[dependencies]`/`[dev-dependencies]`
   entry `X = { workspace = true }` → `synopsis_X = { workspace = true }`
   (both sections, if present in both).
5. In all `.rs` files (`src/`, `tests/`, anywhere): replace every identifier
   reference `\bX::` with `synopsis_X::` — the pattern REQUIRES the `::` suffix
   (word boundary + `::`), so prose like "the db crate", error strings, and config
   keys are never touched. Example:
   `rg --pcre2 -l '\bX::' -g '*.rs' | xargs sed -i 's/\bX::/synopsis_X::/g'`.
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

- [ ] 2.1 Rename `utils` → `synopsis-utils`
  - Dependencies: task 1.1.
  - File scope: root `Cargo.toml`; `crates/utils/**` → `crates/synopsis-utils/**`;
    dependents with a Cargo.toml entry: `crates/embedding`, `crates/ingestion`,
    `crates/graph`, `crates/search`, `crates/cli`; all `.rs` files containing
    `utils::` (~36 lines across embedding, ingestion, graph, search, cli).
  - Acceptance: recipe gates + `rg '\butils::' -g '*.rs'` empty + metadata check.

- [ ] 2.2 Rename `vectors` → `synopsis-vectors`
  - Dependencies: task 2.1.
  - File scope: root `Cargo.toml`; `crates/vectors/**` → `crates/synopsis-vectors/**`;
    dependents with a Cargo.toml entry: `crates/ingestion`, `crates/search`,
    `crates/cli` (`vectors` appears in BOTH cli `[dependencies]` and
    `[dev-dependencies]` — update both); all `.rs` files containing `vectors::`
    (~43 lines).
  - Acceptance: recipe gates + `rg '\bvectors::' -g '*.rs'` empty + metadata check.

- [ ] 2.3 Rename `llm` → `synopsis-llm`
  - Dependencies: task 2.2.
  - File scope: root `Cargo.toml`; `crates/llm/**` → `crates/synopsis-llm/**`;
    dependents with a Cargo.toml entry: `crates/ingestion`, `crates/graph`;
    all `.rs` files containing `llm::` (~19 lines: `crates/llm/tests/client.rs`
    self-references, `crates/ingestion/src/ner/llm.rs`, `crates/ingestion/src/ner/mod.rs`,
    `crates/ingestion/src/error.rs`, `crates/graph/src/linker.rs`).
  - Acceptance: recipe gates + `rg '\bllm::' -g '*.rs'` empty + metadata check.

- [ ] 2.4 Rename `config` → `synopsis-config`
  - Dependencies: task 2.3.
  - File scope: root `Cargo.toml`; `crates/config/**` → `crates/synopsis-config/**`;
    dependents with a Cargo.toml entry (8): `crates/db`, `crates/llm`,
    `crates/embedding`, `crates/ingestion`, `crates/graph`, `crates/search`,
    `crates/mcp`, `crates/cli`; all `.rs` files containing `config::`
    (~214 lines).
  - Acceptance: recipe gates + `rg '\bconfig::' -g '*.rs'` empty + metadata check.

- [ ] 2.5 Rename `db` → `synopsis-db` (user-approved diff exception, design D4)
  - Dependencies: task 2.4.
  - File scope: root `Cargo.toml`; `crates/db/**` → `crates/synopsis-db/**`;
    dependents with a Cargo.toml entry (5): `crates/ingestion`, `crates/graph`,
    `crates/search`, `crates/mcp`, `crates/cli`; all `.rs` files containing `db::`
    (~427 lines).
  - Note: this single atomic task produces ~850–900 diff lines — explicitly
    approved by the user (2026-10-27) as a mechanical-rename exception to the
    ~500-line rule. The diff MUST be 1:1 line replacements only (no logic).
  - Acceptance: recipe gates + `rg '\bdb::' -g '*.rs'` empty + metadata check.

- [ ] 2.6 Rename `embedding` → `synopsis-embedding`
  - Dependencies: task 2.5.
  - File scope: root `Cargo.toml`; `crates/embedding/**` →
    `crates/synopsis-embedding/**`; dependents with a Cargo.toml entry (3):
    `crates/ingestion`, `crates/search`, `crates/cli`; all `.rs` files containing
    `embedding::` (~44 lines).
  - Acceptance: recipe gates + `rg '\bembedding::' -g '*.rs'` empty + metadata check.

- [ ] 2.7 Rename `ingestion` → `synopsis-ingestion`
  - Dependencies: task 2.6.
  - File scope: root `Cargo.toml`; `crates/ingestion/**` →
    `crates/synopsis-ingestion/**`; dependent with a Cargo.toml entry (1):
    `crates/cli`; all `.rs` files containing `ingestion::` (~52 lines, in cli).
  - Acceptance: recipe gates + `rg '\bingestion::' -g '*.rs'` empty + metadata check.

- [ ] 2.8 Rename `graph` → `synopsis-graph`
  - Dependencies: task 2.7.
  - File scope: root `Cargo.toml`; `crates/graph/**` → `crates/synopsis-graph/**`;
    dependents with a Cargo.toml entry (4): `crates/ingestion`, `crates/search`,
    `crates/mcp`, `crates/cli`; all `.rs` files containing `graph::` (~46 lines).
  - Acceptance: recipe gates + `rg '\bgraph::' -g '*.rs'` empty + metadata check.

- [ ] 2.9 Rename `search` → `synopsis-search`
  - Dependencies: task 2.8.
  - File scope: root `Cargo.toml`; `crates/search/**` → `crates/synopsis-search/**`;
    dependents with a Cargo.toml entry (2): `crates/mcp`, `crates/cli`; all `.rs`
    files containing `search::` (~19 lines).
  - Acceptance: recipe gates + `rg '\bsearch::' -g '*.rs'` empty + metadata check.

- [ ] 2.10 Rename `mcp` → `synopsis-mcp`
  - Dependencies: task 2.9.
  - File scope: root `Cargo.toml`; `crates/mcp/**` → `crates/synopsis-mcp/**`;
    dependent with a Cargo.toml entry (1): `crates/cli`; all `.rs` files
    containing `mcp::` (~31 lines, in cli + mcp's own tests).
  - Acceptance: recipe gates + `rg '\bmcp::' -g '*.rs'` empty + metadata check.

- [ ] 2.11 Rename `cli` → `synopsis-cli`
  - Dependencies: task 2.10.
  - File scope: root `Cargo.toml` (`members` only — `cli` has NO
    `[workspace.dependencies]` palette entry); `crates/cli/**` →
    `crates/synopsis-cli/**` (`[package] name = "cli"` → `"synopsis-cli"`;
    `[[bin]] name = "synopsis"` MUST stay unchanged). No other crate depends on
    `cli`, and no `.rs` file references `cli::` — no identifier replacement.
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
    2. `rg '\b(db|config|vectors|utils|llm|embedding|ingestion|graph|search|mcp|cli)::' -g '*.rs'`
       → 0 matches.
    3. `cargo build --release`; re-capture the 19 `--help` sections exactly as in
       task 1.1 to `/tmp/opencode/synopsis-help-after.txt`;
       `diff /tmp/opencode/synopsis-help-before.txt /tmp/opencode/synopsis-help-after.txt`
       → empty (byte-identical CLI surface).
    4. `rg 'crates/(cli|config|db|embedding|graph|ingestion|llm|mcp|search|utils|vectors)\b'`
       → no matches outside `openspec/changes/archive/**` and `target/`.
    5. `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'`
       → exactly the 11 `synopsis-*` names, no bare names.
  - Acceptance: all five checks pass; report the results compactly.
