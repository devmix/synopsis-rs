# Tasks: document-jobs-queue

- [x] 1.1 Add `document_jobs` table (knowledge DB migration) + `DocumentJobDao` + state-machine helpers (commit 37e80a9)
- [x] 1.2 Config: `ingestion.max_retries` (default 3) + `auto_update.retry_failed` (commit d4ecdd0)
- [x] 1.3 Producer API: `enqueue_index` / `enqueue_delete` / `reconcile_source` (commit 3382bf1, reworked: producer no longer parses content / builds Document)
- [x] 1.4 Background worker: poll due jobs, sequential pipeline, retry + backoff, error status, GC phase (commit 35cfcbc)
- [x] 1.5 Watcher → producer; startup reconcile replaces initial sync (commit 8d6d054)
- [x] 1.6 Owner-loop wiring: spawn worker, `RetryBatch` arm, remove standalone orphan-cleanup scheduler (commit a5ba9fe)
- [x] 1.7 CLI `queue status` / `queue reset-retries` (renamed from `index` by human decision 2026-08-29) (commit 6e0b7d0)
- [x] 1.8 Tests: DAO, state machine, worker + retry + GC, watcher-producer, CLI (commit 13d32c3)
- [x] 1.9 Remove dead code + test-only scaffolding (commit 20dc6d3)

## 1.1 Add `document_jobs` table + `DocumentJobDao` + state-machine helpers

**Goal:** Persistent state machine for document operations in the knowledge DB.

**Scope файлов:**
- `migrations/knowledge/2-document-jobs/up.sql` (NEW) — `CREATE TABLE IF NOT EXISTS document_jobs (path TEXT PRIMARY KEY, source_path TEXT NOT NULL, op TEXT NOT NULL DEFAULT 'index', status TEXT NOT NULL DEFAULT 'pending', content_hash TEXT, attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER NOT NULL DEFAULT 3, last_error TEXT, next_attempt_at INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')), updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))); CREATE INDEX IF NOT EXISTS idx_document_jobs_due ON document_jobs(status, next_attempt_at);`
- `crates/db/src/document_job.rs` (NEW) — `DocumentJob` struct + `DocumentJobDao` with: `enqueue_index(conn, path, source, hash)`, `enqueue_delete(conn, path)`, `claim_due(conn, now, batch) -> Vec<DocumentJob>` (atomic `UPDATE ... SET status='processing' WHERE status='pending' AND next_attempt_at<=? ORDER BY next_attempt_at LIMIT ?` then `SELECT`), `mark_done(conn, path)`, `mark_deleted_row(conn, path)`, `record_failure(conn, path, err, now, backoff, max_attempts)` (attempts+1; if < max → pending+next_attempt_at=now+backoff else error), `reset_retries(conn, path)` (error→pending, attempts=0, next_attempt_at=now), `get_by_path`, `list(filter_status, source)`.
- `crates/db/src/lib.rs` — `pub mod document_job;`
- `crates/db/src/error.rs` — if needed, a `DbError` variant for job ops (reuse existing).

**Dependencies:** none.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p db` pass.
- Migration applies on a fresh knowledge DB; `PRAGMA user_version` increments; re-running is a no-op (idempotent `IF NOT EXISTS`).
- DAO unit tests (in `document_job.rs` `#[cfg(test)]`): enqueue is idempotent (same path → one row, attempts reset to 0 on re-enqueue); `claim_due` returns only due `pending` rows and flips them to `processing`; `record_failure` increments attempts and sets `pending`+backoff or `error` at cap; `reset_retries` flips `error`→`pending`/attempts=0; `list` filters by status/source.
- `grep -rn "document_jobs" migrations/knowledge/2-document-jobs/up.sql` present; index `idx_document_jobs_due` exists.

**Oracle reference:** n/a — new Rust operational table; the Go oracle has no equivalent queue (it ingests synchronously). No parity required.

## 1.2 Config: `ingestion.max_retries` + `auto_update.retry_failed`

**Goal:** Add retry configuration, backward compatible.

**Scope файлов:**
- `crates/config/src/preset.rs` — add `pub max_retries: i32` (default 3) to `IngestionConfig` (with `#[serde(default = "default_max_retries")]`); add `pub retry_failed: RetryFailedConfig` to `AutoUpdateConfig` where `RetryFailedConfig { pub enabled: bool, pub poll_interval_seconds: i32 }` with `#[serde(default)]` and `enabled` default `true`, `poll_interval_seconds` default `60`. Apply in `Config::apply_defaults` (presence semantics like `watch_sources`).

**Dependencies:** none.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p config` pass.
- Default `max_retries == 3`; `retry_failed.enabled == true`; `retry_failed.poll_interval_seconds == 60`.
- Existing YAML presets (`config.default.yaml`, `config.demo.yaml`, `tests/data/config.default.yaml`) parse unchanged (new keys optional; serde defaults apply). Add the new keys to the three presets for documentation parity (optional but recommended).
- A config test asserts defaults when the keys are absent.

**Oracle reference:** n/a — retry policy is a new Rust behavior; oracle has no equivalent counter.

## 1.3 Producer API: `enqueue_index` / `enqueue_delete` / `reconcile_source`

**Goal:** Single producer surface that replaces direct `Runner` ingest calls. The producer is the ONLY component that walks source directories; it enumerates file paths + content hashes and writes `document_jobs` rows. It does NOT parse content into `Document` and does NOT hand data to the worker.

**Scope файлов:**
- `crates/ingestion/src/job_queue.rs` (NEW) — `DocumentJobQueue { db: &Db }` with `enqueue_index(path, source_path, content_hash)`, `enqueue_delete(path)`, `reconcile_source(&self, runner, source_path)`. `reconcile_source` obtains the `Source` via `runner.source_for_config(src)`, then enumerates matched file paths with `walk_matched_files(root, |p| source.supported_extensions().iter().any(matches), |p| { read file content; hash = compute_content_hash(&content); enqueue }, &mut errors)` — it must NOT call `source.parse(root)` (that builds `Document`s). Diff enumerated `(path, hash)` against the `documents` table + existing `document_jobs`; enqueue `index` for new/changed hashes, `delete` for paths present in DB/jobs but absent on disk. Idempotent upsert by path.
- `crates/ingestion/src/lib.rs` — `pub mod job_queue;`

**Dependencies:** 1.1, 1.2.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p ingestion` pass.
- Unit tests: `enqueue_index` then `enqueue_index` again → one row, attempts 0; `reconcile_source` on a fixture tree enqueues `index` for new/changed files and `delete` for removed ones; unchanged files produce no job.
- `DocumentJobQueue` uses `DocumentJobDao` from `db`; NO call to `source.parse(...)` / `Runner::process_document*` inside the producer (grep must return 0). The producer builds no `Document`.

**Oracle reference:** `../synopsis/cmd/app/serve.go` `setupFileWatcher` / `internal/ingestion` reconcile logic — behavior reference only; the queue is a new Rust construct.

## 1.4 Background worker: poll due → read ONE file → sequential pipeline → retry/backoff/error → GC

**Goal:** The only consumer. Reads due rows from `document_jobs`, and for each reads the SINGLE referenced file (no directory walk) via `Parser::parse_file`, runs the existing per-document pipeline sequentially, retries with backoff, flips to `error` at cap, runs GC after each cycle. Fully decoupled from the producer (state only via the table).

**Scope файлов:**
- `crates/ingestion/src/types.rs` — add `fn parse_file(&self, path: &Path, root: &Path) -> Result<Document, IngestionError>` to the `Parser` trait (object-safe; mirrors `parse` but reads ONE file). Implement in all five parsers (`markdown` already has `read_file`; `mediawiki`/`webpage`/`unstructured`/`json` gain an equivalent per-file read that builds a `Document` with `content` + `metadata`).
- `crates/ingestion/src/runner/mod.rs` — REMOVE `process_document_at` (whole-tree parse). Add `process_document_by_path(&self, path)` that does `find_source_for_path(path)` → `source_for_config` → `source.parse_file(Path::new(path), Path::new(&src.path))` → `Ingester::process_document(&doc, &mut tracker)` (reuses the existing pipeline). Keep `delete_document_at` + `clear_and_delete_doc` unchanged.
- `crates/ingestion/src/worker.rs` (NEW) — `DocumentWorker { db, runner, cfg }` with `run_once(&self, now)` (claim due batch via `DocumentJobDao::claim_due`, for each: for `index` op call `runner.process_document_by_path(path)`; for `delete` op call `runner.delete_document_at(path)`; on success `mark_done` or `mark_deleted_row`; on failure `record_failure` with backoff = `30 * 2^(attempts-1)` seconds capped at `max_retries`); after draining the due batch, run GC = `cleanup_orphaned_data(runner)` (already moved into the ingestion crate in the prior attempt — keep it). `run_loop(&self, stop: watch::Receiver<bool>, poll_interval)` ticks, exits on stop, callable on the serve owner thread (Runner `!Send`).
- `crates/ingestion/src/lib.rs` — `pub mod worker;`

**Dependencies:** 1.1, 1.3.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p ingestion` pass.
- Integration test (in-memory DB + fake embed/index): enqueue a failing doc (inject a NER parse error) → `run_once` retries up to `max_retries` with increasing `next_attempt_at`, then status `error` with `last_error` set; a succeeding doc → `done` and row present in `documents`. GC runs after the cycle (orphaned chunk/entity rows removed).
- Worker does NOT call `source.parse(...)` / `Runner::process_document_at` / `ingest_source_by_path` (grep must return 0). The worker reads exactly one file per job via `parse_file`; it never walks a source directory.

**Oracle reference:** `../synopsis/internal/ingestion` `Ingest` loop + `cleanupOrphanedData` — behavior reference; retry is new Rust behavior.

## 1.5 Watcher → producer; startup reconcile replaces initial sync

**Goal:** Watcher and startup become producers (enqueue), not direct ingesters.

**Scope файлов:**
- `crates/cli/src/serve/watcher.rs` — `IngestChangeHandler::handle_changes` replaces `ingest::ingest_source_by_path(...)` + `ingest::prune_deleted(...)` with `job_queue.enqueue_index`/`enqueue_delete` for the affected source(s) (derive source path from changed file paths via `runner.find_source_for_path`). Keep graph-reload hook unchanged.
- `crates/cli/src/serve/server.rs` — startup initial sync replaced by `job_queue.reconcile_source(...)` for each enabled source (enqueues diffs); the worker later processes them.
- `crates/cli/src/serve/ingest.rs` — keep `ingest_source_by_path` for the one-off `sync` CLI subcommand (manual sync still directly ingests), but the serve path uses the queue. Mark the watcher's old direct calls removed.

**Dependencies:** 1.3, 1.4.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p cli` pass.
- Watcher test: a changed file enqueues a `document_jobs` row (status `pending`), does NOT directly call `runner.ingest_*` (assert via a spy/fake runner or by checking the queue row exists and `documents` unchanged until worker runs).
- Startup reconcile enqueues new/changed/removed docs; `documents` table unchanged until worker processes.

**Oracle reference:** `../synopsis/cmd/app/serve.go` `setupFileWatcher` callback — ported behavior (callback now enqueues instead of ingesting).

## 1.6 Owner-loop wiring: spawn worker, `RetryBatch` arm, drop standalone orphan scheduler

**Goal:** Wire the worker into the serve owner loop; remove the separate orphan-cleanup scheduler (GC now in worker).

**Scope файлов:**
- `crates/cli/src/serve/server.rs` — in `serve_with_stop`: build `DocumentJobQueue` + `DocumentWorker`; spawn the worker loop as a background task (or drive it on the owner thread via a channel — keep `Runner` `!Send` safe). Add `OwnerEvent::RetryBatch` arm to the `select!` (or have the worker task own its poll loop and only surface completion). Remove the standalone `orphan_cleanup` scheduler task (lines ~476-483, 558, 628-630) since GC moved into the worker (task 1.4). Graceful shutdown: stop the worker on `Stop`.

**Dependencies:** 1.4, 1.5.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p cli` pass.
- Serve starts; a pre-enqueued `document_jobs` row is processed by the worker (document appears in `documents`); `GET /health` still 200; graceful shutdown aborts the worker without panics.
- No standalone orphan-cleanup scheduler remains (GC verified inside worker tests, task 1.4).

**Oracle reference:** n/a (worker wiring is new Rust orchestration).

## 1.7 CLI `queue status` / `queue reset-retries`

**Goal:** Inspect and repair the document queue from the CLI.

> **Решение по контракту (2026-08-29):** подкоманда переименована из `index` в `queue` (имя точнее отражает единую таблицу состояний `document_jobs`). См. cli-surface spec delta.

**Scope файлов:**
- `crates/cli/src/cli.rs` — add `queue` subcommand dispatching to `crates/cli/src/queue.rs`.
- `crates/cli/src/queue.rs` (NEW) — `queue status [--source PATH] [--status NAME]` prints a table (path, source, status, attempts, last_error, next_attempt_at) from `DocumentJobDao::list`; `queue reset-retries [--source PATH] [--path PATH]` calls `DocumentJobDao::reset_retries` for matching `error` rows (or a single `--path`).
- `crates/cli/src/main.rs` — if arg parsing lives there, register `queue`.

**Dependencies:** 1.1.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p cli` pass.
- `cargo run -- queue status` prints the queue table; `queue status --status error` filters; `queue reset-retries --path <p>` flips that `error` job to `pending`/attempts=0 (verified by a subsequent `queue status`).
- CLI-surface spec delta (this change) documents the subcommand.

**Oracle reference:** n/a — no oracle equivalent; new operational command.

## 1.8 Tests: DAO, state machine, worker + retry + GC, watcher-producer, CLI

**Goal:** Cross-cutting verification; full workspace green.

**Scope файлов:** test modules in the files above (`document_job.rs`, `job_queue.rs`, `worker.rs`, `watcher.rs`, `index.rs`) + any integration test in `crates/cli/tests` or `crates/ingestion/tests`.

**Dependencies:** 1.1–1.7.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all pass.
- At least one end-to-end test: enqueue a failing doc → worker retries → `error`; `index reset-retries` → worker re-processes → `done`.
- No `eprintln!("warning: document ... failed")` remains in `ingester/mod.rs` (replaced by queue + `tracing::error!` in the serve layer); `grep -rn 'warning: document' crates/ingestion/src` returns 0.

**Oracle reference:** n/a.

## 1.9 Remove dead code and test-only scaffolding

**Goal:** After the functional tasks land, delete genuinely unused code and code that exists only to support tests (per user directive 2026-08-29).

**Scope файлов:** whole workspace (focus `crates/ingestion`, `crates/cli`).

**Dependencies:** 1.1–1.8.

**Критерии приёмки:**
- `cargo clippy --workspace --all-targets -- -D warnings` (which denies `dead_code`) passes; `grep -rn "allow(dead_code)\|allow(unused)" crates/` returns only intentional, documented cases.
- No functions/branches/methods that are never called from production OR test code remain (verify with `cargo clippy` + a quick `grep` of each candidate).
- Test-only scaffolding that is not real functionality (e.g. fake parsers/sources kept solely as throwaway harness) is trimmed where it does not reduce coverage; trait method stubs required for compilation (e.g. `TestSource::parse_file`) stay since they are needed to satisfy the `Parser` trait in `#[cfg(test)]`.
- `cargo fmt --all --check`, `cargo test --workspace` still pass.

**Oracle reference:** n/a.
