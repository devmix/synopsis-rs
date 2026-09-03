## ADDED Requirements

### Requirement: Chunk metadata is a free-form bag
A chunk's metadata SHALL be a free-form `Map<String, Value>` — the chunk's own metadata bag — not a clone of the document's typed `DocumentMetadata`. Each chunker SHALL build the bag directly, carrying the chunk-specific keys (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) plus any document-level keys it copies in. The document's typed fields (`source_type`, `source_file`, `file_size`, `modified_at`) SHALL NOT be part of the chunk's bag; they stay on the document. The bag SHALL carry the same chunk-specific keys the chunk carried before this change, and the chunk boundaries, `text`, `search_text`, and the byte-offset invariant are unchanged.

#### Scenario: Sectioned chunk bag
- **WHEN** a Markdown chunk is produced under a non-empty heading hierarchy
- **THEN** its metadata bag carries `section_title`, `heading_level`, and `breadcrumb` (the same keys the chunk carried before)

#### Scenario: Document typed fields not in the bag
- **WHEN** a chunk is produced
- **THEN** its metadata bag does not carry the document's typed fields (`source_type`, `source_file`, …); those remain on the document's metadata

#### Scenario: Invariant unaffected
- **WHEN** any chunk is produced
- **THEN** `content[start_offset..end_offset] == text` still holds and `search_text` is unchanged
