# Tasks: remove the unused `DocumentChunk.doc_id` field

Change: `remove-document-chunk-doc-id`

## Change header (read first)

- **Goal:** delete the dead `doc_id: Option<i64>` field from
  `DocumentChunk` (`crates/ingestion/src/types.rs`) — it is always `None`,
  never read in production, and documents a never-implemented design
  ("series change 3"). Update every construction site and test reference.
- **Context (read these first):**
  - `openspec/changes/remove-document-chunk-doc-id/proposal.md`
  - `openspec/changes/remove-document-chunk-doc-id/design.md` (D1, D2)
  - `crates/ingestion/src/types.rs` — the `DocumentChunk` struct
    (`:91`) and its test module
  - `openspec/config.yaml` (frozen stack), `AGENTS.md` (gates)
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test --workspace`.
- **Do NOT touch:** `Chunk.doc_id` in `crates/db/src/chunk.rs` (the DB FK —
  a different struct); local `doc_id: i64` variables in
  `ingester/mod.rs`, `runner/mod.rs` (`clear_and_delete_doc`),
  `tests/pipeline_e2e.rs`.

- [x] **1.1** — Delete the field and update all references.

  **Goal:** `DocumentChunk` no longer has a `doc_id` field; the workspace
  compiles and every gate is green.

  **File scope (exactly these):**

  - `crates/ingestion/src/types.rs` — remove the `pub doc_id: Option<i64>`
    field and its doc comment from `DocumentChunk` (`:91–94`); in the test
    module: drop `doc_id: None` from the fixtures (`:211`, `:256`,
    `:313–314`, including the stale "assigned by the write stage (series
    change 3)" comment) and delete the assertion
    `assert_eq!(chunks[0].doc_id, None)` (`:247`).
  - `crates/ingestion/src/chunkers/mediawiki.rs` — drop `doc_id: None`
    (`:311`); delete the test assertion
    `assert_eq!(c.doc_id, None, "chunk {i} doc_id")` (`:373`).
  - `crates/ingestion/src/chunkers/markdown.rs` — same pair (`:343`, `:409`).
  - `crates/ingestion/src/chunkers/json.rs` — same pair (`:389`, `:434`).
  - `crates/ingestion/tests/ingester.rs` — drop `doc_id: None` (`:112`,
    `:203`).
  - `crates/ingestion/tests/runner.rs` — drop `doc_id: None` (`:94`).
  - `crates/ingestion/src/runner/mod.rs` — drop `doc_id: None` in the test
    module (`:680`).
  - `crates/ingestion/src/worker.rs` — drop `doc_id: None` in the test
    module (`:291`).

  **Acceptance criteria (machine-checkable):**

  - `grep -rn "doc_id" crates/ingestion/src/types.rs
    crates/ingestion/src/chunkers/ crates/ingestion/tests/ingester.rs
    crates/ingestion/tests/runner.rs crates/ingestion/src/runner/mod.rs
    crates/ingestion/src/worker.rs` → no matches for `DocumentChunk`
    (the `clear_and_delete_doc(doc_id: i64)` signature in
    `runner/mod.rs` is a local DB-layer parameter and must remain — verify
    it is the only remaining `doc_id` there).
  - `cargo test --workspace` green (the deleted assertions are the only
    test changes; no other test count changes).
  - Workspace fmt/clippy green.
