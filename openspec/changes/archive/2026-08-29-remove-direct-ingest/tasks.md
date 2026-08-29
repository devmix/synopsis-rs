# Tasks: remove-direct-ingest

- [x] 1.1 Add `db` CLI command (subcommands `stats` + `clear`; `clear` = confirmation + delete, `stats` = read-only counts)
- [x] 1.2 Remove CLI direct-ingest callers (`sync` subcommand, `serve/ingest.rs` wrapper, `bootstrap::initial_sync`, `server.rs` force_rebuild branch → clear+reconcile)
- [x] 1.3 Remove `Runner` direct-ingest methods `ingest_source`/`sync_source`/`ingest_source_by_path`/`prune_deleted`; fix runner unit tests
- [x] 1.4 Remove `Runner::ingest_all`; rewrite `pipeline_e2e.rs` tests through queue/worker; delete `prune_deleted` test
- [x] 1.5 Final verification (workspace gates green, no direct-ingest references remain) + archive

## 1.1 Add `db` CLI command (`stats` + `clear`)

**Goal:** New `db` subcommand with `stats` (read-only counts) and `clear` (confirm + delete).

**Scope файлов:**
- `crates/cli/src/cli.rs` — add `Db(DbCommand)` + `DbCommand::Stats` and `DbCommand::Clear` (clap builder + from_matches).
- `crates/cli/src/db.rs` (NEW) — `run_db(cmd, cfg, db)`:
  - `stats`: gather + print counts (DocumentDao::count, ChunkDao::count, EntityDao::count, FactDao::count, EntityLinkDao::count, DocumentJobDao::list len) — read-only, no prompt.
  - `clear`: print the same stats, prompt `Confirm deletion? [y/N]` on stdin, on `y`/`Y` FIRST `drop(db)` (close the connection), THEN call `clear_dataset(state_path)` where `state_path = config.dataset.state_path(&config.paths.workspace_dir)`. On `n`/EOF/empty, print `aborted: dataset unchanged` and return SUCCESS without deleting.
  - Export `pub(crate) fn clear_dataset(state_path: &Path) -> Result<(), CliError>` that does `if state_path.exists() { std::fs::remove_dir_all(state_path)?; }` — deletes the whole dataset state directory (`knowledge.db` + `vectors/`) in one shot; reused by task 1.2 force_rebuild path.
- `crates/cli/src/lib.rs` — `pub mod db;`.
- `crates/cli/src/main.rs` — dispatch `Command::Db(cmd)`.

**Dependencies:** none (new command; reuses existing DAOs).

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p cli` pass.
- `db stats` prints counts and changes nothing.
- `db clear` prints stats; piped `y` deletes the whole `state` directory (verify `state_path` no longer exists); piped `n` aborts (state dir unchanged).
- `clear_dataset` is callable from `server.rs` (task 1.2) — same helper.
- cli-surface spec delta (this change) documents `db stats` + `db clear`.

**Oracle reference:** n/a — new operational command; no oracle equivalent.

## 1.2 Remove CLI direct-ingest callers

**Goal:** No CLI path calls direct ingest; `force_rebuild` becomes clear-then-queue.

**Scope файлов:**
- `crates/cli/src/cli.rs` — remove `Sync(SyncCommand)` variant + clap builder.
- `crates/cli/src/sync.rs` (DELETE) — remove the `sync` subcommand module.
- `crates/cli/src/lib.rs` / `crates/cli/src/serve/mod.rs` — remove `pub mod sync;` (and `pub mod ingest;` if present).
- `crates/cli/src/serve/ingest.rs` (DELETE) — the `ingest_all` wrapper module; remove `pub mod ingest` from `serve/mod.rs`.
- `crates/cli/src/serve/bootstrap.rs` — delete `initial_sync` (calls `runner.ingest_all`).
- `crates/cli/src/serve/server.rs` — replace the `force_rebuild` branch `ingest::ingest_all(&runner, true)` with an in-place clear of the knowledge DB tables (the `serve` process holds the `Arc<Db>` open and borrowed by the runner/job_queue/worker, so the whole `state` dir MUST NOT be deleted here — that would orphan the connection and `reconcile_source` would write to a deleted inode). Add a `pub(crate) fn clear_dataset_tables(db: &Db) -> Result<(), CliError>` in `crate::db` (SQL transaction deleting entity_links, facts, chunks, entities, documents, document_jobs; join tables via ON DELETE CASCADE) and call it on the open `db`. The vectors are already recreated by `recreate_vectors_engine` (called earlier in `run_serve` on dimension mismatch). Then run the SAME startup `reconcile_source` loop as the `initial_sync_due` branch so the worker re-embeds every source file. Keep the `tracing` warning. (The `clear_dataset(state_path)` dir-delete helper from 1.1 stays for the `db clear` command, which closes the DB first.)
- `crates/cli/src/main.rs` — remove `Sync` dispatch.

**Dependencies:** 1.1 (needs `clear_dataset`).

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` pass.
- `grep -rn "ingest_all\|initial_sync\|SyncCommand" crates/cli/src` returns 0 (except the `Runner::ingest_all` definition, removed in 1.3/1.4).
- `serve` still compiles and starts; `force_rebuild` path clears DB then reconciles (no direct ingest).
- cli-surface spec delta (this change) removes `sync`.

**Oracle reference:** `../synopsis/cmd/app/sync.go` (removed), `../synopsis/cmd/app/serve.go` (ReEmbedChunks → clear+reconcile).

## 1.3 Remove `Runner` direct-ingest methods

**Goal:** Delete `ingest_source`/`sync_source`/`ingest_source_by_path`/`prune_deleted` from `Runner`.

**Scope файлов:**
- `crates/ingestion/src/runner/mod.rs` — delete `ingest_source`, `sync_source`, `ingest_source_by_path`, `prune_deleted` (and their locked helpers if private). Keep `process_document_by_path`, `delete_document_at`, `cleanup_orphaned_data`, and `ingest_all` (still used by pipeline_e2e until 1.4).
- `crates/ingestion/src/runner/cleanup.rs` — keep `cleanup_orphaned_data` + its tests; remove any test that calls `prune_deleted`/`ingest_all`.
- `crates/ingestion/src/runner/mod.rs` unit tests — rewrite any test calling the removed methods through `DocumentJobQueue` + `DocumentWorker::run_once`.

**Dependencies:** 1.2 (CLI callers gone, so these methods have no production caller).

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p ingestion` pass.
- `grep -rn "ingest_source\b\|sync_source\|ingest_source_by_path\|prune_deleted" crates/ingestion/src` returns 0 (except `ingest_all` kept for 1.4).

**Oracle reference:** n/a.

## 1.4 Remove `Runner::ingest_all`; rewrite pipeline_e2e tests

**Goal:** Delete the last direct-ingest method; route all tests through the queue/worker.

**Scope файлов:**
- `crates/ingestion/src/runner/mod.rs` — delete `ingest_all` (and its locked helpers).
- `crates/ingestion/tests/pipeline_e2e.rs` — replace every `runner.ingest_all(...)` setup with `DocumentJobQueue::reconcile_source(&runner, src)` (or `enqueue_index`) + `DocumentWorker::run_once(now)`; keep the DB-state assertions. Delete the `prune_deleted` test.
- Any remaining test referencing `ingest_all` — rewrite or delete.

**Dependencies:** 1.3.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` pass.
- `grep -rn "ingest_all" crates/` returns 0.
- `pipeline_e2e` still verifies parse→chunk→embed→store→vector via the queue/worker.

**Oracle reference:** n/a.

## 1.5 Final verification + archive

**Goal:** Confirm the queue is the only processing path; archive the change.

**Scope файлов:** whole workspace.

**Dependencies:** 1.1–1.4.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all pass.
- `grep -rn "ingest_all\|ingest_source\|sync_source\|initial_sync\|SyncCommand" crates/` returns 0.
- `db clear` + `queue` commands present; `sync` absent.
- Archive via openspec-archive-change with parity note (no oracle equivalent for these commands) and remaining risks.

**Oracle reference:** n/a.
