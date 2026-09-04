# parsing-and-chunking Specification

## Purpose

Parsing of source documents: file discovery by extension, content extraction (markdown, json, mediawiki, webpage, unstructured), structure-aware chunking with offsets and metadata — the entry point of the ingestion pipeline.

## Requirements

### Requirement: Parsing traits

The `ingestion` crate defines the traits: `Parser` — walking a source path and extracting documents (the result contains the documents AND non-fatal parse errors together); `Chunker` — splitting content into chunks; `Source` — a composite Parser+Chunker for one source type (a "self-contained unit of ingestion"). A chunk carries text, a sequence number within the document, byte offsets of start/end in the source text, and metadata; NER results are not stored in the chunk (design: NER is attached by a separate stage).

#### Scenario: Source directory walk
- **WHEN** a parser is invoked on a source path
- **THEN** all documents of matching extensions are returned; errors of individual files are collected into the result without interrupting the walk

#### Scenario: Chunk carries offsets
- **WHEN** content is split into chunks
- **THEN** each chunk has a sequential sequence_num and byte offsets such that slicing the source text by them yields the chunk's text

### Requirement: Markdown source

The Markdown parser discovers `.md`/`.markdown` files and extracts their content. The Markdown chunker splits a document structure-aware — by heading sections — with a maximum chunk size and an overlap from the configuration (`chunking.markdown.max_chunk_size`, default 1000; `overlap_size`, default 100; a configured 0 for overlap is preserved). The chunk's metadata includes the section heading.

#### Scenario: Sectional chunking
- **WHEN** a Markdown document with several headings is split into chunks
- **THEN** chunk boundaries follow the heading structure; the size respects max_chunk_size; adjacent chunks overlap by overlap_size

#### Scenario: Zero overlap
- **WHEN** overlap_size is configured as 0
- **THEN** chunks do not overlap (0 is a valid value, not replaced by the default)

### Requirement: JSON source

The JSON parser handles `.json` files according to the fixed semantics (checked by tests). The JSON chunker splits the content by the document structure.

#### Scenario: Parsing a JSON source
- **WHEN** a JSON source is parsed and chunked
- **THEN** documents are extracted and the chunks cover the content without data loss

### Requirement: Additional formats

The mediawiki, webpage, and unstructured sources implement the same traits (each with its own format of discovery and extraction).

#### Scenario: Unified format pattern
- **WHEN** a source of a new format is added
- **THEN** it implements the same Source trait and is registered in the registry without changes to consuming code

### Requirement: Source registry

The registry maps a source type (from the `<source type=…>` in the global.xml configuration) and file extensions to a Source implementation; an unknown type is an explicit error.

#### Scenario: Source selection by type
- **WHEN** a source is requested by type from the configuration
- **THEN** the registered implementation is returned; an unknown type gives an explicit error

### Requirement: Parity with recorded fixtures

Parsing and chunking are verified against recorded fixtures: identical input gives the same number of chunks, identical chunk text, and consistent offsets.

#### Scenario: Differential test
- **WHEN** a recorded fixture is run through the Rust parser and chunker
- **THEN** the chunk count and their text match the expectations fixed from the recorded fixtures

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
