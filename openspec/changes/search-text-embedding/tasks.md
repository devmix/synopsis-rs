# Tasks — search-text-embedding

Each task is self-contained for a fresh agent (~100k context): goal, exact file
scope, dependencies, machine-checkable acceptance criteria, and the Go oracle
reference. Final diff per task ≤ ~500 lines of code + tests. The Go original in
`../synopsis` is a read-only oracle — never modify it.

## 1. Schema + storage (crates/db)

- [ ] 1.1 Add the `search_text` column + re-point the FTS index (migration `5-search-text`) and update `Chunk`/`ChunkDao`

**Goal.** Introduce `search_text` on the `chunks` table and make the FTS5 index
operate on it, so the lexical leg can match heading terms. `chunk_text` and the
byte offsets are unchanged.

**File scope.**
- NEW `migrations/knowledge/5-search-text/up.sql`.
- `crates/db/src/chunk.rs` (the `Chunk` struct, `ChunkDao::create`/`update`,
  `SELECT_CHUNK`, `FTS_QUERY`, `row_to_chunk`, and the test module).
- Do not touch any other crate, `Cargo.toml`, or `Cargo.lock`.

**Dependencies.** None.

**Acceptance criteria.**
1. `up.sql` (in this order): `ALTER TABLE chunks ADD COLUMN search_text TEXT NOT NULL DEFAULT '';`
   then `UPDATE chunks SET search_text = chunk_text WHERE search_text = '';` then
   `DROP TABLE chunks_fts;` then re-create `chunks_fts` as `fts5(search_text,
   content='chunks', content_rowid='id')` then re-create the `chunks_fts_ai/ad/au`
   triggers referencing `new.search_text` / `old.search_text`; finish with
   `PRAGMA user_version = 5;`.
2. A fresh build reports `PRAGMA user_version` = 5 and the `chunks` table has a
   `search_text` column; `chunks_fts` indexes `search_text`.
3. `Chunk` exposes both `chunk_text` and `search_text`; `ChunkDao::create` and
   `ChunkDao::update` accept and store `search_text`; row reads return both.
4. A unit test proves the FTS index is over `search_text`: a term present only in
   `search_text` (not in `chunk_text`) is found by `search_fts`, while a term only
   in `chunk_text` is not.
5. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/database/dao/chunk_dao.go` (ChunkDao
shape); `../synopsis/migrations/*.sql` (the v5 `chunks` + `chunks_fts` shape being
extended). The `search_text` column is the explicit, justified deviation (design D1/D2).

## 2. Ingestion (crates/ingestion)

- [ ] 2.1 Chunker emits `search_text`; ingester embeds + persists it

**Goal.** The Markdown chunker produces `search_text = breadcrumb + "\n\n" + body`
(or `body` when there is no breadcrumb); the per-document pipeline embeds
`search_text` and persists both `chunk_text` and `search_text`. NER is unchanged.

**File scope.**
- `crates/ingestion/src/types.rs` (`DocumentChunk` gains `search_text`; update the
  byte-offset-invariant docs to note `search_text` is the only synthetic field).
- `crates/ingestion/src/chunkers/markdown.rs` (build `search_text` from the
  breadcrumb already computed for metadata + the body).
- `crates/ingestion/src/ingester/mod.rs` (embed `search_text`; pass both
  `chunk_text` + `search_text` to `ChunkDao::create`).
- The crate's tests (chunker + ingester).
- Do not touch NER (`crates/ingestion/src/ner/*`), any other crate, `Cargo.toml`,
  or `Cargo.lock`.

**Dependencies.** Task 1.1 (so `ChunkDao::create` accepts `search_text`).

**Acceptance criteria.**
1. For a chunk under a heading hierarchy, `search_text == breadcrumb + "\n\n" +
   body`; for a chunk with no breadcrumb (preamble), `search_text == text`.
2. The byte-offset invariant still holds: `content[start_offset..end_offset] ==
   text` for every produced chunk (existing invariant tests still pass).
3. The ingester passes `search_text` (not `text`) to the embedding provider; a
   test or assertion confirms the embedding input is `search_text`.
4. The ingester writes both `chunk_text` and `search_text` to the chunk row.
5. NER still receives `text` + metadata extras (not `search_text`); NER tests are
   unchanged and still pass.
6. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/ingestion/chunkers/markdown_chunker.go`
(`buildBreadcrumbs` + `chunkText = breadcrumb + "\n\n" + body` — the exact
`search_text` shape the oracle feeds both legs).

## 3. Search (crates/search)

- [ ] 3.1 Result `text` field = `search_text`

**Goal.** The fused, ranked search result's `text` field carries the chunk's
`search_text` (breadcrumb + body), so a returned chunk has its section context.
The lexical and semantic hits that feed the fusion carry `search_text` too. RRF,
rerank, enrichment, and the response field set are otherwise unchanged.

**File scope.**
- `crates/search/src/lib.rs` (`SearchResult` text field + the lexical/semantic hit
  types), `crates/search/src/lexical.rs`, `crates/search/src/semantic.rs` (populate
  the hit text from `Chunk.search_text`), and the crate's tests.
- Do not touch any other crate, `Cargo.toml`, or `Cargo.lock`.

**Dependencies.** Task 1.1 (so `Chunk` exposes `search_text`); Task 2.1 is needed
only for end-to-end tests that ingest real chunks.

**Acceptance criteria.**
1. A search result's `text` field equals the matched chunk's `search_text`
   (breadcrumb + body), verified by a unit test that seeds a chunk with distinct
   `chunk_text` and `search_text` and asserts the result `text` is `search_text`.
2. The lexical and semantic hit types carry `search_text` (not `chunk_text`).
3. RRF fusion order, rerank, and the response field set are unchanged (existing
   search tests still pass).
4. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/search/` (the `SearchResult` whose
`text` is the breadcrumb-prefixed chunk text the oracle returns).

## 4. Parity verification

- [ ] 4.1 Re-run `parity-fixture-expansion` task 1.5 (search identity parity)

**Goal.** Confirm the search identity now matches the Go oracle, allowing the
`parity-fixture-expansion` task 1.5 test to assert **full identity parity** (no
`normalize` weakening).

**File scope.** `crates/parity-harness/tests/content_parity.rs` (the search
assertion only — remove any temporary relaxation; do not touch the catalog asserts).

**Dependencies.** Tasks 1.1, 2.1, 3.1.

**Acceptance criteria.**
1. `cargo test -p parity-harness --test content_parity` passes with the search
   assertion comparing the full identity (document_id + chunk_id in rank order)
   against `fixtures/content/search.json` — top-5 must match the Go fixture
   `[18,25,33,17,3]`.
2. If a residual difference remains (e.g. FTS tokenizer or embedding float margin),
   report it with the exact divergence — do NOT weaken `normalize` to hide it.
3. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `crates/parity-harness/fixtures/content/search.json`
(recorded once from `../synopsis/bin/synopsis` over the task 1.2 corpus).
