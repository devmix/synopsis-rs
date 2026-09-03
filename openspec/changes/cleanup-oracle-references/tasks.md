# Tasks: cleanup-oracle-references

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml` (binding
context). This change **removes all `../synopsis/...` path references from
production crate source and crate `Cargo.toml` comments**, preserving meaningful
behavioral/design/wire text. **No spec, contract, code-logic, or dependency
changes. `../synopsis`, `openspec/specs/**`, `openspec/changes/archive/**`,
test-fixture READMEs, migration SQL, and `docs/adr/**` stay untouched.**

## Rule (applies to every task below — see design.md for the full rationale)

For each `../synopsis/...` reference:
1. **Pure provenance tag** (`//! Oracle mapping: …`, `//! Oracle: …`,
   `//! Oracle reference: …`) → delete the whole line (drop dangling
   continuation lines that only elaborated the mapping).
2. **Meaningful text + path** (deviation notes, wire contracts, config rules,
   binary-naming notes) → remove the path, keep the text (reword so it reads).
3. **Stale parity-harness clause** (cites the removed harness as an acceptance
   criterion) → drop the clause.
4. **Cargo.toml** (`# D1 edges; oracle imports: …`) → keep `# D1 edges`, drop
   the `oracle imports:` clause; (`# Binary name … (../synopsis/bin/synopsis)`)
   → keep the note, drop the path.

All refs are `//!`/`///` doc comments or `#` Cargo comments — no code. After
each task, `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
`cargo test` must stay green.

## 1

- [x] 1.1 Clean `db` crate (14 files, 14 refs)

**Scope.** `crates/db/src/{app_kv,chunk_entity,chunk,connection,document,
entity_link,entity,entity_source,executor,fact,fact_source,gc,lib,utils}.rs`.
Every ref is a module-level `//! Oracle mapping: \`../synopsis/internal/
database/...\`` (or similar) tag → **rule 1: delete the whole tag line** (and a
dangling continuation if any).

**Acceptance.** `rg '\.\./synopsis' crates/db/src/` → **0**. `cargo fmt/clippy/
test` green. No code changed (doc comments only).

- [x] 1.2 Clean `config` + `llm` crates (3 files, 3 refs)

**Scope.**
- `crates/config/src/lib.rs` (1 ref): line ends
  `… the XML ontologies (task 3.x). Oracle mapping: \`../synopsis/internal/config\`.`
  → **rule 2:** drop the trailing `Oracle mapping: …` clause, keep
  `… the XML ontologies (task 3.x).`
- `crates/llm/src/lib.rs` (1 ref): `//! Deliberate deviation from the oracle
  (\`../synopsis/internal/llm/client.go\`):` → **rule 2:** `//! Deliberate
  deviation from the oracle:` (drop the path, keep the deviation).
- `crates/llm/src/client.rs` (1 ref): `//! Deliberate deviations from the oracle
  (\`../synopsis/internal/llm/client.go\`),` → **rule 2:** `//! Deliberate
  deviations from the oracle,` (keep the deviation list that follows).

**Acceptance.** `rg '\.\./synopsis' crates/config/src/ crates/llm/src/` → **0**.
`cargo fmt/clippy/test` green. Deviation notes preserved.

- [x] 1.3 Clean `embedding` crate + its Cargo.toml (7 files, 8 refs + 1)

**Scope.** `crates/embedding/src/{cache,downloader,library,lib,model,provider,
tokenizer}.rs` (`tokenizer.rs` has 2 refs) and `crates/embedding/Cargo.toml`.
Source refs are `//! Oracle mapping/reference: \`../synopsis/internal/
{embedding,onnx}/...\`` tags → **rule 1** (delete). `Cargo.toml` line
`# D1 edges; oracle imports: ../synopsis/internal/embedding + internal/onnx.` →
**rule 4:** `# D1 edges.`

**Acceptance.** `rg '\.\./synopsis' crates/embedding/src/ crates/embedding/
Cargo.toml` → **0**. `cargo fmt/clippy/test` green.

- [ ] 1.4 Clean `graph` crate (7 files, 10 refs)

**Scope.** `crates/graph/src/{cel,graph,lib,linker,metrics,prompts,traverser}.rs`
(`cel.rs`, `graph.rs`, `prompts.rs` have 2 refs each). Module-level
`//! Oracle mapping/reference: \`../synopsis/internal/{graph,relations}/...\``
tags → **rule 1** (delete). If any line carries meaningful CEL/linker design
text plus a path → **rule 2** (keep text, drop path).

**Acceptance.** `rg '\.\./synopsis' crates/graph/src/` → **0**. `cargo
fmt/clippy/test` green.

- [ ] 1.5 Clean `search` crate + its Cargo.toml (8 files, 8 refs + 1)

**Scope.** `crates/search/src/{enrich,expand,hybrid,lexical,lib,rerank,rrf,
semantic}.rs` and `crates/search/Cargo.toml`. Most are `//! Oracle mapping:
\`../synopsis/internal/search/...\`` tags → **rule 1**. **Special —
`rrf.rs`:** the line "differential parity with `../synopsis/internal/search/
rrf_test.go` is an acceptance criterion" → **rule 3: drop that clause** (the
parity harness is removed); keep "Faithful port of the oracle's `rrf.go` — every
numeric behavior is preserved" and the internal-simplification note.
`Cargo.toml` `# D1 edges; oracle imports: ../synopsis/internal/search (…)` →
**rule 4:** `# D1 edges.`

**Acceptance.** `rg '\.\./synopsis' crates/search/src/ crates/search/Cargo.toml`
→ **0**. `cargo fmt/clippy/test` green.

- [ ] 1.6 Clean `ingestion` crate (17 files, 18 refs)

**Scope.** `crates/ingestion/src/`: `chunkers/{json,markdown}.rs`,
`entities/{cluster,mod,resolver,similarity}.rs`, `ner/{composite,llm_cache,llm,
llm_schema,mod,parse,prompts}.rs` (`llm_cache.rs` has 2 refs),
`parsers/{mediawiki,unstructured,webpage}.rs`, `types.rs`. Module-level
`//! Oracle mapping/reference: \`../synopsis/internal/ingestion/...\`` tags →
**rule 1**. Test-module refs to `..._test.go` (e.g. `chunkers/json.rs`,
`chunkers/markdown.rs`, `parsers/*.rs`) are also `../synopsis` paths → remove
the path (rule 1/2). Meaningful design text (e.g. `ner/mod.rs` describing the
NER result shape) is kept with the path removed.

**Acceptance.** `rg '\.\./synopsis' crates/ingestion/src/` → **0**. `cargo
fmt/clippy/test` green.

- [ ] 1.7 Clean `mcp` crate + its Cargo.toml (13 files, 20 refs + 1)

**Scope.** `crates/mcp/src/`: `health.rs`, `lib.rs` (2 refs), `pagination.rs`,
`server.rs` (3 refs), `tools/{catalog,documents,dossier,entities_catalog,entity,
facts,graph_tools,search}.rs` (`catalog.rs`, `entities_catalog.rs`, `facts.rs`,
`search.rs` have 2 refs), `transport/jsonrpc.rs`, and `crates/mcp/Cargo.toml`.
Most are `//! Oracle mapping: \`../synopsis/internal/mcp/...\`` tags →
**rule 1**. **Special — `transport/jsonrpc.rs`:** `//! Wire reference: mcp-go
v0.57.0 (pinned in \`../synopsis/go.mod\`)` → **rule 2:** keep `//! Wire
reference: mcp-go v0.57.0` and the handler references, drop the
`(pinned in \`../synopsis/go.mod\`)` clause. **Special — `server.rs`:** the
inline `/// \`../synopsis/internal/mcp/tools.go\` (\`mcp-contract\`)` refs →
keep the `(\`mcp-contract\`)` pointer, drop the path. `Cargo.toml`
`# D1 edges; oracle imports: ../synopsis/internal/mcp + internal/mcp/handlers` →
**rule 4:** `# D1 edges.`

**Acceptance.** `rg '\.\./synopsis' crates/mcp/src/ crates/mcp/Cargo.toml` →
**0**. `cargo fmt/clippy/test` green. Wire reference (mcp-go v0.57.0) and
`mcp-contract` pointers preserved.

- [ ] 1.8 Clean `cli` crate + its Cargo.toml (14 files, 15 refs + 2)

**Scope.** `crates/cli/src/`: `cli.rs`, `config_resolver.rs`, `lib.rs`,
`loadtest/{filler,generator,mod,report,runner}.rs`, `model.rs` (2 refs),
`onnx_runtime.rs`, `serve/{bootstrap,health,server,watcher}.rs`, and
`crates/cli/Cargo.toml`. Module-level `//! Oracle mapping: \`../synopsis/cmd/
app/...\`` and `//! Oracle: \`../synopsis/internal/benchmark/...\`` tags →
**rule 1**. **Special — `config_resolver.rs`:** `//! (\`../synopsis/cmd/app/
main.go\`): an explicit \`--config\` path wins outright; …` → **rule 2:** keep
the config-resolution rule text, drop the path. **Special — `model.rs`:** the
inline `/// (\`../synopsis/internal/utils/human_size_test.go\`).` → drop the
path, keep the surrounding note. `Cargo.toml`: line 9 `# Binary name matches the
Go oracle artifact (../synopsis/bin/synopsis).` → **rule 4:** keep the note,
drop the path; line 14 `# D1 edges; oracle imports: ../synopsis/cmd/app (…)` →
**rule 4:** `# D1 edges.`

**Acceptance.** `rg '\.\./synopsis' crates/cli/src/ crates/cli/Cargo.toml` →
**0**. `cargo fmt/clippy/test` green. Config-resolution and binary-naming notes
preserved.

- [ ] 1.9 Final whole-workspace verification

**Goal.** Confirm the entire production tree is clean and nothing out of scope
was touched.

**Acceptance.**
1. `rg '\.\./synopsis' crates/*/src/ crates/*/Cargo.toml` → **0** matches.
2. KEEP-set provenance is **untouched** — baseline counts must hold exactly:
   `openspec/specs/` = 8, `openspec/changes/archive/` = 281, `docs/adr/` = 6,
   `crates/config/tests/data/README.md` + `fixtures/README.md` = 19
   (check each with `rg -c '\.\./synopsis' <path>`).
3. `git diff --name-only` shows only files under `crates/*/src/` and
   `crates/*/Cargo.toml` (no spec, archive, ADR, fixture, migration, or code
   file).
4. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
   all green.
