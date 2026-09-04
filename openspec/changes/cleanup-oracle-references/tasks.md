# Tasks: cleanup-oracle-references

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. The Go
project at `../synopsis` will be deleted, so the Rust docs must no longer
reference it or frame the code as a port. **Remove all migration-provenance
(oracle / Go / ported narrative) from all living docs, keeping the design
rationale reframed as native Rust decisions.** `openspec/changes/archive/**` is
NOT touched (historical audit trail).

## Rule (applies to every task — full detail + examples in design.md)

- **REMOVE:** any mention of the Go project/oracle (`oracle`, `the original`,
  `Go original/code/binary/service/project`, `../synopsis/...`), Go source
  file/symbol names (`*.go`, `*.tmpl`, and inline Go symbols like `r.mu`,
  `NewRunner`, `belongsToSource`, `SummaryStats`), and port/migration language
  (`ported`, `faithful port`, `re-architected`, `not transcribed`,
  `functional copy`, `migration principle(s)`, `deviations from the oracle`,
  `verified against the oracle`, `parity with the oracle`). Inline
  parentheticals like `(oracle \`r.mu\`)` / `(oracle \`SummaryStats\`)` are
  removed whole. The AGENTS.md `## Oracle` and `## Migration principles`
  sections are removed.
- **KEEP (reframe):** the design rationale / WHY (as a native decision, e.g.
  "Design: silent defaults are replaced by fail-fast validation"), behavioral
  and algorithm descriptions, the project's own `D1…D8` / `ADR 0001…0005`
  references, and wire-format versions (`mcp-go v0.57.0`, minus "the oracle's").
- **DO NOT remove** the legitimate **DB-migration** concept (`migrations`,
  `PRAGMA user_version`, "the v5 migration shape", "not in the migrations") —
  that is a real Rust/SQLite concept, not the Go-project migration.
- **DO NOT remove** the Rust code's **own** `.tmpl` template files (e.g.
  `crates/ingestion/src/ner/templates/{system,user}.tmpl`, loaded via
  `include_str!`) — those are legitimate minijinja assets, not Go files. Only
  remove the *framing* that presents them as a Go origin ("the functional
  rewrite of the `.tmpl` template" → "the embedded `system.tmpl` template").
- **Code-level oracle references are ALSO removed** (human decision 2026-09-03,
  option A): test function names (e.g. `matches_oracle_cases` →
  `matches_recorded_cases`, `like_the_oracle` → `normalizes_to_defaults`) and
  test string literals (assert/`expect` messages, e.g. `"the oracle's
  TestGraphStats"` → drop the oracle clause). These are low-risk, **no behavior
  change** — only identifiers and message text. `cargo fmt/clippy/test` stay
  green.
- **Acceptance pattern** (per scope): `rg -i '\.\./synopsis|oracle|Go
  (original|code|binary|service|project)|\bported\b|re-architected|not
  transcribed|functional (copy|rewrite)|\.go\b' <scope>` → **0**. (No `.tmpl`
  term: the Rust code's own `*.tmpl` templates are legitimate.) NOTES:
  `\bported\b` is word-bounded on purpose — the substring `ported` inside
  legitimate identifiers (`supported_extensions`, `UnsupportedExtension`,
  `reported`, `supported`) is a false positive and must NOT be "fixed" by
  renaming public API. `oracle` is targeted everywhere (doc comments, inline
  comments, test fn names, test string literals) per option A.

## 1 — Crates (whole-crate scope; the narrative is in more files than just the
path-ref files)

- [x] 1.1 Clean `db` crate (~136 mentions)

**Scope.** all of `crates/db/src/`. Strip oracle/Go/ported narrative from every
doc comment; reframe "deviations from the oracle" as "Design:", keep the
rationale (D1/D2/D3/ADR 0001/D8, sealed-enum, DRY-composition, PRAGMA parity).

**Acceptance.** the pattern above over `crates/db/src/` → **0**. Gates green. No
code changed. DB-migration terms preserved.

- [x] 1.2 Clean `config` + `llm` crates (~145 mentions)

**Scope.** all of `crates/config/src/` and `crates/llm/src/`. Strip
"ports the Go oracle's `internal/config` package", "Deliberate deviation(s) from
the oracle" → "Design:", Go file names. Keep the rationale (fail-fast
validation, plain-string content, injectable sleeper, onnx.yaml registry).

**Acceptance.** the pattern above over `crates/config/src/ crates/llm/src/` →
**0**. Gates green.

- [x] 1.3 Clean `embedding` crate (~123 mentions)

**Scope.** all of `crates/embedding/src/`. Strip "re-architected, not
transcribed", "the oracle's `CacheKey`", "mirroring the oracle's
`DefaultMaxLength`", Go file names (`library.go`, …), "Deliberate deviations
from the oracle" → "Design decisions:". Keep retry/SSRF/timeout, CLS pooling,
tokenizer pad/attention_mask rationale. Cargo.toml already clean.

**Acceptance.** the pattern above over `crates/embedding/src/` → **0**. Gates
green.

- [x] 1.4 Clean `graph` crate (~210 mentions)

**Scope.** all of `crates/graph/src/`. Strip "The Go code is a reference…",
"Verified against the oracle", "functional copy, re-architected for Rust",
"ported verbatim", inline Go symbols. Keep hybrid storage, CEL compile-once, BFS
contract, petgraph `DiGraph` design.

**Acceptance.** the pattern above over `crates/graph/src/` → **0**. Gates green.

- [x] 1.5 Clean `search` crate (~105 mentions)

**Scope.** all of `crates/search/src/`. Strip "Faithful port of the oracle's
`rrf.go`", "The Go code is a reference…", Go file names → describe the
algorithms natively (RRF `score += 1/(k+rank)`, hybrid fusion, rerank,
expansion). Cargo.toml clean.

**Acceptance.** the pattern above over `crates/search/src/` → **0**. Gates green.

- [x] 1.6a Clean `ingestion` `parsers/` + `chunkers/` (~42 mentions)

**Scope.** `crates/ingestion/src/parsers/` (json, markdown, mediawiki, mod,
unstructured, webpage) and `crates/ingestion/src/chunkers/` (mediawiki). Strip
the remaining oracle/Go/ported narrative; reframe the rationale as native
design. (Paths and most of the crate were already cleaned in a prior partial
pass — clean only what remains; the crate still compiles and is fmt-clean.)

**Acceptance.** the pattern above over `crates/ingestion/src/parsers/
crates/ingestion/src/chunkers/` → **0**. Gates green. No behavior change.

- [x] 1.6b Clean `ingestion` top-level files (~53 mentions)

**Scope.** `crates/ingestion/src/{sources.rs, error.rs, job_queue.rs, types.rs,
progress.rs, lib.rs, worker.rs}`. Strip the remaining oracle/Go/ported
narrative; reframe the rationale.

**Acceptance.** the pattern above over those 7 files → **0**. Gates green. No
code changed.

- [x] 1.6c Clean `ingestion` `ner/` + `entities/` + `ingester/` + `runner/`
(~30 mentions)

**Scope.** `crates/ingestion/src/{ner/, entities/, ingester/, runner/}`. Strip
the remaining oracle/Go/ported narrative; reframe the rationale.

**Acceptance.** the pattern above over those 4 dirs → **0**, **and** the whole
crate `crates/ingestion/src/` → **0** (final verify). Gates green. No code
changed.

- [x] 1.7a Clean `mcp` `tools/` part 1 (~147 mentions)

**Scope.** `crates/mcp/src/tools/{facts.rs, entities_catalog.rs, catalog.rs}`.
Strip `//! Oracle mapping: ../synopsis/...`, inline Go symbols, "deviations
from the oracle" → "Design:". In doc comments that read
`` `../synopsis/...` (`mcp-contract`) ``, DROP the `../synopsis/...` path but
KEEP the `(`mcp-contract`)` pointer.

**Acceptance.** the pattern above over those 3 files → **0**. Gates green. No
behavior change.

- [x] 1.7b Clean `mcp` `tools/` part 2 (~117 mentions)

**Scope.** `crates/mcp/src/tools/{dossier.rs, graph_tools.rs, documents.rs,
search.rs, entity.rs}`. Same rule as 1.7a. OPTION A: rename the test function
`successful_search_shapes_the_oracle_response` (search.rs) → drop "oracle"
(e.g. `successful_search_shapes_response`).

**Acceptance.** the pattern above over those 5 files → **0**. Gates green. No
behavior change.

- [x] 1.7c Clean `mcp` `transport/` + top-level + Cargo.toml (~136 mentions)

**Scope.** `crates/mcp/src/transport/{jsonrpc.rs, sse.rs, mod.rs}`,
`crates/mcp/src/{server.rs, pagination.rs, lib.rs, health.rs, error.rs,
tools.rs}`, and `crates/mcp/Cargo.toml`. Strip "the oracle's legacy SSE" →
"the legacy SSE transport", "the oracle's server", inline Go symbols,
"deviations from the oracle". `jsonrpc.rs`: KEEP "Wire reference: mcp-go
v0.57.0" + handler refs, drop "the oracle's server". Cargo.toml: replace
`# D1 edges; oracle imports: ../synopsis/internal/mcp + internal/mcp/handlers`
→ `# D1 edges.` and drop the "byte-compatible with the Go oracle's
pagination.go" clause. OPTION A: rename the test function
`wire_format_matches_the_go_oracle` (pagination.rs) → drop "oracle".

**Acceptance.** the pattern above over the 1.7c scope files (`crates/mcp/
src/transport/`, the six top-level `crates/mcp/src/*.rs`, and
`crates/mcp/Cargo.toml`) → **0**. Gates green. No behavior change.
`mcp-contract` pointer + `mcp-go v0.57.0` kept.

- [x] 1.7d Clean `mcp` `tests/` (~83 mentions)

**Scope.** `crates/mcp/tests/{documents.rs, dossier.rs, graph_tools.rs,
server_units.rs, sse_units.rs, server_integration.rs}`. Same rule as the rest
of 1.7: strip oracle/Go/ported narrative from test docs/comments; keep the
`mcp-contract` pointers and behavioral assertions. OPTION A: rename any test
function names that contain "oracle" and edit any test string literals that
mention "oracle".

**Acceptance.** the pattern above over `crates/mcp/src/ crates/mcp/tests/
crates/mcp/Cargo.toml` → **0** (WHOLE crate, final verify). Gates green. No
behavior change.

- [ ] 1.8 Clean `cli` crate + Cargo.toml (~195 mentions)

**Scope.** all of `crates/cli/src/` (14+ files: `loadtest/*`, `serve/*`,
`cli.rs`, `config_resolver.rs`, `lib.rs`, `model.rs`, `onnx_runtime.rs`) and
`crates/cli/Cargo.toml`. Strip `//! Oracle mapping: ../synopsis/cmd/app/...`,
`//! Oracle: ../synopsis/internal/benchmark/...`, "the oracle", Go file names,
inline Go symbols → describe the subcommands/loadtest/serve natively (keep the
config-resolution rule, binary-name note without "Go oracle artifact").
Cargo.toml → `# D1 edges.`.

**Acceptance.** the pattern above over `crates/cli/src/ crates/cli/Cargo.toml`
→ **0**. Gates green.

- [ ] 1.9 Clean `vectors` + `utils` crates (~12 mentions)

**Scope.** all of `crates/vectors/src/` and `crates/utils/src/` (small). Strip
any oracle/Go/ported narrative; keep the design.

**Acceptance.** the pattern above over `crates/vectors/src/ crates/utils/src/`
→ **0**. Gates green.

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
  oracle", and the "transcribed from the Go oracle … until synced to main
  specs" sentence (the specs are now the reference). The Layout table header
  "Oracle mapping" → drop or replace with a neutral column. Keep: the frozen
  stack, hard constraints, commands, gotchas (reworded), the double-transport
  fact, D/ADR references.
- **README.md:** remove the "Migration status" oracle framing ("The Go original
  … is the oracle … throughout the migration", "all modules ported and
  parity-checked", "Parity was machine-checked during the migration … the port
  is complete") → describe a complete, standalone Rust service; reword "the
  oracle's legacy HTTP+SSE" → "the legacy HTTP+SSE transport".

**Acceptance.** `rg -i '\.\./synopsis|oracle|Go (original|code|binary)|ported|
re-architected|not transcribed|migration principle|deviation from the oracle'
AGENTS.md README.md` → **0** (note: `migration` alone is NOT targeted — only
"migration principle(s)"). Markdown still well-formed (headings/tables intact).

- [ ] 2.2 Clean `config.yaml` + `docs/adr/**` (Russian)

**Scope.** `openspec/config.yaml` and all `docs/adr/*.md` (Russian; refs found
in `0001-sqlite-fts5`, `0002-onnx-runtime`, `0003-ann-engine` — verify the rest
have none). Strip the oracle/Go/ported narrative **in Russian** (e.g.
"Go-оригинал живёт в соседнем репозитории ../synopsis и является ОРАКУЛОМ",
"Оракул — референс по ПОВЕДЕНИЮ и КОНТРАКТАМ", "НЕ копировать Go-оригинал 1:1",
"ошибки оригинала не повторяются", "parity-чек против оракула", "референс в
Go-оригинале", Go file names) → reframe as native design rationale. **Do NOT
translate** (that is change `translate-to-english`). Keep the design decisions,
hard constraints, and D/ADR references. Do NOT touch the legitimate DB-migration
text.

**Acceptance.** `rg -i '\.\./synopsis|оракул|оригинал|портирован|из Go|\.go\b|
\.tmpl' openspec/config.yaml docs/adr/` → **0** (except legitimate non-Go
uses). `config.yaml` still valid YAML.

- [ ] 2.3 Clean `openspec/specs/**` (Russian)

**Scope.** all `openspec/specs/*/spec.md` (Russian; refs found in
`config-format`, `cli-surface`, `data-schema`, `db-storage`, `mcp-contract`,
`vector-index` — verify the rest have none). Strip the oracle/Go/ported
provenance **in Russian** (e.g. "зафиксированы из Go оригинала", "источник
истины — ../synopsis/...", "портировано из", Go file names) → the specs
describe the contract directly. **Do NOT translate.** Keep the contract content
and D/ADR references. Do NOT touch legitimate DB-migration text.

**Acceptance.** `rg -i '\.\./synopsis|оракул|оригинал|портирован|из Go|\.go\b|
\.tmpl' openspec/specs/` → **0**. Spec files still valid (headings/requirement
blocks intact).

## 3 — Verification

- [ ] 3.1 Final whole-repo verification

**Acceptance.**
1. `rg -i '\.\./synopsis' crates/ AGENTS.md README.md openspec/config.yaml
   docs/adr/ openspec/specs/` → **0**.
2. `rg -i 'oracle|Go (original|code|binary|service|project)|re-architected|not
   transcribed|functional copy' crates/*/src/ AGENTS.md README.md` → **0**
   (English crate + top-level living docs).
3. `rg -i '\.\./synopsis|оракул|Go-оригинал|портирован' openspec/config.yaml
   docs/adr/ openspec/specs/` → **0** (Russian living docs).
4. **Archive untouched:** `git status --porcelain -- openspec/changes/archive/`
   → empty; `rg -c '\.\./synopsis' openspec/changes/archive/` = 281 (baseline).
5. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo
   test` all green. No code (non-comment) files changed. DB-migration terms
   still present (not over-removed).
