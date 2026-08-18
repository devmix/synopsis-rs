# Synopsis (Rust)

Rust rewrite of [Synopsis](https://github.com/devmix/synopsis) — a local RAG + knowledge-graph MCP server for personal use: ingest documents, extract entities and facts, answer questions with hybrid search over SQLite FTS5 plus an ANN index, and expose the same 12 MCP tools as the original. One local binary, no external services; sized to run on a laptop (16 GB RAM).

## Migration status

The Go original in the sibling repository `../synopsis` is the **oracle**: its behavior, tests, and contracts are the source of truth for parity throughout the migration. Current state of this repository:

- **Done:** workspace skeleton — nine domain crates plus `parity-harness`; CI with quality gates (fmt + clippy + test) and a 5-target cross-build matrix; the parity mechanism itself (rmcp client wrapper with p50/p95 timing, fixture loader API, diff utilities).
- **In progress:** module-by-module porting, one OpenSpec change at a time. The work queue lives in `openspec/changes/<change>/tasks.md`; contract specs in `openspec/specs/`.

## Stack (frozen)

Rust 1.96.0 (pinned in `rust-toolchain.toml`) · tokio + axum · rusqlite — bundled, FTS5 compiled in-tree · rmcp 3.x over Streamable HTTP for MCP (the oracle's legacy SSE transport is intentionally not reproduced — design D8) · ONNX runtime as an external `.so`/`.dylib` (bge-m3 int8 embeddings + NER) · usearch or lance ANN index, disk-backed and quantized (engine decided by benchmark in `native-seam-spikes`). The full list with hard constraints is in [AGENTS.md](AGENTS.md).

## Commands

| Command | What it does |
|---|---|
| `cargo build --release` | builds the `synopsis` binary (`target/release/synopsis`; stub prints its version) |
| `cargo test` | full workspace suite — no services or network needed |
| `cargo fmt --check` | formatting gate |
| `cargo clippy --all-targets -- -D warnings` | lint gate — any warning fails the build |
| `cargo zigbuild --release --target <t>` | cross-compile for one of the 5 CI targets (needs Zig 0.16.0) |
| `cargo test -p parity-harness` | parity harness: percentile unit tests + in-process MCP round-trip |

Cross-build targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.

### Parity

Parity is machine-checked, not reviewed line-by-line: the Go oracle's tool responses are recorded once as fixtures, then compared against the Rust server through the `parity-harness` MCP client (rmcp over Streamable HTTP) — JSON diffs of `tools/list` and tool-call responses, plus p50/p95 latency gates. The harness mechanism exists now; actual parity cases arrive together with each module change, and a task's acceptance gate is "its own cases are green".

## Layout

See [AGENTS.md](AGENTS.md) for the crate table, dependency graph, execution model (AI writes / human reviews), and agent-facing rules.
