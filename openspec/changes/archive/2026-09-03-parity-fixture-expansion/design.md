## Context

- The parity harness (`crates/parity-harness`) already has: `parity_test.rs` (boots the
  product MCP server in-process over a 2-doc corpus, drives all 12 frozen tools, gates
  `search` latency p50 ≤ 2× / p95 ≤ 5× vs the Go baseline), `sse_parity.rs` (legacy SSE
  transport, 3 tools), `diff.rs` (`json_diff` — a strict structural JSON comparator, and
  `text_diff`), `fixtures.rs` (`FixtureSet` / `load_fixture_set_from_dir`, loads the
  `vectors.bin` fixture), and `mcp_client.rs` (`McpClient::connect` / `list_tools` /
  `call_tool` / timing stats).
- **`json_diff` is strict:** it reports every diverging path, including volatile fields
  (exact scores, array ordering, internal ids). Content parity therefore needs a
  **normalization** step that strips those fields before comparison.
- **Go oracle CLI** (`../synopsis/cmd/app/main.go`): `sync` (ingest), `serve` (MCP server),
  `load-test` (how the latency baseline was recorded). Global flags: `-config`, `-preset`,
  `-db`, `-version`. The binary is already built at `../synopsis/bin/synopsis`.
- **Model match (key finding):** the Go oracle's **default config already uses
  `bge-small-en-v1.5` / `vector_dim: 384`** — the exact model the Rust parity harness uses.
  So embeddings are model-matched and `search` results are comparable. (The production
  bge-m3 1024-dim model is a separate, larger model; parity deliberately uses the small one
  for a fast, deterministic, model-matched comparison.)
- **Config match:** the Go default config also has `graph.enable_graph: true` and
  `linker.disabled: false` (LLM cross-domain linking), which do NOT match the Rust parity
  harness (`GraphIndex::Unavailable`, no linker, NER disabled via
  `ingest_cfg.ner.disabled = true`). Recording therefore needs a **custom Go config** that
  matches the Rust harness: `ner.disabled: true`, `graph.enable_graph: false`,
  `linker.disabled: true`, `embeddings.local = bge-small 384`.
- **Catalog vs search determinism:** the catalog tools (`catalog_overview`,
  `catalog_documents`, `catalog_entities`) are deterministic DB queries — for the same
  ingested corpus they return the same rows regardless of embedding model. `search` is
  embedding-dependent, but is model-matched (bge-small on both sides), so it is also
  deterministic for a fixed corpus + config.

## Goals / Non-Goals

**Goals:**
- Machine-checked **content parity** for `search` + the catalog tools, against fixtures
  recorded once from the Go binary.
- A reusable **record/verify** golden-file mechanism with **normalization** of volatile
  fields.
- An **expanded, deterministic corpus** that exercises pagination and multi-domain listing.
- Zero product-code, frozen-contract, or `Cargo.lock` impact; graceful-skip when the model
  is absent.

**Non-Goals:**
- No NER / entity-fact / graph content parity (no NER model; graph disabled in the harness).
- No new embedding model or download.
- No changes to product code, the frozen stack, `Cargo.toml`, or `Cargo.lock`.
- No coverage work (separate change).

## Decisions

### D1 — Content-parity mechanism: golden JSON fixtures + normalization + `json_diff`

**Choice:** a new `content_parity` module in `parity-harness` that:
1. **Record:** given a running MCP server URL and a tool name + args, calls the tool via
   `McpClient` and writes the response JSON to `fixtures/content/<tool>.json` (committed).
2. **Verify:** loads the fixture, calls the Rust tool, **normalizes** both the fixture and
   the live response (D4), and asserts `json_diff(normalized_fixture, normalized_actual)`
   is empty.

**Why over alternatives:**
- **vs re-recording in CI:** recording requires the Go binary + a model + a config; doing it
  in CI is slow and brittle. Record once, commit the fixtures, and CI only *verifies* the
  Rust side (fast, no Go binary needed in CI).
- **vs field-by-field assertions:** a per-field `assert_eq!` per tool is brittle and
  duplicates across tools; `json_diff` + normalization is generic, reports every diverging
  path, and is reusable for future tools (entity/fact) without new code.

### D2 — Corpus: expanded, deterministic, separate from the latency corpus

**Choice:** a new `write_content_corpus(corpus: &Path)` writer producing **8 markdown docs
across 3 domains** (e.g. `hr/`, `product/`, `eng/`), each domain with distinct topics and a
consistent entity vocabulary (people, systems, policies) so multi-domain listing and
pagination are exercised. Content is **static string literals** (no random, no timestamps)
so ingestion is byte-for-byte deterministic.

**Why separate from `write_corpus` (the latency 2-doc corpus):** the latency test's p50/p95
baseline was recorded against a fixed corpus; changing it would invalidate the latency gate.
The content-parity corpus is a distinct, richer input for the new tests only.

### D3 — Recording: one-time, custom Go config matching the Rust harness

**Choice:** a documented, repeatable recording procedure (a small `record` test/binary in
`parity-harness`) that:
1. Builds a **temporary Go config** (written to a scratch dir, NOT committed to
   `../synopsis`) with: `embeddings.local = bge-small-en-v1.5 / 384`, `ner.disabled: true`,
   `graph.enable_graph: false`, `linker.disabled: true`, and the same `search` /
   `ingestion.chunking` values the Rust harness uses.
2. Runs `../synopsis/bin/synopsis -config <scratch>/parity.yaml -db <scratch>/knowledge.db
   sync` to ingest the content corpus.
3. Runs `… serve --port <free-port>`, connects `McpClient`, and records the responses for
   the in-scope tools (`search`, `catalog_overview`, `catalog_documents`,
   `catalog_entities`) into `fixtures/content/*.json`.
4. Shuts the Go server down.

**Why:** the fixtures must come from the **Go oracle** (the source of truth). Matching the
config to the Rust harness (same model, NER/graph/linker off) makes the Go and Rust inputs
identical, so the only differences `json_diff` can surface are genuine behavior divergences.

### D4 — Normalization: strip volatile fields, keep contract fields

**Choice:** a `normalize(value: Value, tool: &str) -> Value` step applied to **both** the
fixture and the live response before `json_diff`, that:
- **Strips exact numeric scores / confidence / rank** (implementation-defined floats that
  differ by epsilon across runtimes).
- **Sorts result arrays** that are unordered by contract (e.g. `catalog_entities` listing)
  by a stable key (id / name) so ordering is not a false positive; keeps `search` result
  order as-is (rank order IS contract-relevant) but compares only the identity fields
  (doc id / chunk id / title), not the score.
- **Strips timestamps / durations / server version / request ids** (environment-defined).
- **Keeps:** result counts, ids, titles, entity types, pagination cursors/offsets, and every
  contract field from `mcp-contract`.

**Why:** without normalization, `json_diff` flags epsilon score differences and array-order
noise, making the test flaky and useless. Normalization makes the comparison focus on the
contract-relevant content. The exact set of stripped fields is defined per tool in the
`content_parity` module and documented in each fixture's header comment.

### D5 — Scope: `search` + catalog; defer NER / entity-fact / graph

**Choice:** this change adds content parity for `search`, `catalog_overview`,
`catalog_documents`, and `catalog_entities`. The entity/fact tools (`get_entity_dossier`,
`get_entity_relations`, `get_entity_links`, `search_facts`, `get_fact_by_id`) and the graph
tools are **out of scope**: the test environment has no NER model (so no entities/facts are
extracted) and the Rust harness runs with the graph disabled, so those tools return
tool-level errors by design on the corpus — there is no positive content to compare.

**Why:** content parity is only meaningful where the corpus produces positive results. The
catalog + search tools do; the entity/fact/graph tools do not (until an NER model is
present). Deferring them keeps this change focused and avoids asserting on error responses.

## Risks / Trade-offs

- **[Recording setup is fiddly (Go config + model + ingest + serve)]** → mitigated: the Go
  default config already uses bge-small 384; the custom config only flips 3 booleans
  (NER/graph/linker). The procedure is documented and repeatable; the fixtures are recorded
  once and committed, so CI never needs the Go binary.
- **[Non-deterministic `search` results across runtimes]** → mitigated: both sides use
  bge-small 384, deterministic chunking, NER/graph off; normalization strips scores and
  compares identity fields, so epsilon/FFI drift does not cause false failures.
- **[Normalization over-strips and hides a real divergence]** → mitigated: the stripped-field
  set is minimal and per-tool documented; `search` keeps rank order and compares identity,
  so a wrong row still fails. A future change can tighten the normalization.
- **[Corpus expansion could drift the latency test]** → mitigated: the content corpus is a
  separate writer; the latency test's `write_corpus` is untouched.
- **[Fixture staleness if the corpus or config changes]** → mitigated: fixtures carry a
  header comment with the corpus hash + Go config; the verify test re-ingests the same
  corpus, so a corpus edit changes the Rust result and (if the Go fixture is stale) fails —
  surfacing the need to re-record.

## Migration Plan

1. Add the `content_parity` module (record/verify + normalization) — no behavior change.
2. Add the `write_content_corpus` writer — no behavior change.
3. Record the golden fixtures from the Go binary (one-time) and commit them.
4. Add the content-parity tests for `search` + catalog (graceful-skip without the model).
5. Verify gates: `cargo test --workspace` green (new tests pass or graceful-skip); the
   existing latency test is unchanged.
- **Rollback:** delete the `content_parity` module, the corpus writer, the fixtures, and the
  new tests. No product code or contract is touched, so rollback is clean.

## Open Questions

- None blocking. The one-time recording (task 1.3) is the only step that needs the Go
  binary + model locally; if the environment cannot run the Go binary, the fixtures are
  recorded on a machine that can, and only the committed fixtures + verify tests land in
  CI. All design decisions (mechanism, corpus, recording, normalization, scope) are
  user-approved (2026-09-02).
