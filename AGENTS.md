# AGENTS.md — Synopsis (Rust)

A local RAG + knowledge-graph MCP server, one binary for a 16 GB laptop. Toolchain pinned to Rust **1.96.0** (`rust-toolchain.toml`). FTS5 is compiled in-tree via rusqlite (bundled) — there are no CGO flags anywhere, so a build cannot silently lack FTS5 support.

## Execution model (read this first)

- AI writes the code; a human reviews **process and contracts** only, not lines of code.
- The orchestrator runs a *different* agent per task: every agent starts with **no memory of previous sessions**, ~100k-token context budget. Therefore each task in `openspec/changes/<change>/tasks.md` is self-contained — goal, exact file scope, dependencies, acceptance criteria, and reference paths must all fit in the body.
- Task size: final diff **≤ ~500 lines** of new code + tests. If a module is larger, split into subtasks with explicit order — never expand scope silently.
- Parity is checked **by the machine**: differential tests against recorded fixtures; recall@k + p50/p95 gates for ANN. The human reviews contracts and deviations.
- **Verify libraries online:** before choosing any library/framework, check the internet for the most suitable candidates for the task. Verify versions, MSRV, maintenance status, and CVEs against official sources.

## Frozen stack (from `openspec/config.yaml`)

- Rust + tokio (async runtime) + axum (HTTP/SSE for non-MCP endpoints such as `/health`)
- rusqlite (`bundled` + `fts5`) — FTS5 always available, no cgo flags; sync driver behind `spawn_blocking`/connection pool. Migrations via `rusqlite_migration` 2.x (`from-directory`) with `PRAGMA user_version` as sole schema-state authority (design D6)
- ONNX runtime as external `.so`/`.dylib` (bge-m3 int8, 1024-dim embeddings + NER); the download/verify mechanism per `onnx.yaml`. **Bindings crate deferred:** the frozen-stack entry "onnxruntime-rs" no longer exists on crates.io and its successor `ort` has no stable release yet — decided in the embedding change
- usearch 2.x — the sole ANN engine (ADR 0004): C++11 HNSW core via cxx, disk-backed, scalar-quantized (default bf16), WAL + segments; replaces vec0 brute-force. Vectors are NOT read from old vec0: they are rebuilt from chunk text
- rmcp 3.x — official MCP SDK over Streamable HTTP (design D8); the legacy HTTP+SSE transport is **also** served (double transport — override of D8, human decision 2026-08-31, change `add-legacy-sse-transport`; wire contract mcp-go v0.57.0)
- cel-interpreter (entity-linking expressions), tokenizers, notify, tokio-cron-scheduler, indicatif

## Hard constraints

- Laptop target (16 GB RAM), personal use — NOT a server. At N=1M × 1024-dim full-precision vectors (~4 GB) do not fit in RAM → the ANN index must be disk-backed/mmap + quantized; the query path does **not** load the embedding model.
- One local binary, no external services.
- The v5 schema shape remains the **structural contract**, but Rust always builds its own DB from scratch: one squashed init migration via `rusqlite_migration` (from-directory, compile-time embedded), `PRAGMA user_version` is the single source of truth for schema state — the `_schema_migrations` table is deliberately NOT created. Legacy `knowledge.db` files are NOT opened or upgraded, and data is not migrated (human decisions 2026-08-18; see native-seam-spikes design D6 + task 1.1 revisions). Future migrations are added as numbered directories `<id>-<slug>/up.sql`, shipped ones are never edited, forward-only (no down.sql).

## Layout

| Crate | Role |
|---|---|
| `crates/config` | YAML presets, onnx.yaml download config, XML ontologies |
| `crates/db` | rusqlite connections, migrations, DAOs, FTS5 queries |
| `crates/vectors` | ANN index contract (trait) + engine; replaces vec0 brute-force (design D5) |
| `crates/embedding` | ONNX runtime lifecycle + bge-m3 int8 provider |
| `crates/ingestion` | document parsers, chunkers, NER, entity extraction |
| `crates/graph` | knowledge graph storage + CEL linkers |
| `crates/search` | hybrid (FTS5 + vector) search with RRF fusion |
| `crates/mcp` | MCP server over Streamable HTTP + 12 tool handlers |
| `crates/cli` | binary: subcommand dispatch, flags, config resolution |

Dependency graph (fixed by design D1): `config, db, vectors → embedding, ingestion, graph → search → mcp → cli`. Also: `openspec/` holds the spec-driven workflow artifacts; `.opencode/skills/` holds agent skills (`openspec-*`, `rust-best-practices`).

## Commands (repo root)

| Command | What it does |
|---|---|
| `cargo fmt --check` | formatting gate — must be clean |
| `cargo clippy --all-targets -- -D warnings` | lint gate — any warning fails the build |
| `cargo test` | full workspace suite, ~seconds; no services or network needed |
| `cargo build --release` | → `target/release/synopsis` (stub prints its version) |
| `cargo zigbuild --release --target <t>` | cross-compile one of the 5 CI targets (needs Zig 0.16.0 + `cargo install --locked cargo-zigbuild`) |

- Single crate: `cargo test -p <crate>` · single test: `cargo test -p <crate> -- <TestNameFilter>`
- Cross-build targets (the CI cross-build matrix): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.

## Gotchas

- **Toolchain pin:** `rust-toolchain.toml` pins 1.96.0, and the CI action `dtolnay/rust-toolchain@<rev>` must match that file exactly — a mismatch makes every cargo command fail with "toolchain not installed".
- **Cargo.lock is committed on purpose:** this workspace produces a binary, and reproducible builds matter for this binary. Do not remove it or add `lock = false`.
- **Workspace lints (task 1.1):** `missing_docs = "deny"` — public API must be documented; `unsafe_code = "forbid"`; clippy `all`/`unwrap_used`/`expect_used` are warn-level, but the gate runs with `-D warnings`, so any warning fails CI. Test modules may opt out locally: `#![allow(clippy::unwrap_used)]`.
- **MCP transport (design D8):** the transport is now **double** — Streamable HTTP via rmcp **and** the legacy HTTP+SSE (`GET /sse` + `POST /message?sessionId=`), re-added by human decision 2026-08-31 (change `add-legacy-sse-transport`; wire contract mcp-go v0.57.0) after D8 originally dropped it. Parity lives at the level of tool responses, compared against recorded fixtures.
- **Cross-builds:** windows-gnu instead of msvc (Zig cannot link MSVC ABI from a Linux host — cargo-zigbuild's own CI uses the same path). The x86_64 musl artifact is fully static and smoke-tested in an Alpine container in CI (`./synopsis --version`).
- **No `make`:** there is no Makefile/CGO machinery here; plain cargo commands above are the whole build system.

## OpenSpec workflow

proposal → design → specs → tasks → apply → archive. Rules live in `openspec/config.yaml`; active changes under `openspec/changes/<name>/` (each with `proposal.md`, `design.md`, `specs/`, `tasks.md`); synced contract specs under `openspec/specs/`. Agent skills: `.opencode/skills/openspec-*` (`propose`, `apply-change`, `sync-specs`, `archive-change`, `update-change`, `explore`).

Frozen contracts — MCP tools, CLI surface, data schema, config formats — live in `openspec/changes/scaffold-rust-project/specs/{mcp-contract,cli-surface,data-schema,config-format}` and stay the reference until synced to main specs. Changing a frozen contract is a **separate explicit decision** with justification; it never happens inside an implementation task.
