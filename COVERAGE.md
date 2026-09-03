# Coverage

Line-coverage measurement and per-crate targets for this workspace.

Tool: **`cargo-llvm-cov` v0.9.0** — a cargo-installed binary, dev-only. It is not a
workspace dependency: it never appears in any `Cargo.toml` or `Cargo.lock` and never
ends up in the product binary (same pattern as the `cargo-zigbuild` CI tool).

## Setup

```bash
rustup component add llvm-tools-preview
cargo install --locked cargo-llvm-cov
```

Both are no-ops if already installed. The toolchain is pinned to Rust 1.96.0
(`rust-toolchain.toml`) = LLVM 19, within `cargo-llvm-cov`'s supported range (LLVM 19–22).

## Local commands

| Command | What it does |
|---|---|
| `cargo llvm-cov --workspace --html` | HTML report in `target/llvm-cov/html/` — open `index.html` |
| `cargo llvm-cov --workspace --text` | Per-file table (`Lines`, `Missed Lines` columns) |

CI runs the same measurement in the `coverage` job (`.github/workflows/ci.yml`) and
uploads an `lcov.info` artifact. That job runs parallel to `checks` and never blocks
the core gate.

## Per-crate targets

Targets are documented ranges, **not gates** (see [Status](#status-measure-first)).

| Crate | Target | Rationale |
|---|---|---|
| `db` | 70–80% | core data layer, DAO/FTS5 logic |
| `search` | 70–80% | hybrid search, RRF fusion, enrichment |
| `vectors` | 70–80% | ANN engine, usearch FFI, compaction |
| `ingestion` | 60–70% | parsing, chunking, NER pipelines |
| `mcp` | 60–70% | tool handlers, transport, dispatch |
| `config` | 60–70% | presets, ontology, NER config |
| `graph` | 60–70% | knowledge graph, CEL linkers |
| `utils` | 60–70% | shared helpers |
| `llm` | 60–70% | LLM client, retry/backoff |
| `cli` | 50–60% | subcommand dispatch, flags (integration-heavy) |
| `embedding` | 50–60% | ONNX runtime lifecycle (complex FFI) |

## Status: measure-first

- **Current numbers:** [openspec/changes/coverage-rust-workspace/baseline.md](openspec/changes/coverage-rust-workspace/baseline.md)
  (reproduction command, tool version, per-file aggregation, residual-gap analysis).

| Crate | Target | Current (baseline) |
|---|---|---:|
| `db` | 70–80% | 97.98% |
| `graph` | 60–70% | 97.44% |
| `llm` | 60–70% | 97.78% |
| `utils` | 60–70% | 97.79% |
| `ingestion` | 60–70% | 96.11% |
| `search` | 70–80% | 96.43% |
| `mcp` | 60–70% | 95.64% |
| `vectors` | 70–80% | 94.96% |
| `config` | 60–70% | 94.62% |
| `embedding` | 50–60% | 88.13% |
| `cli` | 50–60% | 76.90% |
| **Product total (11 crates)** | — | **92.98%** |

- **Every crate already meets its target.** The residual gap is concentrated in the
  integration-heavy / FFI crates: `cli` (76.90%, 1,391 missed lines) and `embedding`
  (88.13%, 307 missed lines) — see the baseline for the per-file breakdown.
- **No gates.** This change is measure-first: no `--fail-under-*` flag is used locally
  or in CI. Gating is deferred to **Phase 3**.
- Targets may be **raised** once the per-line gap list is in hand (e.g. `cli`/`embedding`
  toward 80%), before Phase 3 enforcement.
