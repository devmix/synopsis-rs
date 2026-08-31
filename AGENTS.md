# AGENTS.md — Synopsis (Rust)

Rust rewrite of the Go service "Synopsis" (`../synopsis`): a local RAG + knowledge-graph MCP server, one binary for a 16 GB laptop. Toolchain pinned to Rust **1.96.0** (`rust-toolchain.toml`). FTS5 is compiled in-tree via rusqlite (bundled) — there are no CGO flags anywhere; that whole class of silent-degradation bugs from the oracle does not exist by construction here.

## Execution model (read this first)

- AI writes the code; a human reviews **process and contracts** only, not lines of code.
- The orchestrator runs a *different* agent per task: every agent starts with **no memory of previous sessions**, ~100k-token context budget. Therefore each task in `openspec/changes/<change>/tasks.md` is self-contained — goal, exact file scope, dependencies, acceptance criteria, and oracle reference paths must all fit in the body.
- Task size: final diff **≤ ~500 lines** of new code + tests. If a module is larger, split into subtasks with explicit order — never expand scope silently.
- Parity is checked **by the machine**: differential tests against fixtures recorded once from the Go binary; recall@k + p50/p95 gates for ANN. The human reviews contracts and deviations.

## Oracle

`../synopsis` (Go) is the read-only reference implementation for the whole migration: its behavior, tests, and contracts are the source of truth. Never create/modify/delete anything there. Contract sources: `internal/mcp/tools.go`, `cmd/app/*`, `migrations/*.sql`, `configs/*.yaml`, `data/ontology/*.xml`.

## Migration principles (human decisions 2026-08-18)

- **No 1:1 copying.** The oracle is a reference for *behavior and contracts*, not a blueprint to transcribe. Every ported piece must be re-designed architecturally for Rust: do not repeat the original's mistakes; if the Go code is wrong or Rust allows a more optimal/efficient solution, do it — **even at the cost of losing compatibility** with the oracle (contract changes still require an explicit decision per openspec/config.yaml rules).
- **Verify libraries online.** Before choosing any library/framework, check the internet for the most suitable candidates for the task (e.g., Rust has different template engines, HTTP clients, serialization crates than Go). Do not mechanically carry over the Go stack. Verify versions, MSRV, maintenance status, and CVEs against official sources.

## Frozen stack (from `openspec/config.yaml`)

- Rust + tokio (async runtime) + axum (HTTP/SSE for non-MCP endpoints such as `/health`)
- rusqlite (`bundled` + `fts5`) — FTS5 always available, no cgo flags; sync driver behind `spawn_blocking`/connection pool. Migrations via `rusqlite_migration` 2.x (`from-directory`) with `PRAGMA user_version` as sole schema-state authority (design D6)
- ONNX runtime as external `.so`/`.dylib` (bge-m3 int8, 1024-dim embeddings + NER); the download/verify mechanism per `onnx.yaml` is ported from the oracle. **Bindings crate deferred:** the frozen-stack entry "onnxruntime-rs" no longer exists on crates.io and its successor `ort` has no stable release yet — decided in the embedding change
- usearch 2.x — the sole ANN engine (ADR 0004): C++11 HNSW core via cxx, disk-backed, scalar-quantized (default bf16), WAL + segments; replaces vec0 brute-force. Vectors are NOT read from old vec0: they are rebuilt from chunk text
- rmcp 3.x — official MCP SDK over Streamable HTTP (design D8); wire compatibility with the oracle's legacy SSE transport is **deliberately not preserved**
- cel-interpreter (entity-linking expressions), tokenizers, notify, tokio-cron-scheduler, indicatif

## Hard constraints

- Laptop target (16 GB RAM), personal use — NOT a server. At N=1M × 1024-dim full-precision vectors (~4 GB) do not fit in RAM → the ANN index must be disk-backed/mmap + quantized; the query path does **not** load the embedding model.
- One local binary, no external services.
- The schema from the Go original's 5 migrations remains the **structural contract** (final v5 shape), but Rust always builds its own DB from scratch: one squashed init migration via `rusqlite_migration` (from-directory, compile-time embedded), `PRAGMA user_version` is the single source of truth for schema state — the `_schema_migrations` table is deliberately NOT created. Legacy Go-created `knowledge.db` is NOT opened, NOT upgraded, and data is NOT migrated (human decisions 2026-08-18; see native-seam-spikes design D6 + task 1.1 revisions). Future migrations are added as numbered directories `<id>-<slug>/up.sql`, shipped ones are never edited, forward-only (no down.sql).

## Layout

| Crate | Role | Oracle mapping |
|---|---|---|
| `crates/config` | YAML presets, onnx.yaml download config, XML ontologies | `internal/config` |
| `crates/db` | rusqlite connections, migrations, DAOs, FTS5 queries | `internal/database` |
| `crates/vectors` | ANN index contract (trait) + engine; replaces vec0 brute-force | none — new crate (design D5) |
| `crates/embedding` | ONNX runtime lifecycle + bge-m3 int8 provider | `internal/onnx` + `internal/embedding` |
| `crates/ingestion` | document parsers, chunkers, NER, entity extraction | `internal/ingestion` |
| `crates/graph` | knowledge graph storage + CEL linkers | `internal/graph` + `internal/relations` |
| `crates/search` | hybrid (FTS5 + vector) search with RRF fusion | `internal/search` |
| `crates/mcp` | MCP server over Streamable HTTP + 12 tool handlers | `internal/mcp` (+handlers) |
| `crates/cli` | binary: subcommand dispatch, flags, config resolution | `cmd/app` |
| `crates/parity-harness` | dev-tooling: rmcp client with p50/p95 timing, fixture loader, diff utilities | none — new crate (design D6) |

Dependency graph (fixed by design D1): `config, db, vectors → embedding, ingestion, graph → search → mcp → cli`; `parity-harness` is outside the product dependency graph. Also: `openspec/` holds the spec-driven workflow artifacts; `.opencode/skills/` holds agent skills (`openspec-*`, `rust-best-practices`).

## Commands (repo root)

| Command | What it does |
|---|---|
| `cargo fmt --check` | formatting gate — must be clean |
| `cargo clippy --all-targets -- -D warnings` | lint gate — any warning fails the build |
| `cargo test` | full workspace suite, ~seconds; no services or network needed |
| `cargo build --release` | → `target/release/synopsis` (stub prints its version) |
| `cargo zigbuild --release --target <t>` | cross-compile one of the 5 CI targets (needs Zig 0.16.0 + `cargo install --locked cargo-zigbuild`) |
| `cargo test -p parity-harness` | parity harness: percentile unit tests + in-process MCP round-trip integration test |

- Single crate: `cargo test -p <crate>` · single test: `cargo test -p <crate> -- <TestNameFilter>`
- Cross-build targets (CI matrix, mirrors the oracle's build-all platforms): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.

## Gotchas

- **Toolchain pin:** `rust-toolchain.toml` pins 1.96.0, and the CI action `dtolnay/rust-toolchain@<rev>` must match that file exactly — a mismatch makes every cargo command fail with "toolchain not installed".
- **Cargo.lock is committed on purpose:** this workspace produces a binary, and reproducible builds matter for parity testing against the Go oracle. Do not remove it or add `lock = false`.
- **Workspace lints (task 1.1):** `missing_docs = "deny"` — public API must be documented; `unsafe_code = "forbid"`; clippy `all`/`unwrap_used`/`expect_used` are warn-level, but the gate runs with `-D warnings`, so any warning fails CI. Test modules may opt out locally: `#![allow(clippy::unwrap_used)]`.
- **MCP transport (design D8):** Streamable HTTP via rmcp — do NOT implement the oracle's legacy SSE (`GET /sse` + `POST /message?sessionId=`); it is deprecated and was intentionally dropped by human decision 2026-08-18. Parity lives at the level of tool responses, compared against fixtures recorded once from the Go binary.
- **vectors.bin fixture format:** stubbed with a TODO in `parity-harness`; the format is fixed by change `native-seam-spikes`. Do not invent one before that change lands.
- **Cross-builds:** windows-gnu instead of msvc (Zig cannot link MSVC ABI from a Linux host — cargo-zigbuild's own CI uses the same path). The x86_64 musl artifact is fully static and smoke-tested in an Alpine container in CI (`./synopsis --version`).
- **No `make`:** unlike the oracle, there is no Makefile/CGO machinery here; plain cargo commands above are the whole build system.

## OpenSpec workflow

proposal → design → specs → tasks → apply → archive. Rules live in `openspec/config.yaml`; active changes under `openspec/changes/<name>/` (each with `proposal.md`, `design.md`, `specs/`, `tasks.md`); synced contract specs under `openspec/specs/`. Agent skills: `.opencode/skills/openspec-*` (`propose`, `apply-change`, `sync-specs`, `archive-change`, `update-change`, `explore`).

Frozen contracts — MCP tools, CLI surface, data schema, config formats — are transcribed from the Go oracle (currently in `openspec/changes/scaffold-rust-project/specs/{mcp-contract,cli-surface,data-schema,config-format}`) and stay the reference until synced to main specs. Changing a frozen contract is a **separate explicit decision** with justification; it never happens inside an implementation task.
