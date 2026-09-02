# Content-parity golden fixtures

Recorded once from the Go oracle (`../synopsis/bin/synopsis`) over the content
corpus (task 1.2). The content-parity tests (tasks 1.4/1.5) compare the Rust
tool responses against these files after `content_parity::normalize`. Each
`.json` file is the oracle's exact response payload (raw, pretty, keys
sorted); this file is the header that makes the set self-describing and
re-recordable.

## Corpus (task 1.2, `corpus::write_content_corpus`)

8 static markdown documents across 3 domains — `hr/` (3), `product/` (3),
`eng/` (2) — with a recurring entity vocabulary (Dana Kovac, Marcus Webb,
Priya Sharma; Atlas, Portal, Ledger, Beacon; Vacation Policy, Remote Work
Policy, API v2 deprecation). No randomness or timestamps.

## Go recording config (`target/parity-go/parity.yaml`)

- model: **bge-small-en-v1.5, 384-dim** (explicit `model_path`, offline —
  no download); ONNX runtime 1.28.0 copied from `workspace/onnxruntime`.
- NER: **disabled** (`ingestion.ner.disabled: true`) — no entities/facts.
- graph: **disabled** (`graph.enable_graph: false`); linker: **disabled**
  (`linker.disabled: true`).
- chunking (markdown): `hybrid`, max 8192, overlap 100, min section 500.
- search: `rrf_k 20`, lexical/semantic `top_k 20`, final `top_k 10`, both
  legs enabled, `timeout_ms 10000`.
- `global.xml`: 3 sources (one per domain sub-directory), each with its own
  domain, so each document carries its sub-directory's domain.

Note: the Go `ApplyDefaults` forces non-zero search boosts
(`deprecated 0.2`, `official 1.5`, `recent 1.2 / 90d`, `authority default 1.0`)
that the Rust harness leaves at zero. For this corpus they are neutral to
rank order (all docs share one ingestion timestamp → uniform recent boost;
no deprecated/official content; authority `1.0`), and `normalize` strips the
exact `score` — so only rank order (identity fields) is compared.

## Per-fixture tool args

| fixture | tool | args |
|---|---|---|
| `search.json` | `search` | `{"query": "Atlas dashboard builder", "top_k": 5}` |
| `catalog_overview.json` | `catalog_overview` | `{}` |
| `catalog_documents.json` | `catalog_documents` | `{"page_size": 3}` |
| `catalog_entities.json` | `catalog_entities` | `{}` (empty — NER disabled) |

## Transport (deviation from the task body)

The Go oracle (mcp-go v0.57.0 `NewSSEServer`) serves the **legacy SSE**
transport only (`GET /sse` + `POST /message`); `POST /mcp` (Streamable HTTP)
answers `404`. `content_parity::record_response` uses the Streamable-HTTP
`McpClient`, so it cannot reach the Go server. The recorder
(`examples/record_content.rs`) drives the oracle with the harness `SseClient`
and writes the fixtures with `record_response`'s byte semantics.

## Re-record

```sh
cargo run -p parity-harness --example record_content
```
