# Synopsis (Rust)

A local RAG + knowledge-graph MCP server for personal use: ingest documents, extract entities and facts, answer questions with hybrid search over SQLite FTS5 plus an ANN index, and expose 12 MCP tools. One local binary, no external services; sized to run on a laptop (16 GB RAM).

## Status

Current state of this repository:

- **Done:** workspace skeleton — nine domain crates; CI with quality gates (fmt + clippy + test) and a 5-target cross-build matrix.
- **Complete:** all modules complete; every change is archived under `openspec/changes/archive/`; contract specs in `openspec/specs/`.

## Stack (frozen)

Rust 1.96.0 (pinned in `rust-toolchain.toml`) · tokio + axum · rusqlite — bundled, FTS5 compiled in-tree · rmcp 3.x over Streamable HTTP for MCP (the legacy HTTP+SSE is also served — double transport, override of design D8 by human decision 2026-08-31, change `add-legacy-sse-transport`; wire contract mcp-go v0.57.0) · ONNX runtime as an external `.so`/`.dylib` (bge-m3 int8 embeddings + NER) · usearch ANN index, disk-backed and quantized (sole engine, ADR 0004). The full list with hard constraints is in [AGENTS.md](AGENTS.md).

## Commands

| Command | What it does |
|---|---|
| `cargo build --release` | builds the `synopsis` binary (`target/release/synopsis`; stub prints its version) |
| `cargo test` | full workspace suite — no services or network needed |
| `cargo fmt --check` | formatting gate |
| `cargo clippy --all-targets -- -D warnings` | lint gate — any warning fails the build |
| `cargo zigbuild --release --target <t>` | cross-compile for one of the 5 CI targets (needs Zig 0.16.0) |

Cross-build targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.

### Parity

Parity was machine-checked, not reviewed line-by-line: tool responses were recorded as fixtures and compared — JSON diffs of `tools/list` and tool-call responses, plus p50/p95 latency gates. The transitional harness that ran those checks has now been removed; the implementation is complete.

## Layout

See [AGENTS.md](AGENTS.md) for the crate table, dependency graph, execution model (AI writes / human reviews), and agent-facing rules.
