//! Core ingestion types and traits (design D1/D2).
//!
//! Document metadata is a typed [`DocumentMetadata`] with an extension bag
//! (replacing an untyped free-form map); the chunk keeps its own free-form
//! [`Map<String, Value>`] bag, and carries no NER result (design D2).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::IngestionError;

/// A parsed document: the raw text extracted from one source file plus typed
/// metadata about its origin.
#[derive(Debug, Clone, Default)]
pub struct Document {
    /// Filesystem path to the original file (as walked by the parser).
    pub source_path: PathBuf,
    /// Raw text content extracted from the file (UTF-8).
    pub content: String,
    /// Typed metadata about the document's origin.
    pub metadata: DocumentMetadata,
}

/// Result of a parser walk: the documents found plus all non-fatal errors
/// encountered along the way.
///
/// Design (D1): parsing is best-effort — a broken file does not abort the
/// walk; its error is collected here alongside the documents so the pipeline
/// can report partial results.
///
/// Not `Clone`: `IngestionError` carries non-cloneable parser errors
/// (`serde_json::Error`); the result is consumed by the pipeline.
#[derive(Debug, Default)]
pub struct ParseResult {
    /// Documents successfully parsed.
    pub documents: Vec<Document>,
    /// Non-fatal errors encountered during parsing.
    pub errors: Vec<IngestionError>,
}

/// Typed metadata attached to a [`Document`].
///
/// Design (D1): the fields every parser fills are typed here, and the
/// format-specific keys (`structure`, `title`, `url`, `graph_relations`, …)
/// live in [`extra`](Self::extra). The chunk-specific keys
/// (`section_title`, `heading_level`, `breadcrumb`, `image_paths`) are
/// produced by the chunkers and live in each chunk's own
/// [`metadata`](DocumentChunk::metadata) bag.
///
/// `extra` is a [`serde_json::Map`], i.e. a `BTreeMap`-backed key-sorted map
/// (unless serde_json is built with `preserve_order`): deterministic ordering,
/// in contrast to the random iteration order of a plain `HashMap`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentMetadata {
    /// Source type name as registered in the source registry (the `type`
    /// attribute of the `<source>` element in `global.xml`), e.g. `"markdown"`.
    pub source_type: String,
    /// Source file path relative to the walk root.
    pub source_file: String,
    /// File size in bytes, if known.
    pub file_size: Option<u64>,
    /// File modification time as an RFC 3339 string, if known.
    pub modified_at: Option<String>,
    /// Format-specific extension fields (see the type docs for the key
    /// vocabulary).
    pub extra: Map<String, Value>,
}

/// A chunk of a document's content.
///
/// Design (D2): no `NerResult` field — the NER stage (series change 2)
/// attaches its results through its own structure keyed by chunk index,
/// keeping the chunk a pure chunking artifact.
///
/// **Byte-offset invariant.** `start_offset`/`end_offset` are *byte* offsets
/// into the original document `content` (UTF-8, not character positions), and
/// every chunk produced by this crate satisfies
/// `content[start_offset..end_offset] == text`: the chunk text is always a
/// pure slice of the source, and the offsets always land on character
/// boundaries (they derive from line/character boundaries). Decorative context
/// (section trimming, breadcrumb/file-name prefixes) is deliberately kept out
/// of `text` — it lives in the
/// [`metadata`](Self::metadata) bag — so the invariant always holds.
///
/// [`search_text`](Self::search_text) is the **only synthetic field** on the
/// chunk: it may carry the section's heading breadcrumb prefixed to `text`
/// (the context the FTS5 index and the embedding leg operate on), and is
/// therefore NOT bound by the byte-offset invariant. Chunkers with no section
/// context set it equal to `text`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentChunk {
    /// Database id of the parent document; `None` until the write stage
    /// (series change 3) assigns it.
    pub doc_id: Option<i64>,
    /// Chunk content — a pure slice of the source (see the invariant above).
    pub text: String,
    /// The text the FTS5 index and the embedding leg operate on:
    /// `breadcrumb + "\n\n" + text` for sectioned chunks, equal to `text`
    /// otherwise (search-text-embedding design D1). The only synthetic field
    /// — not bound by the byte-offset invariant (see the type docs).
    pub search_text: String,
    /// Position of this chunk within its document (0-based).
    pub sequence_num: usize,
    /// Byte offset of the chunk's first byte in the original content.
    pub start_offset: usize,
    /// Byte offset one past the chunk's last byte in the original content.
    pub end_offset: usize,
    /// The chunk's own free-form metadata bag: the originating document's
    /// [`extra`](DocumentMetadata::extra) keys plus the chunk-specific keys
    /// the chunker adds (`section_title`, `heading_level`, `breadcrumb`,
    /// `image_paths`, …). The document's typed fields are not part of the
    /// bag — they stay on [`DocumentMetadata`].
    pub metadata: Map<String, Value>,
}

impl DocumentChunk {
    /// Returns this chunk's span as a slice of the original content.
    ///
    /// Panics if the offsets are not valid character-boundary byte offsets of
    /// `content` — that is an internal invariant violation (see the type
    /// docs), not a caller error.
    pub fn source_slice<'a>(&'a self, content: &'a str) -> &'a str {
        &content[self.start_offset..self.end_offset]
    }
}

/// Parses a source path into [`Document`]s.
///
/// Object-safe: the source registry (task 1.6) stores implementations as
/// `Box<dyn Parser>` / `Box<dyn Source>`.
pub trait Parser {
    /// Walks `source_path` and returns the documents found.
    ///
    /// Best-effort: per-file failures are collected in
    /// [`ParseResult::errors`](ParseResult::errors), never returned as a hard
    /// error.
    fn parse(&self, source_path: &Path) -> ParseResult;

    /// Reads and parses the single file at `path`; never walks the source
    /// tree. `root` is the source root used to compute the relative
    /// `source_file` metadata.
    ///
    /// # Errors
    ///
    /// An [`IngestionError`] for I/O failures, format errors, or a `path`
    /// extension this parser does not handle.
    fn parse_file(&self, path: &Path, root: &Path) -> Result<Document, IngestionError>;

    /// File extensions this parser handles, including the leading dot
    /// (e.g. `".md"`, `".json"`).
    fn supported_extensions(&self) -> &[&str];
}

/// Splits document content into [`DocumentChunk`]s.
///
/// Chunking configuration (strategy, max/overlap sizes) is injected at
/// construction time; [`chunk`](Self::chunk) receives only the content and
/// the document's metadata, from which each produced chunk builds its own
/// [`metadata`](DocumentChunk::metadata) bag.
///
/// Object-safe: see [`Parser`].
pub trait Chunker {
    /// Splits `content` into ordered chunks.
    ///
    /// `metadata` is the originating document's metadata; each returned
    /// chunk's bag is built from the document's `extra` keys plus the
    /// chunker's chunk-specific keys.
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError>;
}

/// A self-sufficient ingestion unit for one source type: one parser plus its
/// chunker (design D1).
///
/// Object-safe composite: `Box<dyn Source>` is the registry's storage type.
pub trait Source: Parser + Chunker {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Minimal in-memory source proving the trait objects work end to end.
    struct StubSource;

    impl Parser for StubSource {
        fn parse(&self, _source_path: &Path) -> ParseResult {
            ParseResult::default()
        }

        fn parse_file(&self, _path: &Path, _root: &Path) -> Result<Document, IngestionError> {
            // The stub parses nothing: every single-file read is unsupported.
            Err(IngestionError::UnsupportedExtension(".stub".to_owned()))
        }

        fn supported_extensions(&self) -> &[&str] {
            &[".stub"]
        }
    }

    impl Chunker for StubSource {
        fn chunk(
            &self,
            content: &str,
            metadata: &DocumentMetadata,
        ) -> Result<Vec<DocumentChunk>, IngestionError> {
            Ok(vec![DocumentChunk {
                text: content.to_owned(),
                // No section context: search_text defaults to the text.
                search_text: content.to_owned(),
                sequence_num: 0,
                start_offset: 0,
                end_offset: content.len(),
                // The stub adds no chunk-specific keys: the bag is the
                // document's `extra` as-is.
                metadata: metadata.extra.clone(),
                ..Default::default()
            }])
        }
    }

    impl Source for StubSource {}

    #[test]
    fn source_is_object_safe() {
        let stub = StubSource;
        // Both the fat-pointer and the boxed form used by the registry (task 1.6).
        let dyn_source: &dyn Source = &stub;
        assert_eq!(dyn_source.supported_extensions(), [".stub"]);
        assert!(
            dyn_source
                .parse(Path::new("/nonexistent"))
                .documents
                .is_empty()
        );

        let boxed: Box<dyn Source> = Box::new(StubSource);
        let chunks = boxed
            .chunk("hello", &DocumentMetadata::default())
            .expect("stub always succeeds");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello");
        assert_eq!(chunks[0].doc_id, None);
    }

    #[test]
    fn chunk_offsets_are_byte_offsets_into_utf8_content() {
        // Every Cyrillic letter is 2 bytes in UTF-8, so byte and character
        // offsets diverge — the invariant must hold in bytes.
        let content = "Первый заголовок\n\nСодержание второго раздела";
        let prefix = "Первый заголовок\n\n";
        let chunk = DocumentChunk {
            text: content[prefix.len()..].to_owned(),
            sequence_num: 1,
            start_offset: prefix.len(),
            end_offset: content.len(),
            ..Default::default()
        };

        // The invariant: the byte-offset slice of the source reproduces the
        // chunk text exactly.
        assert_eq!(&content[chunk.start_offset..chunk.end_offset], chunk.text);
        assert_eq!(chunk.source_slice(content), chunk.text);

        // Byte offsets are NOT character offsets for multi-byte content.
        let prefix_chars = content[..chunk.start_offset].chars().count();
        assert_ne!(chunk.start_offset, prefix_chars);
        // "Первый заголовок": 15 Cyrillic letters × 2 bytes + 1 space + 2 newlines.
        assert_eq!(chunk.start_offset, 15 * 2 + 1 + 2);
        assert_eq!(prefix_chars, 15 + 1 + 2);
    }

    #[test]
    fn types_construct_and_carry_metadata() {
        let mut metadata = DocumentMetadata {
            source_type: "markdown".to_owned(),
            source_file: "docs/readme.md".to_owned(),
            file_size: Some(42),
            modified_at: Some("2026-08-23T00:00:00Z".to_owned()),
            ..Default::default()
        };
        metadata.extra.insert(
            "image_paths".to_owned(),
            serde_json::json!(["a.png", "b.png"]),
        );

        let document = Document {
            source_path: PathBuf::from("/data/docs/readme.md"),
            content: "# Title\n\nBody".to_owned(),
            metadata: metadata.clone(),
        };

        let result = ParseResult {
            documents: vec![document.clone()],
            errors: vec![IngestionError::UnknownSourceType("xml".to_owned())],
        };
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(document.metadata, metadata);
        assert_eq!(
            document
                .metadata
                .extra
                .get("image_paths")
                .map(|v| v.as_array().map(|a| a.len())),
            Some(Some(2))
        );

        let chunk = DocumentChunk {
            doc_id: None, // assigned by the write stage (series change 3)
            text: "Title".to_owned(),
            // No section context in this fixture: search_text equals text.
            search_text: "Title".to_owned(),
            sequence_num: 0,
            start_offset: 2,
            end_offset: 7,
            // The chunk's own bag: the document's `extra` keys, no typed
            // fields.
            metadata: document.metadata.extra.clone(),
        };
        assert_eq!(chunk.source_slice(&document.content), "Title");
        assert_eq!(
            chunk
                .metadata
                .get("image_paths")
                .map(|v| v.as_array().map(|a| a.len())),
            Some(Some(2))
        );
    }

    #[test]
    fn parse_result_defaults_to_empty() {
        let result = ParseResult::default();
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }
}
