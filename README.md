# Synopsis

[![CI](https://img.shields.io/github/actions/workflow/status/devmix/synopsis-rs/ci.yml?label=ci&branch=main)](https://github.com/devmix/synopsis-rs/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/devmix/synopsis-rs?include_prereleases)](https://github.com/devmix/synopsis-rs/releases)
[![License](https://img.shields.io/github/license/devmix/synopsis-rs)](LICENSE)
[![Rust 1.96.0](https://img.shields.io/badge/rust-1.96.0-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![MCP Server](https://img.shields.io/badge/MCP-server-8A2BE2)](https://modelcontextprotocol.io)
[![SQLite FTS5 + usearch](https://img.shields.io/badge/SQLite-FTS5%20+%20usearch-003B57?logo=sqlite&logoColor=white)](https://www.sqlite.org/)
[![ONNX Runtime](https://img.shields.io/badge/embedding-ONNX%20Runtime-3E8EDE)](https://onnxruntime.ai)

A local RAG + knowledge-graph MCP server in Rust — one binary, no external
services, built for a 16 GB laptop. It ingests your documents (Markdown,
JSON, web pages), builds a hybrid index (SQLite FTS5 + disk-backed quantized
ANN via usearch), extracts entities and links them across domains into a
knowledge graph, and serves everything over MCP (12 read-only tools).

- **Hybrid search** — lexical (FTS5) + semantic (vector) with RRF fusion and
  recency/authority reranking
- **Knowledge graph** — entity extraction (NER via ONNX), cross-domain entity
  linking (LLM-based), CEL linkers, petgraph index
- **MCP server** — 12 tools over Streamable HTTP (`/mcp`) with the legacy
  HTTP+SSE transport (`GET /sse` + `POST /message`) also served, plus
  `GET /health`
- **Disk-backed ANN** — usearch HNSW (mmap, scalar-quantized, WAL + segments);
  the query path never loads the embedding model; vectors survive unclean
  shutdowns: the RAM layer is persisted after each ingestion cycle, and missing
  vectors are re-embedded at startup (the self-heal)
- **Self-contained build** — SQLite/FTS5 compiled in-tree (bundled), no CGO,
  no system dependencies; the ONNX runtime `.so`/`.dylib` and model weights
  are downloaded by the binary on demand (verified by URL + size + SHA-256)

## Building from source

Requires the pinned toolchain (see `rust-toolchain.toml`, Rust **1.96.0**);
rustup installs it automatically on the first cargo command.

```sh
cargo build --release     # → target/release/synopsis
cargo test                # full workspace suite; no services or network needed
```

Prebuilt archives for the 5 supported targets (Linux amd64/arm64, Windows
amd64, macOS arm64/amd64) are published as GitHub Releases on `v*` tags — the
archive bundles the stripped binary, this README, `workspace/configs/` and
the `edtech` demo ontology.

## Quick start

```sh
synopsis onnx-runtime install   # download the ONNX runtime per workspace/configs/onnx.yaml
synopsis model download         # download the default embedding model
synopsis serve                  # start the MCP server (port 8080, preset "default")
```

Without `--config`, the config is auto-searched:
`<exeDir>/workspace/configs/` → parent directory → CWD; the default preset is
`config.default.yaml` next to `onnx.yaml` (the model/runtime registry).
`--preset NAME` selects `config.{NAME}.yaml`; `--dataset NAME` overrides the
configured dataset.

## CLI

```
synopsis [--config PATH] [--preset NAME] [--dataset NAME] <subcommand>
```

| Subcommand | Purpose |
|---|---|
| `serve [--no-initial-sync] [--port N] [--auto-rebuild-vectors]` | start the MCP server with initial sync + file watching |
| `queue status\|reset-retries` | inspect/repair the event task queue |
| `db stats\|clear` | dataset statistics / delete all dataset state |
| `model list\|download\|delete\|info\|benchmark [NAME]` | manage embedding models |
| `onnx-runtime install\|status\|uninstall` | manage the ONNX runtime library |
| `load-test [--scale small\|medium\|large] [--seed N] [--iterations N] [--json PATH] [--no-fill]` | benchmark all MCP tool handlers on generated data |

## Data layout

Runtime state lives under `workspace/` (gitignored; created and downloaded by
the binary):

```
workspace/
├── configs/                  # tracked: presets, onnx.yaml, prompt templates
├── datasets/<name>/
│   ├── ontology/             # tracked for the edtech demo dataset
│   ├── content/              # tracked for the edtech demo dataset
│   └── state/                # knowledge.db + vectors (per dataset)
├── db/cache/cache.db         # global cache DB (embedding cache, manifests)
├── models/                   # downloaded model weights
└── onnxruntime/              # downloaded ONNX runtime library
```

The knowledge DB is always built from scratch by the Rust binary (one
squashed init migration, `PRAGMA user_version` as the sole schema state) —
existing `knowledge.db` files are never opened or upgraded.

## MCP tools

`search`, `catalog_overview`, `catalog_documents`, `catalog_entities`,
`search_entities_by_type`, `search_facts`, `get_document_context`,
`get_chunk_by_id`, `get_fact_by_id`, `get_entity_dossier`,
`get_entity_relations`, `get_entity_links` — all read-only, facts restricted
to `approved` status. Wire contract: `openspec/specs/mcp-contract/`.

## Development

- Gates: `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` ·
  `cargo test` (CI runs all three; cross-builds only on `v*` tags)
- Coverage: `cargo llvm-cov --workspace --html` — measure-first, no gates
  (see `COVERAGE.md`)
- Cross-builds: `cargo zigbuild --release --target <t>` with Zig 0.16.0
  (targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`)

Workspace crates: `config` (presets/ontology), `db` (SQLite/FTS5/migrations),
`vectors` (ANN engine), `embedding` (ONNX runtime), `ingestion` (parsers,
chunkers, NER, event task queue), `graph` (knowledge graph + linkers), `search`
(hybrid/RRF), `mcp` (server + tools), `llm` (LLM client), `utils` (shared
helpers), `cli` (the `synopsis` binary).

Architecture decisions: `docs/adr/`. Spec-driven workflow: `openspec/`.
Agent instructions: `AGENTS.md`.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
