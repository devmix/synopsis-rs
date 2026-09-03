## ADDED Requirements

### Requirement: search result carries document freshness
The `search` tool's result item SHALL carry an `updated_at` field holding the
owning document's `updated_at` normalized to RFC3339 (the value the enricher
already computes into the result's enrichment bag). The field SHALL be omitted
from the JSON when the document has no parseable timestamp. This is a
deliberate, additive divergence from the Go oracle's MCP wire item
(`../synopsis/internal/mcp/handlers/search.go`), which does not expose
`updated_at` per result: a RAG client can now see hit freshness without a
second `get_document_context` call. It is distinct from the chunk's own
`metadata` bag — `updated_at` is a top-level result field, not a bag key. The
parity harness strips `updated_at` from the `search` response before comparison
(it is a non-deterministic timestamp), so content parity is unaffected.

#### Scenario: Document with a parseable timestamp
- **WHEN** a search returns a chunk whose document has a parseable `updated_at`
- **THEN** the result item's `updated_at` is that timestamp in RFC3339 form

#### Scenario: Document with no parseable timestamp
- **WHEN** a search returns a chunk whose document has no (or unparseable) `updated_at`
- **THEN** the result item has no `updated_at` field (omitted, not `null`)

## MODIFIED Requirements

### Requirement: search result carries chunk metadata
The `search` tool's result item SHALL carry a `metadata` field holding the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) as a raw JSON object — the Go oracle's `SearchResult.Metadata`, which the Rust port previously dropped. The bag SHALL be passed through uncured; an empty bag SHALL be omitted from the JSON. The `text` field's type is unchanged (a string) — only its content is now the chunk's pure body (`chunk_text`) rather than the breadcrumb-prefixed `search_text`. Of the Go oracle's *document-level* keys in `SearchResult.Metadata`, `updated_at` is now exposed as a top-level result field (see "search result carries document freshness"); the remaining keys (`document_source_type`, `document_metadata_json`) are still a deferred concern and SHALL NOT be part of the `metadata` bag.

#### Scenario: Sectioned chunk metadata in the response
- **WHEN** a search returns a chunk produced under a heading hierarchy
- **THEN** the result item's `metadata` object carries `section_title`, `heading_level`, and `breadcrumb`

#### Scenario: Empty bag omitted
- **WHEN** a search returns a chunk with no chunk-specific metadata
- **THEN** the result item has no `metadata` field (omitted, not `{}`)

#### Scenario: text is the pure body
- **WHEN** a search returns a chunk
- **THEN** the result item's `text` is the chunk's pure body (the byte-offset slice), and the section context is in `metadata`, not glued into `text`
