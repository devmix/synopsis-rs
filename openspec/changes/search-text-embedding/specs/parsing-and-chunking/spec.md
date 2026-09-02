## ADDED Requirements

### Requirement: search_text emission
The Markdown chunker emits, for every chunk, a `search_text` value in addition to the invariant-preserving `text`. `search_text` is the chunk's heading breadcrumb (the multi-line heading path, e.g. `> H1` / ` > H2`) followed by a blank line and the chunk body; when the chunk has no breadcrumb (e.g. the preamble before the first heading) `search_text` equals `text`. The `text` field and the byte-offset invariant (`content[start_offset..end_offset] == text`) are unchanged, and the breadcrumb is still also available in the chunk metadata.

#### Scenario: Chunk under a heading
- **WHEN** a Markdown chunk is produced under a non-empty heading hierarchy
- **THEN** its `search_text` is the breadcrumb followed by the body, while its `text` remains the pure body slice

#### Scenario: Chunk with no heading
- **WHEN** a Markdown chunk has no heading breadcrumb
- **THEN** its `search_text` is equal to its `text`

#### Scenario: Invariant unaffected
- **WHEN** any chunk is produced
- **THEN** `content[start_offset..end_offset] == text` still holds (only `search_text` carries the synthetic breadcrumb)
