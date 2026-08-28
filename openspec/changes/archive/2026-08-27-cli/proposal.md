# Proposal: cli

## Change name
`cli`

## Why

All product crates are implemented and pass gates (config, db, vectors, embedding,
ingestion, graph, search, mcp). The binary is still a 6-line stub that only prints
the version. This change assembles the real `synopsis` binary: subcommand dispatch,
global flags, config resolution, and the full serve pipeline (initial sync + MCP over
Streamable HTTP + file watching + scheduler + health check + cache DB), plus the
`sync`, `model`, `onnx-runtime`, and `load-test` subcommands.

The frozen contract (`openspec/specs/cli-surface/spec.md`, transcribed from
`../synopsis/cmd/app/*`) defines the subcommands, global flags, config-resolution
order, per-command flags, and the load-test report format. This change implements
that contract; it does NOT change it.

## What changes

1. **CLI skeleton** — clap 4.6.6 (Builder API) with global flags
   (`--config`, `--preset`, `--db`, `--version`) before the subcommand, dispatch to
   five subcommands, config-resolution logic (exeDir → CWD, `--config` >
   `config.{preset}.yaml`), and `--version`.
2. **serve orchestration** — bootstrap (config load/validate, domain discovery, ONNX
   config, `--db` override, DB open + migrations, dimension-mismatch handling,
   embedding-model assurance, cache DB, startup health check) → initial sync →
   knowledge-graph load → `HybridSearcher` → `mcp::Server::router()` on the port →
   file watcher (notify) → scheduler (orphan_cleanup) → graceful shutdown
   (SIGINT/SIGTERM, 10s timeout).
3. **sync** — one-shot `ingest_all(rebuild)` + stderr summary block.
4. **model** — list/download/delete/info/benchmark via `embedding::ModelManager` +
   `benchmark_model`.
5. **onnx-runtime** — install/status/uninstall via `embedding::LibraryManager`.
6. **load-test** — deterministic data generation + fill (real embeddings) + graph
   load + `mcp::Server::dispatch` over all 12 tools (direct, no HTTP) + Report
   (CALLS/AVG/P50/P95/P99/MAX ms/QPS, `--json`).

## Non-goals

- No legacy SSE transport (frozen D8) — MCP is rmcp 3.x Streamable HTTP.
- No auth/middleware beyond what the oracle has.
- No new product crates: watcher / scheduler / benchmark are `cli`-internal modules
  (KISS — serve-only concerns, no other consumer).
- No dependency on `parity-harness` (outside the product dependency graph).

## Deviations from the frozen contract (explicit, user-approved 2026-08-27)

- **Help/usage text is NOT byte-matched to Go.** The cli-surface spec's machine-diff
  parity requirement for `model --help` / `onnx-runtime --help` is relaxed: clap
  produces Rust-idiomatic help; only the *set of flags, their defaults, and
  behavior* stay faithful to the contract. Rationale: functional-copy principle —
  "Дословно копировать Go не нужно… цель сделать копию приложения по функционалу, а
  не копию по кода" (user directive, repeated).
- **Stack extension (user-approved 2026-08-27):** `clap`, `tracing`,
  `tracing-subscriber` are added to the workspace. They are not in the original
  frozen stack list but are required for (a) CLI parsing with controllable help and
  (b) structured logging in serve/sync/load-test. Both are pure-Rust, ubiquitous,
  no CGO. `notify 8`, `tokio-cron-scheduler 0.15`, `indicatif 0.18` are already
  declared in the workspace.

## Risks

- `RunnerParams` assembly (≈11 fields) is the hardest wiring — mitigated by helper
  functions + unit tests per sub-assembly.
- Graceful-shutdown coordination across scheduler + watcher + axum — mitigated by a
  shutdown channel; each component has its own stop path.
- Dimension-mismatch detection relies on `rusqlite_migration` error propagation —
  verified against `db` crate error types.
- Online CVE verification for clap/tracing was blocked (search provider 403); both
  are the most-maintained crates in the ecosystem with no known core CVEs. Marked as
  *not performed*; the human reviewer should confirm before the binary ships.
