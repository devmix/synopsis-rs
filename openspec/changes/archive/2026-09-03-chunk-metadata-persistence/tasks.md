# Tasks — chunk-metadata-persistence

Each task is self-contained for a fresh agent (~100k context): goal, exact file
scope, dependencies, machine-checkable acceptance criteria, and the Go oracle
reference. Final diff per task ≤ ~500 lines of code + tests. The Go original in
`../synopsis` is a read-only oracle — never modify it. Read `design.md` and
`openspec/config.yaml` (frozen stack, frozen contracts, task size) before
starting; the task body is the single source of truth for scope.

## 1. Type-hygiene: `DocumentChunk.metadata` → free-form bag (crates/ingestion)

- [x] 1.1 Change `DocumentChunk.metadata` from `DocumentMetadata` to `Map<String, Value>`; chunkers build the bag; NER reads the bag

**Goal.** `DocumentChunk.metadata` is currently a clone of the document's typed
`DocumentMetadata` (a modeling mistake: a chunk should carry its own metadata).
Change it to a free-form `Map<String, Value>` — the chunk's own metadata bag —
and make the three chunkers build that bag directly. NER already consumes a
`&Map<String, Value>`; its call site changes from `&chunk.metadata.extra` to
`&chunk.metadata`. No change to chunk boundaries, `text`, `search_text`, or the
byte-offset invariant; the bag carries the same chunk-specific keys it carried
in `extra` before.

**File scope.**
- `crates/ingestion/src/types.rs` — `DocumentChunk.metadata` field type
  (`DocumentMetadata` → `Map<String, Value>`) + its doc comment; `DocumentMetadata`
  itself is unchanged (still used by `Document`).
- `crates/ingestion/src/chunkers/markdown.rs` — `push_chunk` builds the bag
  (replacing `metadata: metadata.clone()` + the `extra` inserts); the bag carries
  the same keys as before (`section_title`, `heading_level`, `breadcrumb`,
  `image_paths`, …).
- `crates/ingestion/src/chunkers/json.rs` — same (build the bag directly).
- `crates/ingestion/src/chunkers/mediawiki.rs` — same (build the bag directly).
- `crates/ingestion/src/ingester/mod.rs` — the NER call site
  (`ner.extract_entities(&chunk.text, &chunk.metadata.extra)` →
  `ner.extract_entities(&chunk.text, &chunk.metadata)`). Do not touch the
  embedding input (`search_text`) or the document-metadata serialization
  (`doc.metadata.extra` → `documents.metadata_json`).
- `crates/ingestion/src/worker.rs`, `crates/ingestion/src/runner/mod.rs` — every
  `DocumentChunk { … }` construction site that sets `metadata`.
- `crates/ingestion/src/ner/mod.rs` — only if the `extract_entities` doc comment
  needs a one-line clarification (the signature already takes `&Map<String, Value>`).
- The crate's tests: `crates/ingestion/tests/ingester.rs`,
  `crates/ingestion/tests/runner.rs`, and the in-crate test modules
  (`chunkers/*.rs`, `types.rs`) that assert on `chunk.metadata` (read the bag
  instead of the typed fields, e.g. `chunk.metadata["section_title"]` instead of
  `chunk.metadata.extra.get("section_title")`).
- Do NOT touch: the NER provider logic (`crates/ingestion/src/ner/{regex,llm,composite}.rs`),
  `crates/db`, `crates/search`, `crates/mcp`, `Cargo.toml`, `Cargo.lock`, or
  `../synopsis`.

**Dependencies.** None (foundation task).

**Acceptance criteria.**
1. `DocumentChunk.metadata` is `Map<String, Value>`; `DocumentMetadata` still
   exists and is still the type of `Document.metadata`.
2. A sectioned Markdown chunk's bag carries the same chunk-specific keys it did
   before (`section_title`, `heading_level`, `breadcrumb`, `image_paths` where
   applicable); a test asserts the bag keys for a chunk under a heading
   hierarchy (e.g. `chunk.metadata["breadcrumb"]` is the heading path).
3. NER receives the bag (`&chunk.metadata`); the NER provider tests are
   unchanged and still pass (the keys NER sees are the same as before).
4. Chunk boundaries, `text`, `search_text`, and the byte-offset invariant
   (`content[start_offset..end_offset] == text`) are unchanged — the existing
   chunker invariant and `search_text` tests still pass.
5. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/ingestion/chunkers/markdown_chunker.go`
(`buildBreadcrumbs` + the chunk metadata keys), `../synopsis/internal/ingestion/
ingester.go` (the NER call passing the chunk metadata map).

## 2. Schema: `chunks.metadata_json` column (crates/db + init migration)

- [x] 2.1 Add the `metadata_json` column to `chunks` and carry it in `Chunk`/`ChunkDao`

**Goal.** Add a nullable `metadata_json TEXT` column to the `chunks` table
(storing the per-chunk metadata bag as raw JSON, following the
`documents.metadata_json` pattern) and make `Chunk`/`ChunkDao` carry it. The
column is folded into the single squashed init migration; `user_version` stays 1.

**File scope.**
- `migrations/knowledge/1-init/up.sql` — add `metadata_json TEXT` (nullable, no
  default) to the `chunks` table definition (next to `search_text`). Preserve the
  header/oracle-reference comments; do not renumber or add a new migration.
- `crates/db/src/chunk.rs` — the `Chunk` struct gains `metadata_json:
  Option<String>`; `SELECT_CHUNK` and `FTS_QUERY` select it; `row_to_chunk`
  reads it; `create_with_search_text` and `update` accept + store it; the test
  module is extended for the round-trip.
- Do NOT touch any other crate, `Cargo.toml`, `Cargo.lock`, or `../synopsis`.

**Dependencies.** None (logically before task 3).

**Acceptance criteria.**
1. `migrations/knowledge/1-init/up.sql` defines `chunks.metadata_json TEXT`
   (nullable); a fresh knowledge DB reports `PRAGMA user_version` = 1 and the
   `chunks` table has a `metadata_json` column.
2. `Chunk` exposes `metadata_json: Option<String>`; `SELECT_CHUNK`, `FTS_QUERY`,
   and `row_to_chunk` carry it; `get_by_id`/`list_by_doc_id`/`list_all`/
   `search_fts` all return it.
3. `create_with_search_text` and `update` accept a `metadata_json: Option<&str>`
   and store it; a unit test proves the round-trip: a chunk created with a
   `metadata_json` value returns it on read, and a chunk created without one
   returns `None`.
4. The existing `search_text` behavior is unchanged (the FTS index is still over
   `search_text`; the `fts_index_is_over_search_text` test still passes).
5. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/database/dao/chunk_dao.go` (ChunkDao
shape); the `documents.metadata_json` column in the same migration (the raw-JSON
`Option<String>` pattern to follow). The `metadata_json` column is the explicit,
justified deviation (design D1).

## 3. Persist: write the chunk's metadata bag (crates/ingestion + crates/db)

- [x] 3.1 The ingester serializes each chunk's metadata bag to `chunks.metadata_json`

**Goal.** The per-document pipeline serializes each chunk's metadata bag (now a
`Map<String, Value>` after task 1.1) to the `chunks.metadata_json` column (added
in task 2.1) and passes it through the chunk write. NER and the embedding input
are unchanged.

**File scope.**
- `crates/ingestion/src/ingester/mod.rs` — serialize `chunk.metadata` (the bag)
  to a JSON string and pass it to the chunk write (`create_with_search_text`),
  alongside `chunk.text` and `chunk.search_text`. A chunk with an empty bag
  stores `NULL`.
- `crates/db/src/chunk.rs` — only if the `create_with_search_text`/`update`
  signature from task 2.1 needs a parameter-ordering or defaulting adjustment to
  be ergonomic for the ingester (the column itself is already added there).
- `crates/ingestion/tests/ingester.rs` — an end-to-end test: ingest a Markdown
  document with a section and assert the persisted `metadata_json` contains the
  `breadcrumb`/`section_title` keys; a chunk with no section stores `NULL`.
- Do NOT touch any other crate, `Cargo.toml`, `Cargo.lock`, or `../synopsis`.

**Dependencies.** Task 1.1 (the bag type), Task 2.1 (the column + DAO write).

**Acceptance criteria.**
1. The ingester serializes `chunk.metadata` (the bag) to `metadata_json` and
   passes it to the chunk write; the document-level `doc.metadata.extra`
   serialization to `documents.metadata_json` is unchanged.
2. An end-to-end test ingests a Markdown document with a heading and asserts the
   persisted `chunks.metadata_json` for a sectioned chunk contains the
   `breadcrumb` (and `section_title`) keys.
3. A chunk with an empty metadata bag persists `NULL` `metadata_json`.
4. NER still receives the bag and the embedding input is still `search_text`
   (the task 1.1/`search_text` tests still pass).
5. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green.

**Oracle reference.** `../synopsis/internal/ingestion/ingester.go` (the per-chunk
write); `../synopsis/internal/database/dao/chunk_dao.go` (the write signature).

## 4. Search + MCP: result `text` = pure `chunk_text`, wire `metadata` (crates/search + crates/mcp + parity fixture)

- [x] 4.1 Result `text` is the pure `chunk_text`; the chunk's metadata bag is carried on the result and the MCP response

**Goal.** The fused, ranked search result's `text` field carries the chunk's pure
`chunk_text` (the byte-offset slice), not `search_text`; the section context
moves from the text into a structured `metadata` field. The chunk's metadata bag
(task 3.1) is carried on the result and exposed on the MCP `search` response.
The legs still *match* on `search_text` (the FTS index and the embeddings are
unchanged) — only the returned `text` and the new `metadata` field change.

**File scope.**
- `crates/search/src/lib.rs` — `LexicalHit`/`SemanticHit`/`SearchResult`: the
  `chunk_text` field now holds the pure `chunk_text` (update the doc comments);
  `SearchResult` gains a `chunk_metadata: Map<String, Value>` field (the chunk's
  own bag, distinct from the enrichment `metadata` bag).
- `crates/search/src/lexical.rs` — the `From<FtsHit> for LexicalHit` maps
  `chunk_text` from `chunk.chunk_text` (not `chunk.search_text`) and carries
  `metadata_json`; the crate's tests are updated (a seeded chunk with distinct
  `chunk_text`/`search_text` returns the pure `chunk_text` in the hit).
- `crates/search/src/semantic.rs` — same (the hit carries `chunk_text` = pure
  body + `metadata_json`).
- `crates/search/src/hybrid.rs` — the `RawHit` trait gains a `metadata_json()`
  accessor; `standalone_results` populates `SearchResult.chunk_metadata` from it
  (parsed).
- `crates/search/src/rrf.rs` — the RRF entry carries `chunk_metadata` (or
  `metadata_json`); the fused `SearchResult` gets `chunk_metadata` from the
  winning hit (both legs carry the same bag for the same `chunk_id`).
- `crates/search/src/{rerank,enrich,expand}.rs` — only if the new
  `chunk_metadata` field requires a constructor/default update (the enrichment
  `metadata` bag is untouched).
- `crates/mcp/src/tools/search.rs` — `ResultItem` gains a `metadata` field
  (`#[serde(skip_serializing_if = "Map::is_empty")]`) = `result.chunk_metadata`;
  `text` stays `result.chunk_text.clone()` (whose content is now the pure body).
- `crates/parity-harness/fixtures/content/search.json` — re-pin the `text` values
  to the pure body (the harness `normalize_search` already strips `text` and
  compares only `{document_id, chunk_id}` + `total_count`, so the rank assertions
  are unaffected; re-pin for cleanliness).
- Do NOT touch any other crate, `Cargo.toml`, `Cargo.lock`, or `../synopsis`.

**Dependencies.** Task 2.1 (the column exists to read); Task 3.1 (for end-to-end
data in the search/ingestion integration tests).

**Acceptance criteria.**
1. A search result's `text` field equals the matched chunk's `chunk_text` (pure
   body), verified by a unit test that seeds a chunk with distinct `chunk_text`
   and `search_text` and asserts the result `text` is `chunk_text` (not
   `search_text`).
2. A term present only in `search_text` (not in `chunk_text`) still matches (the
   FTS index is over `search_text`), and the returned `text` is the pure
   `chunk_text` — the external-content-index vs. returned-text compatibility is
   pinned by a test.
3. The result carries the chunk's metadata bag: a unit test seeds a chunk with a
   `metadata_json` and asserts the result's `chunk_metadata` is the parsed bag
   (and `NULL`/absent → an empty bag).
4. The MCP `search` result item has a `metadata` field = the chunk's bag (empty
   bags are omitted from the JSON via `skip_serializing_if`).
5. RRF fusion order, rerank, and the response field set (other than the new
   `metadata` and the `text` content) are unchanged — the existing search tests
   still pass, and the rank-order parity is unchanged.
6. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` all green; `cargo test -p parity-harness`
   passes (the `content_parity` search rank order is unchanged).

**Oracle reference.** `../synopsis/internal/search/search.go` +
`../synopsis/internal/search/enricher.go` (the `SearchResult` whose `text` is the
body and whose `Metadata` carries the section context);
`../synopsis/internal/mcp/handlers/search.go` (the wire mapping).
