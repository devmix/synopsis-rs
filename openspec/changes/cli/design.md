# Design: cli

Oracle references (read-only): `../synopsis/cmd/app/{main,cmd,serve,sync,
model_cmd,onnx_runtime,loadtest}.go` (+ `main_test.go`,
`resolve_config_path_test.go`). Frozen contract:
`openspec/specs/cli-surface/spec.md` — implemented, not amended (help-text parity
relaxed per user decision 2026-08-27, see proposal.md Deviations).

## D1 — CLI parsing (clap 4.6.6, Builder API)

`clap` 4.6.6 Builder API (not derive) for full control over flag/help structure.
Global flags (`--config`, `--preset`, `--db`, `--version`) parsed before the
subcommand; per-command flags follow. Help text is Rust-idiomatic — NOT byte-matched
to Go (user decision). Flags, defaults, and behavior stay faithful to the frozen
cli-surface spec. `clap` is added to the workspace (stack extension, user-approved).

Config resolution (port of `resolveConfigPath`/`resolveConfigCandidates`):
`--config` wins outright; otherwise search `config.{preset}.yaml` candidates in
order: `<exeDir>/configs`, `<exeDir>`, `<parent(exeDir)>`, `<parent(exeDir)>/configs`,
then CWD `/configs`, CWD; ultimate fallback `configs/config.{preset}.yaml` (let
config.Load surface the error). Use `std::env::current_exe()` (no symlink resolution
needed — Rust binaries are not symlinked in this deployment).

## D2 — Logging (tracing + tracing-subscriber, cli only)

`tracing` + `tracing-subscriber` (features `json`, `env-filter`) initialized once in
`main` from `config.Logging.Level` (default `info`). Library crates stay logger-less
(their errors surface as `Result`s). `RUST_LOG` env filter also honored. Plain
`eprintln!` only for fatal pre-logging errors. Stack extension, user-approved.

## D3 — Bootstrap assembly

`bootstrap(cfg_path, db_path) -> (Config, DomainRegistry, ...)`:
1. `Config::load` → `apply_defaults` → `validate`.
2. Domain discovery: `config::domain` registry from `global.xml` + per-domain
   `domains/*.xml` (port of `domain.DiscoveryWithLogger`).
3. `config::load_onnx_config(paths.onnx_config_path)`.
4. `--db` overrides `config.database.path`.
5. `Db::open(db_path)` + `run_migrations` (PRAGMA user_version is sole schema
   authority). Dimension mismatch: `rusqlite_migration` returns an error that is
   **not fatal** here — surfaced to the caller so serve/sync decide auto-rebuild vs
   exit.
6. `embedding::new_onnx_provider(cfg, data_dir, onnx_cfg)` (local mode auto-downloads
   the model via `ModelManager::ensure_model`; explicit `ModelPath` skips).
7. Cache DB: `Db::open(cache_path)`; on `Err` → log warning, continue with
   `llm_cache = None` (port of `openCacheStore` nil-on-failure).
8. `RunnerParams` assembly (≈11 fields): `db`, `ingest_cfg`, `global`, `domains`,
   `registry`, `embed`, `vectors`, `prompts`, `linker_cfg`, `prompts_path`,
   `llm_cache`. Read `ingestion::runner::RunnerParams` for exact field names/types.

## D4 — serve orchestration

`run_serve`: bootstrap → port override (`--port` > `server.port`) →
`auto_rebuild_vectors = cli_flag || config.embeddings.auto_rebuild_vectors` →
dimension-mismatch handling (auto-rebuild via `Runner::ingest_all(true)` or fatal) →
startup health check (db ping, doc count, embedding-provider probe; log-only) →
`Runner::new` → initial sync if `config.autoupdate.initial_sync && !--no-initial-sync`
→ graph load if `config.graph.enable_graph && config.graph.load_on_startup` →
`HybridSearcher::new(...)` → `mcp::Server::new(...).router()` mounted on the port →
file watcher (D5) if `autoupdate.enabled && watch_sources` → scheduler (D6) if
orphan_cleanup enabled → graceful shutdown (D7).

## D5 — File watcher (notify + debounce)

`notify::PollWatcher` with `debounce` from `config.autoupdate.debounce_seconds`.
Callback: dedupe changed paths by source → `Runner::ingest_source_by_path` for each
affected source → `Runner::prune_deleted` → if `enable_graph`, reload graph and call
`SetGraph` on the searcher + `mcp::Server`. Watch all enabled, non-disabled sources
from the global config. (Port of `setupFileWatcher`.)

## D6 — Scheduler (tokio-cron-scheduler)

`JobScheduler`; register `orphan_cleanup` job iff
`config.scheduler.jobs["orphan_cleanup"].enabled`, interval from
`config.scheduler.jobs["orphan_cleanup"].interval_seconds`; job body calls
`Runner::cleanup_orphaned_data`. Start after initial sync; stop on shutdown.

## D7 — Graceful shutdown

`tokio::signal::unix` SIGINT + SIGTERM (and `ctrl_c` fallback) → send on a
`mpsc`/watch channel → `axum::serve(listener, router).with_graceful_shutdown(async {
shutdown_rx.await })`; then `scheduler.shutdown()` and `watcher.stop()`. 10s timeout
via `tokio::time::timeout` around the wait. (Port of the Go select + 10s
`shutdownCtx`.)

## D8 — sync subcommand

`run_sync`: bootstrap → dimension-mismatch handling → `Runner::new` →
`ingest_all(rebuild)` → stderr summary block (sources processed, documents
created/updated/skipped, errors, duration). Exit 0 on success, non-zero on error.

## D9 — model subcommand

`model list|download|delete|info|benchmark` over `embedding::ModelManager`
(`list_models`, `download_model`, `delete_model`, `get_model_path`, `registry`) and
`embedding::benchmark_model` (port of `model_cmd.go`). `indicatif` progress where the
oracle shows progress.

## D10 — onnx-runtime subcommand

`onnx-runtime install|status|uninstall` over `embedding::LibraryManager`
(`ensure_library`, `get_library_path`, `uninstall`, `get_version`, `get_cache_dir`)
+ platforms table from onnx config (port of `onnx_runtime.go`).

## D11 — load-test subcommand

`load-test --scale small|medium|large --seed 42 --iterations 100 --json PATH
--no-fill`. Dimension mismatch under `--no-fill` is fatal; otherwise drop+recreate
the vector table. `require_embedding_model` (never auto-download — fail loudly).
Deterministic `Generator(seed)` → `Fill` (real embeddings, progress logs) → graph
load + timing → `HybridSearcher` → `benchmark::Runner` calling
`mcp::Server::dispatch(name, args)` **directly** (already `pub` at
`crates/mcp/src/server.rs:113`, no HTTP, no mcp change) for all 12 tools →
`Report` (CALLS/AVG/P50/P95/P99/MAX ms/QPS) to stdout, `--json` to file. Benchmark
module lives in `cli/src/loadtest/*` (NOT parity-harness).

## D12 — Testing strategy

- Unit: config resolution order; bootstrap error paths (missing model, dimension
  mismatch, cache-DB failure → continue); watcher debounce; scheduler registration;
  load-test generator determinism + report shape.
- Integration: build the binary; `synopsis --version` → exit 0; `synopsis serve
  --port N` → `GET /health` 200; `synopsis sync --rebuild` runs; `synopsis model
  list` / `onnx-runtime status` / `load-test --scale small` execute (model download
  may be skipped in CI via a fixture or `--no-fill` against a seeded DB).
- Gates per task: `cargo fmt --check`, `cargo clippy -p cli --all-targets -- -D
  warnings`, `cargo test -p cli`. Workspace gates run by the orchestrator only.
