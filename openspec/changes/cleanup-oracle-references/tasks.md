# Tasks: cleanup-oracle-references

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. The Go
project at `../synopsis` will be deleted, so the Rust docs must no longer
reference it or frame the code as a port. **Remove all migration-provenance
(oracle / Go / ported narrative + any remaining `../synopsis` paths) from all
living docs, keeping the design rationale reframed as native Rust decisions.**

**`openspec/changes/archive/**` is NOT touched** (historical audit trail).

## Rule (applies to every task — full detail + examples in design.md)

- **REMOVE:** references to the Go project/oracle (`the oracle`, `Go original`,
  `Go code`, `the original`, `../synopsis/...`), Go source file names
  (`*.go`, `*.tmpl`), and port/migration language (`ported`, `faithful port`,
  `re-architected`, `not transcribed`, `functional copy`, `migration`,
  `deviations from the oracle`, `verified against the oracle`, `parity with the
  oracle`, `parity-checked`). Also the AGENTS.md `## Oracle` and
  `## Migration principles` sections.
- **KEEP (reframe):** the design rationale / WHY (as a native decision, e.g.
  "Design: silent defaults are replaced by fail-fast validation"), behavioral
  and algorithm descriptions, the project's own `D1…D8` / `ADR 0001…0005`
  references, and wire-format versions (`mcp-go v0.57.0`, minus "the oracle's").
- Crate edits are comment-only → `cargo fmt/clippy/test` stay green.
- Note: tasks 1.1–1.6 already had the `../synopsis` **paths** removed in a first
  pass (committed for 1.1–1.5, uncommitted for 1.6). This pass removes the
  **remaining narrative** in those files; 1.7–1.8 do the full cleanup.

## 1 — Crates

- [ ] 1.1 Clean `db` crate narrative (14 files)

**Scope.** `crates/db/src/{app_kv,chunk_entity,chunk,connection,document,
entity_link,entity,entity_source,executor,fact,fact_source,gc,lib,utils}.rs`.
Paths are already removed; strip the remaining migration narrative (e.g.
"re-architected for Rust per the migration principles of 2026-08-19", "Go
oracle", "Go bug fixes", "deliberate deviation from the oracle") → reframe as
native design decisions, keeping the rationale (D1/D2/D3/ADR 0001/D8 rationale
in lib.rs, sealed-enum in executor.rs, DRY-composition in gc.rs).

**Acceptance.** `rg -i '\.\./synopsis|oracle|Go (oracle|original|code|binary)|
ported|re-architected|not transcribed|migration|deviation from the oracle'
crates/db/src/` → **0**. `cargo fmt/clippy/test` green. No code changed.

- [ ] 1.2 Clean `config` + `llm` narrative (3 files)

**Scope.** `crates/config/src/lib.rs`, `crates/llm/src/lib.rs`,
`crates/llm/src/client.rs`. Strip "ports the Go oracle's `internal/config`
package", "Deliberate deviation(s) from the oracle" → "Design:" (keep the
deviation rationale: fail-fast validation, plain-string content, injectable
sleeper).

**Acceptance.** `rg -i` (same pattern as 1.1) over `crates/config/src/
crates/llm/src/` → **0**. Gates green. Design rationale preserved.

- [ ] 1.3 Clean `embedding` narrative (7 files)

**Scope.** `crates/embedding/src/{cache,downloader,library,lib,model,provider,
tokenizer}.rs`. Strip "re-architected, not transcribed", "the oracle's
`CacheKey`", "mirroring the oracle's `DefaultMaxLength`", Go file names
(`library.go`, `model-manager.go`, …), "Deliberate deviations from the oracle"
→ "Design decisions:". Keep the rationale (retry/SSRF/timeout behavior, CLS
pooling, tokenizer pad/attention_mask handling).

**Acceptance.** `rg -i` (pattern 1.1) over `crates/embedding/src/` → **0**.
Gates green. Cargo.toml already clean (`# D1 edges.`).

- [ ] 1.4 Clean `graph` narrative (7 files)

**Scope.** `crates/graph/src/{cel,graph,lib,linker,metrics,prompts,traverser}.rs`.
Strip "The Go code is a reference for behavior and contracts only, not a code
blueprint", "Verified against the oracle", "functional copy, re-architected for
Rust", "ported verbatim" → keep the design (hybrid storage, CEL compile-once,
BFS contract, petgraph `DiGraph`).

**Acceptance.** `rg -i` (pattern 1.1) over `crates/graph/src/` → **0**. Gates
green.

- [ ] 1.5 Clean `search` narrative (8 files)

**Scope.** `crates/search/src/{enrich,expand,hybrid,lexical,lib,rerank,rrf,
semantic}.rs`. Strip "Faithful port of the oracle's `rrf.go`", "The Go code is a
reference…", "ported", Go file names → describe the algorithms natively (RRF
`score += 1/(k+rank)`, hybrid fusion, rerank, expansion). Cargo.toml is clean.

**Acceptance.** `rg -i` (pattern 1.1) over `crates/search/src/` → **0**. Gates
green.

- [ ] 1.6 Clean `ingestion` narrative (17 files)

**Scope.** `crates/ingestion/src/`: `chunkers/{json,markdown}.rs`,
`entities/{cluster,mod,resolver,similarity}.rs`, `ner/{composite,llm_cache,llm,
llm_schema,mod,parse,prompts}.rs`, `parsers/{mediawiki,unstructured,webpage}.rs`,
`types.rs`. Paths already removed (uncommitted); strip the remaining narrative
("Oracle reference/mapping", "the oracle's NER result", Go `_test.go` names,
"ported") → describe the parsers/chunkers/NER natively.

**Acceptance.** `rg -i` (pattern 1.1) over `crates/ingestion/src/` → **0**.
Gates green.

- [ ] 1.7 Clean `mcp` crate + Cargo.toml (13 files, full)

**Scope.** `crates/mcp/src/`: `health.rs`, `lib.rs`, `pagination.rs`,
`server.rs`, `tools/{catalog,documents,dossier,entities_catalog,entity,facts,
graph_tools,search}.rs`, `transport/jsonrpc.rs`, and `crates/mcp/Cargo.toml`.
Full cleanup (paths + narrative): strip `//! Oracle mapping: ../synopsis/...`,
"the oracle's legacy SSE", `/// `../synopsis/internal/mcp/tools.go`
(`mcp-contract`)` → keep the `mcp-contract` pointer; `jsonrpc.rs` keep
"Wire reference: mcp-go v0.57.0" + handler refs, drop the path and "the
oracle's server". Cargo.toml → `# D1 edges.`.

**Acceptance.** `rg -i` (pattern 1.1) over `crates/mcp/src/ crates/mcp/
Cargo.toml` → **0**. Gates green. `mcp-contract` pointer + `mcp-go v0.57.0`
kept.

- [ ] 1.8 Clean `cli` crate + Cargo.toml (14 files, full)

**Scope.** `crates/cli/src/`: `cli.rs`, `config_resolver.rs`, `lib.rs`,
`loadtest/{filler,generator,mod,report,runner}.rs`, `model.rs`,
`onnx_runtime.rs`, `serve/{bootstrap,health,server,watcher}.rs`, and
`crates/cli/Cargo.toml`. Full cleanup: strip `//! Oracle mapping: ../synopsis/
cmd/app/...`, `//! Oracle: ../synopsis/internal/benchmark/...`, "the oracle",
Go file names → describe the subcommands/loadtest/serve natively (keep the
config-resolution rule, binary-name note). Cargo.toml → `# D1 edges.` (keep the
binary-name note without "Go oracle artifact").

**Acceptance.** `rg -i` (pattern 1.1) over `crates/cli/src/ crates/cli/
Cargo.toml` → **0**. Gates green.

## 2 — Top-level living docs

- [ ] 2.1 Clean `AGENTS.md` + `README.md`

**Scope.** `AGENTS.md` and `README.md` (English).
- **AGENTS.md:** remove the `## Oracle` section and the `## Migration
  principles` section entirely; rewrite the intro line ("Rust rewrite of the Go
  service … bugs from the oracle does not exist") to describe a standalone Rust
  MCP server; strip "oracle reference paths", "fixtures recorded once from the
  Go binary", "ported from the oracle", "the oracle's legacy HTTP+SSE" → "the
  legacy HTTP+SSE transport", "mirrors the oracle's build-all platforms" →
  "the cross-build matrix", "parity testing against the Go oracle", "unlike the
  oracle", and the "transcribed from the Go oracle … until synced to main specs"
  sentence (the specs are now the reference). The Layout table header "Oracle
  mapping" → drop or replace with a neutral column. Keep: the frozen stack, hard
  constraints, commands, gotchas (reworded), the double-transport fact,
  D/ADR references.
- **README.md:** remove the "Migration status" oracle framing ("The Go original
  … is the oracle … throughout the migration", "all modules ported and
  parity-checked", "Parity was machine-checked during the migration … the port
  is complete") → describe a complete, standalone Rust service; reword "the
  oracle's legacy HTTP+SSE" → "the legacy HTTP+SSE transport".

**Acceptance.** `rg -i '\.\./synopsis|oracle|Go original|Go code|Go binary|
ported|re-architected|not transcribed|migration|deviation from the oracle'
AGENTS.md README.md` → **0**. Markdown still well-formed (headings/tables
intact).

- [ ] 2.2 Clean `config.yaml` + `docs/adr/**` (Russian)

**Scope.** `openspec/config.yaml` and all `docs/adr/*.md` (Russian; refs found
in `0001-sqlite-fts5`, `0002-onnx-runtime`, `0003-ann-engine` — verify the rest
have none). Strip the oracle/Go/
ported narrative **in Russian** (e.g. "Go-оригинал живёт в соседнем
репозитории ../synopsis и является ОРАКУЛОМ", "Оракул — референс по ПОВЕДЕНИЮ
и КОНТРАКТАМ", "НЕ копировать Go-оригинал 1:1", "ошибки оригинала не
повторяются", "parity-чек против оракула", "референс в Go-оригинале", Go file
names) → reframe as native design rationale. **Do NOT translate** (that is
change `translate-to-english`). Keep the design decisions, hard constraints,
and D/ADR references.

**Acceptance.** `rg -i '\.\./synopsis|оракул|оригинал|портирован|перенос.*из
Go|\.go\b|\.tmpl' openspec/config.yaml docs/adr/` → **0** (except legitimate
non-Go uses). `config.yaml` still valid YAML.

- [ ] 2.3 Clean `openspec/specs/**` (Russian)

**Scope.** all `openspec/specs/*/spec.md` (Russian; refs found in
`config-format`, `cli-surface`, `data-schema`, `db-storage`, `mcp-contract`,
`vector-index` — verify the rest have none). Strip the
oracle/Go/ported provenance **in Russian** (e.g. "зафиксированы из Go
оригинала", "источник истины — ../synopsis/...", "портировано из", Go file
names) → the specs describe the contract directly. **Do NOT translate.** Keep
the contract content and D/ADR references.

**Acceptance.** `rg -i '\.\./synopsis|оракул|оригинал|портирован|\.go\b|\.tmpl'
openspec/specs/` → **0**. Spec files still valid (headings/requirement blocks
intact).

## 3 — Verification

- [ ] 3.1 Final whole-repo verification

**Acceptance.**
1. `rg -i '\.\./synopsis' crates/ AGENTS.md README.md openspec/config.yaml
   docs/adr/ openspec/specs/` → **0**.
2. `rg -i 'oracle|Go original|Go code|Go binary|re-architected|not transcribed|
   deviations from the oracle|функциональная копия' crates/*/src/ AGENTS.md
   README.md` → **0** (crate + top-level English living docs).
3. `rg '\.\./synopsis|оракул|Go-оригинал' openspec/config.yaml docs/adr/
   openspec/specs/` → **0** (Russian living docs).
4. **Archive untouched:** `git diff --name-only HEAD -- openspec/changes/
   archive/` → empty; `rg -c '\.\./synopsis' openspec/changes/archive/`
   unchanged from baseline (281).
5. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo
   test` all green. No code (non-comment) files changed.
