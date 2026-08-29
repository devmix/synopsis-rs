# Design: remove-direct-ingest

## Summary

Remove the legacy direct-ingest path (`Runner::ingest_all` and friends) so the
`document_jobs` queue is the only document-processing mechanism. Add a `db clear`
CLI command for wiping a dataset's knowledge DB with a confirmation prompt.

## Architecture

After this change:

```
producer (watcher / startup reconcile / db clear+restart)  ──enqueue──▶  document_jobs
                                                                worker ──▶ Runner::process_document_by_path ──▶ DB
```

There is NO code path that parses + ingests a whole source tree inline.

### `db` command (`stats` + `clear`)

- New module `crates/cli/src/db.rs` with `run_db(cmd, cfg, db)`.
- `db stats`: open the dataset-bound knowledge DB, gather + print counts via
  existing DAOs (`DocumentDao::count`, `ChunkDao::count`, `EntityDao::count`,
  `FactDao::count`, `EntityLinkDao::count`, `DocumentJobDao::list(...).len()` for
  queue rows). Read-only, no prompt, no deletion.
- `db clear`:
  1. Open the dataset-bound knowledge DB.
  2. Gather + print the same stats.
  3. Prompt `Confirm deletion? [y/N]` on stdin; read a line; proceed only on `y`/`Y`.
  4. On confirm, FIRST `drop(db)` (close the connection), THEN delete the whole
     dataset state directory via a shared `clear_dataset(state_path)` helper that
     runs `std::fs::remove_dir_all(state_path)` (ignoring a missing dir), where
     `state_path = config.dataset.state_path(&config.paths.workspace_dir)`. This
     removes `knowledge.db` (SQLite) and `vectors/` (Lance index) in one shot.
     Reused by the `force_rebuild` recovery path. On `n`/EOF/empty, print
     `aborted: dataset unchanged` and return SUCCESS without deleting.
- No embedding model / ONNX loaded (same as the `queue` command).

### Removing direct ingest

- `Runner` (crates/ingestion/src/runner/mod.rs): delete `ingest_all`,
  `ingest_source`, `sync_source`, `ingest_source_by_path`, `prune_deleted`. Keep
  `process_document_by_path`, `delete_document_at`, `cleanup_orphaned_data`.
- CLI:
  - Delete `crates/cli/src/sync.rs` and the `Sync` variant in `cli.rs`; remove
    `pub mod sync` from `lib.rs`/`serve/mod.rs`; remove its `main.rs` dispatch.
  - Delete `crates/cli/src/serve/ingest.rs` (the `ingest_all` wrapper) and its
    `pub mod ingest` in `serve/mod.rs`.
  - Delete `bootstrap::initial_sync` (crates/cli/src/serve/bootstrap.rs).
  - `server.rs` `force_rebuild` branch: the `serve` process holds the `Arc<Db>`
    open and borrowed by the runner/job_queue/worker, so the whole `state` dir
    MUST NOT be deleted here (that would orphan the connection and
    `reconcile_source` would write to a deleted inode). Instead clear the
    knowledge DB tables IN PLACE via a new `clear_dataset_tables(db: &Db)` helper
    (SQL transaction: entity_links, facts, chunks, entities, documents,
    document_jobs; join tables via ON DELETE CASCADE), then run the SAME startup
    `reconcile_source` loop as `initial_sync_due` so the worker re-embeds every
    source file. The vectors are already recreated by `recreate_vectors_engine`
    (called earlier in `run_serve` on dimension mismatch). This is clear-then-queue,
    not direct ingest. (The `clear_dataset(state_path)` dir-delete helper from 1.1
    stays for the `db clear` command, which closes the DB first.)

### Tests

- `crates/ingestion/tests/pipeline_e2e.rs`: replace `runner.ingest_all(...)` setup
  with `DocumentJobQueue::reconcile_source(&runner, src)` (or `enqueue_index`) +
  `DocumentWorker::run_once(now)`; assert on DB state as before.
- `crates/ingestion/src/runner/mod.rs` / `runner/cleanup.rs` unit tests that call
  removed methods: rewrite through the queue/worker or delete if they only exercised
  the removed path.
- Delete the `prune_deleted` test (no production caller remains).

## Risks / Constraints

- `db clear` is destructive; the confirmation prompt + stats are the only guard. It
  is a one-shot CLI, not the serve path.
- After `db clear` the user must restart `serve` for startup reconcile to re-enqueue
  (the watcher does not fire on unchanged files). Documented in help text.
- `force_rebuild` recovery now clears the whole dataset DB (not just vectors); on
  restart everything re-embeds. Acceptable for a 16 GB laptop personal use case.
- `unsafe_code = "forbid"`; clippy `-D warnings` (dead_code denied) must stay clean
  after removal.
- Frozen CLI-surface contract changes (`sync` removed, `db clear` added) are recorded
  in the cli-surface spec delta and require this explicit decision (user-approved
  2026-08-29).
