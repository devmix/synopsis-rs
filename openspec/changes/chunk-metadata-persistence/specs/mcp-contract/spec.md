## ADDED Requirements

### Requirement: search result carries chunk metadata
The `search` tool's result item SHALL carry a `metadata` field holding the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) as a raw JSON object — the Go oracle's `SearchResult.Metadata`, which the Rust port previously dropped. The bag SHALL be passed through uncured; an empty bag SHALL be omitted from the JSON. The `text` field's type is unchanged (a string) — only its content is now the chunk's pure body (`chunk_text`) rather than the breadcrumb-prefixed `search_text`. The Go oracle's *document-level* keys in `SearchResult.Metadata` (`document_source_type`, `updated_at`, `document_metadata_json`) are a separate, deferred concern and SHALL NOT be part of this field.

#### Scenario: Sectioned chunk metadata in the response
- **WHEN** a search returns a chunk produced under a heading hierarchy
- **THEN** the result item's `metadata` object carries `section_title`, `heading_level`, and `breadcrumb`

#### Scenario: Empty bag omitted
- **WHEN** a search returns a chunk with no chunk-specific metadata
- **THEN** the result item has no `metadata` field (omitted, not `{}`)

#### Scenario: text is the pure body
- **WHEN** a search returns a chunk
- **THEN** the result item's `text` is the chunk's pure body (the byte-offset slice), and the section context is in `metadata`, not glued into `text`
