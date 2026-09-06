# Tasks: vector-loss-self-heal

Read first (binding context): `openspec/config.yaml`, `AGENTS.md`, this
change's `proposal.md`, `design.md` (decisions D1–D4), and the delta spec
under `specs/pipeline/spec.md`. Root cause: a `SIGKILL` loses the `vectors`
RAM layer; the content-hash reconcile does not re-embed unchanged documents,
and only the orphan-*vector* direction of reconciliation is implemented.

Order reflects dependencies: 1.1 → 1.2 (independent code paths; 1.1 bounds
the loss window, 1.2 repairs the residual). Both are Rust implementation
tasks (route to `rust-implementer`). Every task must leave the workspace
green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test` (whole workspace). The `vectors` crate is **unchanged** —
`build_index` / `chunk_ids` already exist and are reused as-is.

- [ ] **1.1** — Per-cycle vector persistence (bound the loss window)

**Goal.** After a `DocumentWorker` cycle that processed one or more `doc:*`
tasks, persist the vector engine's RAM layer to disk so an unclean shutdown
(`SIGKILL`) loses at most the in-progress batch, not everything since the
last flush. Design D1.

**File scope.**
- `crates/ingestion/src/runner/mod.rs` — add a public method to `Runner`:
  `pub fn persist_vectors(&self) -> Result<(), IngestionError>` that calls
  `self.vectors.build_index()` and maps `VectorsError` → `IngestionError`
  (follow the existing conversion the pipeline already uses for vector
  errors, e.g. the `?` pattern in `runner/cleanup.rs`). Document it (the
  workspace denies `missing_docs`).
- `crates/ingestion/src/worker.rs` — in `run_once`, **after** the GC phase
  (`cleanup_orphaned_data`) and only when `processed > 0`, call
  `self.runner.persist_vectors()`; on `Err` log a `tracing::warn!` (do not
  propagate — a persistence failure must not abort the cycle). Keep the
  existing `log_cycle_summary` call.
- Tests (in the `worker.rs` test module, which already has a `MemoryIndex`
  `VectorIndex` stub and a `Harness`):
  - Extend the `MemoryIndex` stub with a `build_index_calls: Mutex<usize>`
    counter that `build_index` increments (it currently returns `Ok(())`
    without recording).
  - A work cycle persists: enqueue one `doc:index`, `run_once` → assert
    `build_index_calls == 1` and the document was created.
  - An idle cycle does not persist: fresh harness, no tasks, `run_once` →
    assert `build_index_calls == 0`.
  - A persistence failure is non-fatal: make `build_index` return an error
    (e.g. a `fail_build_index: Mutex<bool>` flag on the stub), enqueue one
    `doc:index`, `run_once` → the cycle returns `Ok(())`, the document is
    still created, and the task is still `done`.

**Dependencies.** None (first task). Reference: delta spec
`specs/pipeline/spec.md` (`Per-cycle vector persistence`); `build_index` in
`crates/vectors/src/usearch/mod.rs:589` (no-op when the RAM layer is empty);
the existing `MemoryIndex` stub in `worker.rs` tests.

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace): `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo test`.
- The three new worker tests pass (work cycle → 1 save; idle cycle → 0 saves;
  failing save → cycle still `Ok`, document created, task `done`).
- `persist_vectors` is documented; no `vectors` crate change; no new
  dependency.

**Estimated size.** ~150–200 lines (code + tests).

- [ ] **1.2** — Startup vector self-heal (re-embed chunks without a vector)

**Goal.** At serve startup, detect chunk rows that have no corresponding
vector (the residual loss window after a mid-cycle `SIGKILL`) and enqueue
`doc:index` for their documents so the startup drain re-embeds them. Design
D2–D4.

**File scope.**
- `crates/db/src/chunk.rs` — add a light accessor to `ChunkDao`:
  `pub fn list_id_doc_id(&self) -> Result<Vec<(i64, i64)>, DbError>` running
  `SELECT id, doc_id FROM chunks` (no text). Export it as needed (it is a
  method on the existing `ChunkDao`, so no new `lib.rs` export is required —
  verify). Add a DAO test: seed two chunks under two documents, assert the
  returned `(id, doc_id)` pairs.
- `crates/ingestion/src/runner/mod.rs` — add a public method to `Runner`:
  `pub fn heal_missing_vectors(&self, now: i64) -> Result<usize,
  IngestionError>` that:
  1. Reads `(chunk_id, doc_id)` rows via `ChunkDao::list_id_doc_id` (the
     `with_conn` + double-`?` pattern used by `reconcile_vectors`).
  2. Reads `self.vectors.chunk_ids()?` into a `HashSet<u32>`.
  3. Computes the missing set: chunk ids present in SQLite but absent from
     the index; groups them by `doc_id` (`HashMap<i64, Vec<i64>>`).
  4. If empty → return `Ok(0)` (no-op).
  5. Otherwise resolves the affected doc ids to documents via
     `DocumentDao::get_by_ids(&doc_ids)` and, for each, enqueues
     `QueueTaskDao::enqueue(QueueTaskType::DocIndex, &doc.original_path,
     &DocIndexPayload { source_path: doc.original_path.clone(),
     content_hash: None }, now)`; returns the number of documents enqueued.
  Document the method (the workspace denies `missing_docs`).
- `crates/cli/src/serve/server.rs` — at serve startup, **after** the
  `force_rebuild` / `initial_sync_due` reconcile block and **before**
  `let worker = DocumentWorker::new(&db, &runner);` / the startup
  `worker.run_once`, call `runner.heal_missing_vectors(now_unix_seconds())`;
  on `Ok(n) > 0` log `tracing::info!(healed = n, …)`, on `Ok(0)` stay quiet,
  on `Err` log `tracing::warn!` (do not abort startup).
- Tests:
  - `crates/db` (chunk DAO test, as above).
  - `crates/ingestion` (runner test module, which has a `MemoryIndex` stub +
    harness): seed a document through the pipeline (so it has chunks +
    vectors in the `MemoryIndex`), then **clear** the `MemoryIndex` rows
    (simulate a lost RAM layer) while the chunk rows remain, call
    `heal_missing_vectors(now)`, and assert it returns 1 and a `doc:index`
    task for that path is now in `queue_tasks`. A second call (index
    repopulated, or after re-enqueue) with no missing chunks returns 0.
    Also: a fresh empty DB (no chunks) → returns 0 (no-op).

**Dependencies.** 1.1 committed (both add `Runner` methods; 1.2 must build on
the 1.1 state). Reference: delta spec `specs/pipeline/spec.md` (`Vectors
outside the transaction` — `Missing-vector self-heal at startup`);
`reconcile_vectors` in `runner/cleanup.rs:203` (the set-diff pattern, but the
reverse direction); `DocumentDao::get_by_ids` (`crates/db/src/document.rs:171`);
`QueueTaskDao::enqueue` (`crates/db/src/queue_task.rs:182`); the startup
sequence in `serve/server.rs` (~line 449–460).

**Acceptance criteria (machine-checked).**
- Gates green (whole workspace).
- The chunk DAO test passes (light `(id, doc_id)` accessor).
- The runner self-heal tests pass: (a) chunks present + empty index → 1
  `doc:index` enqueued for the affected document; (b) every chunk has a
  vector → 0 (no-op); (c) empty DB → 0.
- `serve/server.rs` performs the self-heal before the startup worker drain
  (verify by reading the call site; a serve-level test is optional if the
  harness does not drive startup — the runner test is the machine check).
- No `vectors` crate change; no new dependency; no CLI/config/schema change.

**Estimated size.** ~300–400 lines (code + tests).
