# Tasks — parity-fixture-expansion

**Change header (context for every fresh agent):**
- Workspace: synopsis-rs (Rust rewrite of the Go service "Synopsis"). Read
  `openspec/config.yaml` (frozen stack, execution model) and `AGENTS.md` (commands,
  gotchas, OpenSpec workflow) first.
- This is a **test-infrastructure** change (`skip_specs: true`) — it adds machine-checked
  **content parity** to `crates/parity-harness`. It changes **no product crate, no frozen
  contract, no `Cargo.toml`, no `Cargo.lock`**.
- The Go oracle (`../synopsis`) is **read-only**. The Go binary (`../synopsis/bin/synopsis`)
  is driven to *record* fixtures once (task 1.3); it is never modified. A temporary Go
  config is written to a scratch dir during recording and is NOT committed to `../synopsis`.
- Content parity compares Rust tool responses against **golden JSON fixtures recorded from
  the Go binary**, using the existing strict comparator `parity_harness::diff::json_diff`
  after a **normalization** step that strips volatile, implementation-defined fields.
- The model used on BOTH sides is **bge-small-en-v1.5 / 384-dim** (the Go oracle's default
  config already uses it; the Rust parity harness uses it via `build_provider`). Tests
  **graceful-skip** (like the existing `parity_test.rs`) when the model / ONNX runtime is
  unavailable.
- Gates that must stay green after every task: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`,
  `cargo test --workspace`.
- Per-task diff ≤ ~500 lines; each task is self-contained (a fresh agent with no prior
  context can complete it from this body alone).

---

## 1. Content-parity record/verify module

- [x] 1.1 Add a `content_parity` module to `parity-harness` (record/verify + normalization)

**Goal.** Add a reusable module that (a) **records** a tool response from a running MCP
server into a committed JSON fixture, and (b) **verifies** a Rust tool response against a
fixture using `json_diff` after normalization.

**File scope.** `crates/parity-harness/src/content_parity.rs` (new) and
`crates/parity-harness/src/lib.rs` (add `pub mod content_parity;`). Do not modify any other
harness file, any product crate, or any `Cargo.toml`.

**Dependencies.** None.

**Approach.** Implement in `content_parity.rs`:
1. `pub fn record_response(url: &str, tool: &str, args: &serde_json::Value, out: &Path)
   -> Result<(), HarnessError>` — connect an `McpClient` to `url`, call `tool` with `args`,
   and write the response JSON (pretty-printed, sorted keys) to `out`. Used by the one-time
   recording step (task 1.3) against the **Go** server.
2. `pub fn load_fixture(path: &Path) -> Result<serde_json::Value, HarnessError>`.
3. `pub fn normalize(value: serde_json::Value, tool: &str) -> serde_json::Value` — strip
   volatile fields per tool (see design D4): exact numeric scores/confidence/rank; sort
   unordered result arrays by a stable key; strip timestamps/durations/server
   version/request ids. `search` keeps rank order and keeps identity fields (doc/chunk id,
   title). Document the exact stripped set per tool in a doc comment.
4. `pub fn assert_content_parity(fixture: &serde_json::Value, actual: &serde_json::Value,
   tool: &str)` — normalize both, run `json_diff`, and panic with the full diff list if
   non-empty.
5. Unit tests for `normalize` (a score is stripped, an unordered array is sorted, a
   timestamp is stripped, `search` identity fields survive) and for `assert_content_parity`
   (equal-after-normalization passes; a real divergence fails).

**Acceptance criteria.**
1. `content_parity` is a `pub mod` in `parity-harness` and compiles (`cargo check
   -p parity-harness`).
2. `record_response`, `load_fixture`, `normalize`, and `assert_content_parity` are public
   and documented (`missing_docs` is deny at the workspace level).
3. `normalize` strips scores/rank/confidence, sorts unordered arrays, strips
   timestamps/version, and preserves `search` identity fields — covered by unit tests.
4. `assert_content_parity` passes on equal-after-normalization input and fails (with a
   non-empty diff) on a real divergence — covered by unit tests.
5. All four gates pass; no product crate, `Cargo.toml`, or `Cargo.lock` changed; `../synopsis`
   untouched.

---

## 2. Expanded content-parity corpus

- [x] 1.2 Add a `write_content_corpus` writer (8 markdown docs, 3 domains)

**Goal.** Add a deterministic, expanded corpus for the content-parity tests, separate from
the latency test's 2-doc corpus.

**File scope.** `crates/parity-harness/src/content_parity.rs` (add the writer) or a new
`crates/parity-harness/src/corpus.rs` (new, plus `pub mod corpus;` in `lib.rs`). Do not
modify the existing `write_corpus` in `tests/parity_test.rs` (the latency corpus is
untouched), and do not modify any product crate or `Cargo.toml`.

**Dependencies.** None.

**Approach.** Implement `pub fn write_content_corpus(corpus: &Path)` that writes **8
markdown docs across 3 sub-directories** (`hr/`, `product/`, `eng/`), each a **static
string literal** (no random, no `chrono` timestamps) so ingestion is byte-for-byte
deterministic. Requirements:
- 3 domains; each domain has 2–3 docs with distinct topics.
- A **consistent entity vocabulary** across domains (a few named people, systems, and
  policies that recur) so multi-domain listing and cross-domain search are exercised.
- Enough docs (≥ 8) that `catalog_documents` pagination (page size < total) is exercised.
- No NER-relevant ambiguity that would make extraction non-deterministic is required — NER
  is disabled in both the Go recording config and the Rust harness, so only chunk text
  matters.

**Acceptance criteria.**
1. `write_content_corpus` writes 8 markdown files under `hr/`, `product/`, `eng/` and
   creates the directories.
2. Content is static (no `rand`, no `chrono`, no `SystemTime`) — a unit test asserts the
   written bytes are identical across two calls.
3. The existing `write_corpus` (latency corpus) is byte-for-byte unchanged.
4. All four gates pass; no product crate, `Cargo.toml`, or `Cargo.lock` changed;
   `../synopsis` untouched.

---

## 3. Record golden fixtures from the Go oracle

- [x] 1.3 Record `search` + catalog fixtures from `../synopsis/bin/synopsis` (one-time)

**Goal.** Produce committed golden fixtures for `search`, `catalog_overview`,
`catalog_documents`, and `catalog_entities` by driving the **Go** binary over the content
corpus with a config that matches the Rust parity harness.

**File scope.** New fixtures under
`crates/parity-harness/fixtures/content/` (`search.json`, `catalog_overview.json`,
`catalog_documents.json`, `catalog_entities.json`) plus a **record driver** — either a
`#[test] #[ignore]` (run with `cargo test -p parity-harness -- --ignored record_content_
fixtures`) or a small `examples/record_content.rs` — that calls
`content_parity::record_response` against the Go server. Do not modify any product crate,
`Cargo.toml`, or `Cargo.lock`, and do **not** write anything into `../synopsis`.

**Dependencies.** Tasks 1.1 (record_response) and 1.2 (write_content_corpus) complete.

**Approach.**
1. Write a **temporary Go config** to a scratch dir (e.g. `target/parity-go/parity.yaml`)
   that matches the Rust harness: `embeddings.local: {model_name: bge-small-en-v1.5,
   vector_dim: 384}`, `ingestion.ner.disabled: true`, `graph.enable_graph: false`,
   `linker.disabled: true`, and the same `search` + `ingestion.chunking.markdown` values the
   Rust harness uses (copy from `crates/parity-harness/tests/parity_test.rs`).
2. Copy the content corpus (from task 1.2) into the Go `paths.documents_dir` and run
   `../synopsis/bin/synopsis -config <scratch>/parity.yaml -db <scratch>/knowledge.db sync`.
3. Run `… serve --port <free-port>`, point `content_parity::record_response` at
   `http://127.0.0.1:<port>/mcp`, and record the four tools' responses into
   `fixtures/content/*.json`. For `search` and `catalog_documents`, use the **same args**
   the verify tests will use (a fixed query string; a fixed `page_size`/cursor).
4. Shut the Go server down. Each fixture file gets a **header comment** with the corpus
   description + the Go config summary (model, NER/graph/linker state) + the exact tool
   args, so the fixture is self-describing and re-recordable.

**Acceptance criteria.**
1. Four fixture files exist under `fixtures/content/`, each a valid JSON object with a
   header comment (corpus + config + args).
2. `catalog_overview.json` shows non-zero document/chunk counters (the corpus ingested).
3. `catalog_documents.json` shows paginated rows; `catalog_entities.json` shows the
   (possibly empty) entity listing; `search.json` shows a non-empty result list for the
   fixed query.
4. The record driver is `#[ignore]`d (or an example) so it does NOT run in the default
   `cargo test` gate.
5. All four gates pass (the ignored record test is not run by default); no product crate,
   `Cargo.toml`, or `Cargo.lock` changed; `../synopsis` untouched (verify with
   `git status` that nothing under `../synopsis` changed).

**Note to the implementer.** This task needs the Go binary + bge-small model locally. If
the environment cannot run the Go binary, stop and report `status=blocked` with the exact
error — do NOT fabricate fixtures.

---

## 4. Catalog content-parity tests

- [ ] 1.4 Add content-parity tests for `catalog_overview`, `catalog_documents`,
      `catalog_entities`

**Goal.** Assert the Rust catalog tool responses match the Go fixtures (after
normalization).

**File scope.** `crates/parity-harness/tests/content_parity.rs` (new integration test). Do
not modify any product crate, `Cargo.toml`, or `Cargo.lock`, and do not modify the existing
`parity_test.rs` / `sse_parity.rs`.

**Dependencies.** Tasks 1.1 (module), 1.2 (corpus), 1.3 (fixtures) complete.

**Approach.** In `tests/content_parity.rs`:
1. Boot the product MCP server in-process over the **content corpus** (reuse the boot
   pattern from `tests/parity_test.rs`: `build_provider` graceful-skip, `write_content_
   corpus`, ingest via the production pipeline with `ner.disabled`, `GraphIndex::
   Unavailable`, `PooledSearcher`, `Server::new`, random loopback port).
2. For each of `catalog_overview`, `catalog_documents` (with the same `page_size`/cursor as
   the fixture), and `catalog_entities`: call the Rust tool, `load_fixture`,
   `assert_content_parity(fixture, actual, tool)`.
3. Graceful-skip (print `SKIP:` and return) when `build_provider` returns `None` (no model
   / ONNX runtime), exactly like `parity_test.rs`.

**Acceptance criteria.**
1. Three catalog content-parity assertions exist and pass when the model is present
   (`cargo test -p parity-harness -- content_parity`).
2. The tests graceful-skip (not fail) when the model is absent.
3. The existing `parity_test.rs` latency test is unchanged and still passes.
4. All four gates pass; no product crate, `Cargo.toml`, or `Cargo.lock` changed;
   `../synopsis` untouched.

---

## 5. Search content-parity test

- [ ] 1.5 Add a content-parity test for `search`

**Goal.** Assert the Rust `search` response matches the Go fixture (result count + top-
result identity), after normalization.

**File scope.** `crates/parity-harness/tests/content_parity.rs` (extend the file from task
1.4). Do not modify any product crate, `Cargo.toml`, or `Cargo.lock`.

**Dependencies.** Tasks 1.1, 1.2, 1.3, 1.4 complete.

**Approach.** In `tests/content_parity.rs`:
1. Add a test that calls the Rust `search` tool with the **same query args** the Go fixture
   was recorded with (from the fixture header comment), then
   `assert_content_parity(fixture, actual, "search")`.
2. Rely on `normalize` (task 1.1) to strip exact scores and compare result count +
   identity fields (doc/chunk id, title) in rank order.
3. If the Go and Rust `search` results diverge on identity (not just score), the test
   fails with the `json_diff` output — this is the intended signal of a real behavior
   divergence. Do NOT weaken `normalize` to hide it; report the divergence.

**Acceptance criteria.**
1. A `search` content-parity assertion exists and passes when the model is present.
2. The test compares result count + top-result identity (not exact scores).
3. Graceful-skip when the model is absent.
4. All four gates pass; no product crate, `Cargo.toml`, or `Cargo.lock` changed;
   `../synopsis` untouched.

---

## 6. Final verification

- [ ] 1.6 Final verification: gates + fixture integrity + scope

**Goal.** Machine-verify the whole change before archive.

**File scope.** Read-only (no code edits). The only files this change should have touched
are under `crates/parity-harness/` (`src/content_parity.rs` or `src/corpus.rs`,
`src/lib.rs`, `tests/content_parity.rs`, `fixtures/content/*.json`, and the record driver).

**Dependencies.** Tasks 1.1–1.5 complete.

**Acceptance criteria.**
1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo check --workspace`, `cargo test --workspace` all green.
2. The existing `parity_test.rs` (latency) and `sse_parity.rs` tests are byte-for-byte
   unchanged and still pass.
3. The four fixture files exist, are valid JSON, and each has a header comment (corpus +
   Go config + args).
4. The record driver is `#[ignore]`d / an example — it does not run in the default gate.
5. No product crate, `Cargo.toml`, or `Cargo.lock` changed; `../synopsis` untouched (verify
   with `git status` / `git diff --name-only`).
6. `git diff` of the change is scoped to `crates/parity-harness/**` (plus the openspec
   change artifacts).
