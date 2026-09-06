# AGENTS.md — Synopsis (Rust)

A local RAG + knowledge-graph MCP server, one binary for a 16 GB laptop. Toolchain pinned to Rust **1.96.0** (`rust-toolchain.toml`). FTS5 is compiled in-tree via rusqlite (bundled) — there are no CGO flags anywhere, so a build cannot silently lack FTS5 support.

## Execution model (read this first)

- AI writes the code; a human reviews **process and contracts** only, not lines of code.
- The orchestrator runs a *different* agent per task (definitions in `.opencode/agents/`): every agent starts with **no memory of previous sessions**, ~100k-token context budget. Each task in `openspec/changes/<change>/tasks.md` is therefore self-contained — goal, exact file scope, dependencies, acceptance criteria, and reference paths must all fit in the body.
- Task size: final diff **≤ ~500 lines** of new code + tests. If a module is larger, split into subtasks with explicit order — never expand scope silently.
- Parity is checked **by the machine**: differential tests against recorded fixtures; recall@k + p50/p95 gates for ANN. The human reviews contracts and deviations.
- **Verify libraries online:** before choosing any library/framework, check the internet for the most suitable candidates for the task. Verify versions, MSRV, maintenance status, and CVEs against official sources.

## Frozen stack (from `openspec/config.yaml`)

- Rust + tokio (async runtime) + axum (HTTP for non-MCP endpoints such as `/health`)
- rusqlite 0.40 — bundled SQLite via a **direct** `libsqlite3-sys = { features = ["bundled"] }` workspace dependency (cargo forbids enabling it transitively through rusqlite); FTS5 ships in every bundled build (there is no separate `fts5` feature anymore). Sync driver behind `spawn_blocking` + r2d2 pool. Migrations: `rusqlite_migration` 2.x (`from-directory`) with `PRAGMA user_version` as sole schema-state authority (design D6)
- ONNX runtime as external `.so`/`.dylib` (bge-m3 int8, 1024-dim embeddings only); downloaded/verified per `onnx.yaml`. Bindings: `ort` **2.0.0-rc.13** (`load-dynamic` only, default features off) — a deliberate pre-release pin (ADR 0002); all API churn is isolated in `crates/embedding`
- usearch 2.x — the sole ANN engine (ADR 0004): C++11 HNSW core via cxx, disk-backed, scalar-quantized (default bf16), WAL + segments. Vectors are NOT read from old vec0: they are rebuilt from chunk text
- rmcp 3.x — MCP protocol (Streamable HTTP, design D8); the legacy HTTP+SSE transport is **also** served (double transport — D8 override, human decision 2026-08-31, wire contract mcp-go v0.57.0)
- `cel` 0.14 (entity-linking expressions — supersedes cel-interpreter, 2026-08-22), tokenizers, notify, minijinja (prompt templates), petgraph (graph index), noyalib (YAML), jiff (UTC dates), clap 4.6.6 (CLI), tracing (logging in `cli` only — library crates stay logger-less)

## Hard constraints

- Laptop target (16 GB RAM), personal use — NOT a server. At N=1M × 1024-dim full-precision vectors (~4 GB) do not fit in RAM → the ANN index must be disk-backed/mmap + quantized; the query path does **not** load the embedding model.
- One local binary, no external services — except the user-configured OpenAI-compatible LLM endpoint that NER + cross-domain entity linking call (`crates/llm`). The test suite needs no network.
- Rust always builds its own DB from scratch: one squashed init migration, `PRAGMA user_version` is the single source of schema truth — the `_schema_migrations` table is deliberately NOT created. Existing `knowledge.db` files are NOT opened or upgraded, and data is not migrated. Future migrations: numbered directories `<id>-<slug>/up.sql`, shipped ones never edited, forward-only (no down.sql).

## Layout

| Crate | Role |
|---|---|
| `crates/config` | YAML presets, onnx.yaml download config, XML ontologies |
| `crates/db` | rusqlite connections, migrations, DAOs, FTS5 queries |
| `crates/vectors` | ANN index contract (trait) + usearch engine; WAL + compaction |
| `crates/embedding` | ONNX runtime lifecycle + bge-m3 int8 provider |
| `crates/ingestion` | document parsers, chunkers, NER, entity extraction, event task queue + worker |
| `crates/graph` | knowledge graph (petgraph) + CEL linkers + minijinja prompts |
| `crates/search` | hybrid (FTS5 + vector) search with RRF fusion |
| `crates/mcp` | MCP server (double transport) + 12 tool handlers |
| `crates/llm` | OpenAI-compatible LLM client (NER + entity linking); base tier, depends only on `config` |
| `crates/utils` | shared helpers (temporal: RFC 3339 UTC); leaf crate any crate may depend on |
| `crates/cli` | `synopsis` binary: subcommand dispatch, flags, config resolution |

Dependency graph (fixed by design D1): `config, db, vectors, utils, llm → embedding, ingestion, graph → search → mcp → cli`. Also: `openspec/` holds the spec-driven workflow artifacts; `docs/adr/` holds ADRs 0001–0004; `.opencode/` holds agent skills (`openspec-*`, `rust-best-practices`), `opsx-*` commands, and subagent definitions; `migrations/{knowledge,cache}/` holds the SQL migrations (see Gotchas).

## Commands (repo root)

| Command | What it does |
|---|---|
| `cargo fmt --check` | formatting gate — must be clean |
| `cargo clippy --all-targets -- -D warnings` | lint gate — any warning fails |
| `cargo test` | full workspace suite, ~20 s warm; no services or network needed |
| `cargo build --release` | → `target/release/synopsis` (the real server binary) |
| `cargo zigbuild --release --target <t>` | cross-compile one of the 5 CI targets (needs Zig 0.16.0 + `cargo install --locked cargo-zigbuild`) |
| `cargo llvm-cov --workspace --html` | coverage report — measure-first, **no gates** (see `COVERAGE.md`) |

- Single crate: `cargo test -p <crate>` · single test: `cargo test -p <crate> -- <TestNameFilter>`
- Run the server: `cargo run -p cli -- serve` (config auto-search: `<exeDir>/workspace/configs/` → parent dir → CWD; default preset `default`, port 8080). Subcommands: `serve`, `queue status|reset-retries`, `db stats|clear`, `model list|download|delete|info|benchmark`, `onnx-runtime install|status|uninstall`, `load-test`.
- Cross-build targets (the CI cross-build matrix): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.

## Gotchas

- **Toolchain pin:** `rust-toolchain.toml` pins 1.96.0, and the CI action `dtolnay/rust-toolchain@<rev>` must match that file exactly — a mismatch makes every cargo command fail with "toolchain not installed".
- **Cargo.lock is committed on purpose:** this workspace produces a binary, and reproducible builds matter for parity testing. Do not remove it.
- **Workspace lints:** `missing_docs = "deny"` — public API must be documented; `unsafe_code = "forbid"`; clippy `all`/`unwrap_used`/`expect_used` are warn-level, but the gate runs with `-D warnings`, so any warning fails CI. Test modules may opt out locally: `#![allow(clippy::unwrap_used)]`.
- **MCP transport is double:** rmcp Streamable HTTP serves as the **fallback for every path** (clients connect e.g. to `/mcp`), and the legacy HTTP+SSE pair (`GET /sse` + `POST /message?sessionId=`) is mounted explicitly, plus `GET /health`. Parity lives at the level of tool responses, compared against recorded fixtures.
- **Migrations live at the repo root** in `migrations/{knowledge,cache}/` and are embedded at compile time via `include_dir!` in `crates/db` — a new migration goes there, not in the crate. There are two DBs: the per-dataset knowledge DB and the global cache DB (`workspace/db/cache/cache.db`).
- **`workspace/` is runtime state, mostly gitignored:** ONNX runtime `.so` (~24 MB) under `workspace/onnxruntime/`, model weights (~2.3 GB) under `workspace/models/`, per-dataset state (knowledge.db + vectors) under `workspace/datasets/<name>/state/`. Only `workspace/configs/` and the `edtech` demo dataset (ontology + content) are tracked. The runtime and models are downloaded by the binary per `onnx.yaml` (`onnx-runtime install` / `model download`).
- **Shipped config presets deliberately deviate from the frozen default:** the local presets use `bge-small-en-v1.5` (384-dim) instead of the frozen `bge-m3-int8` (1024-dim) — documented in `workspace/configs/README.md`; do not "fix" it.
- **Prompt templates are minijinja:** `workspace/configs/prompts/**` are byte-identical to the embedded defaults in `crates/graph`/`crates/ingestion`; a missing override file silently falls back to the embedded default.
- **CI is Gitea-compatible on purpose:** the Gitea runner has no artifact service, so `release.yml` is a single job (5-target zigbuild loop → package → Gitea Release) and both workflows use Gitea-fork actions (`ChristopherHX/gitea-upload-artifact`, `akkuman/gitea-release-action`). Do not "upgrade" them to the standard GitHub actions. Dev CI (`ci.yml`) stays fast: fmt + clippy + test only; cross-builds run only on `v*` tags.
- **`release.yml` copies the root `README.md` into every release archive** — the README doubles as end-user documentation shipped with the binary; keep it in sync with the CLI surface and config layout.
- **Cross-builds:** windows-gnu instead of msvc (Zig cannot link MSVC ABI from a Linux host). The x86_64 musl artifact is fully static.
- **No `make`:** plain cargo commands above are the whole build system.
- **`.opencode/opencode.json`** wires an OpenCode session to this project's own running server (`http://localhost:8080/sse`) as the `synopsis` MCP — that is the legacy SSE transport of the binary under development.

## OpenSpec workflow

proposal → design → specs → tasks → apply → archive. Rules live in `openspec/config.yaml`; main contract specs are synced under `openspec/specs/` (`mcp-contract`, `cli-surface`, `data-schema`, `config-format`, plus per-module specs); completed changes are archived under `openspec/changes/archive/YYYY-MM-DD-<name>/`. Agent skills: `.opencode/skills/openspec-*`; commands: `opsx-propose`, `opsx-apply`, `opsx-sync`, `opsx-archive`, `opsx-update`, `opsx-explore`.

Changing a frozen contract (MCP tools, CLI surface, data schema, config formats) is a **separate explicit decision** with justification; it never happens inside an implementation task.
