## Why

The parity harness currently checks **latency** only (p50/p95 of the `search` tool vs the Go
oracle baseline) and **tool coverage** (all 12 frozen tools respond). It does **not** check
**response content** — whether the Rust tool actually returns the same *results* the Go oracle
does for the same input. The migration principle ("no 1:1 copy; parity is checked by the
machine against fixtures recorded once from the Go binary") requires content-level parity to
catch subtle behavior divergences (result counts, response schema, ordering, field values)
that a latency gate cannot see. Without it, a Rust regression that returns the *right shape*
but the *wrong rows* would pass every gate.

## What Changes

- Extend `parity-harness` with **golden-file content parity**: a record/verify helper that
  (a) records tool responses from a running MCP server into committed JSON fixtures, and
  (b) verifies a Rust tool response against a fixture using the existing `json_diff`
  comparator, after **normalization** that strips volatile, implementation-defined fields
  (exact scores, internal ordering, timestamps) and keeps the contract-relevant fields.
- Add an **expanded, deterministic content-parity corpus** (5–10 markdown docs across 2–3
  domains, varied topics) — separate from the latency test's 2-doc corpus, so the existing
  latency gates are untouched.
- **Record golden fixtures once from the Go binary** (`../synopsis/bin/synopsis`) using a
  custom config that matches the Rust parity harness exactly (bge-small 384-dim, NER
  disabled, graph disabled, linker disabled), then commit the fixtures.
- Add **content-parity tests** for the `search` tool (result count + top-result identity)
  and the catalog tools (`catalog_overview` counters, `catalog_documents` pagination,
  `catalog_entities` listing).

## Capabilities

### New Capabilities
None.

### Modified Capabilities
None.

This is a **test-infrastructure** change: it adds machine-checked content parity and does not
change any product behavior or frozen contract, so it sets `skip_specs: true` (no spec
deltas). The frozen MCP tool **schemas** it verifies against are already specified in
`openspec/changes/scaffold-rust-project/specs/mcp-contract`.

## Impact

- **Frozen contracts: NONE affected.** MCP tools, CLI surface, data schema, and config
  formats are unchanged. This change *verifies* the existing content behavior; it changes
  nothing in the product.
- **Scope:** `crates/parity-harness/**` (new `content_parity` module, new corpus, new
  fixtures, new tests) only. No product crate, no `Cargo.toml`, no `Cargo.lock` change.
- **Go oracle:** read-only. The Go binary is driven to *record* fixtures once; it is never
  modified. A custom (temporary) Go config is used for recording and is not committed to
  `../synopsis`.
- **CI:** the new content-parity tests run in the existing `cargo test --workspace` gate.
  They graceful-skip (like the latency test) when the bge-small model / ONNX runtime is
  unavailable.

## Non-goals

- **No NER / entity-fact / graph content parity** — the test environment has no NER model
  and the Rust parity harness runs with the graph disabled (`GraphIndex::Unavailable`); the
  entity/fact tools return tool-level errors by design on the corpus. Deferred to a
  follow-up change once an NER model is present.
- **No new embedding model** — content parity reuses the existing bge-small 384-dim model
  (the one the Go oracle's default config already uses), so no download or new fixture.
- **No changes** to product code, the frozen stack, `Cargo.toml`, or `Cargo.lock`.
- **No coverage work** — that is the separate change `coverage-rust-workspace`.
- **No branch-level or fuzz parity** — content parity is golden-file, deterministic.
