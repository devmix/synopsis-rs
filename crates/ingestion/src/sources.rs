//! Format composites and the source registry
//! (oracle: `internal/ingestion/sources/`).
//!
//! A [`Source`] is the self-sufficient ingestion unit for one source type:
//! one parser plus its chunker (design D1). The format composites
//! ([`MarkdownSource`], [`JsonSource`]) wire a crate parser to an injected
//! chunker — the oracle's `MarkdownSource`/`JsonSource` do the same, and the
//! remaining formats (mediawiki, webpage, unstructured; tasks 1.7–1.9) follow
//! this pattern (mediawiki and webpage reuse the markdown chunker,
//! unstructured both, cf. the oracle's `registry_test.go`).
//!
//! [`Registry`] maps the `type` attribute word of a `global.xml` `<source>`
//! element to its implementation (design D5, oracle `sources.Registry`).
//! Looking up an unknown type is an explicit
//! [`IngestionError::UnknownSourceType`](crate::IngestionError::UnknownSourceType)
//! — never a silent skip (the oracle's runner failed with
//! `no source for type %q` in the same case).

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::IngestionError;
use crate::parsers::json::JsonParser;
use crate::parsers::markdown::MarkdownParser;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};

/// Markdown source: [`MarkdownParser`] plus an injected Markdown chunker
/// (oracle `sources.MarkdownSource`).
///
/// The parser is a stateless unit struct, so the composite holds only the
/// chunker. The chunker arrives as `Box<dyn Chunker>` (oracle: an interface
/// parameter) so the pipeline (series change 3) can inject whatever the
/// config selects, and later formats that reuse the markdown chunker
/// (mediawiki, webpage) keep the same composition.
pub struct MarkdownSource {
    chunker: Box<dyn Chunker>,
}

impl MarkdownSource {
    /// Registry key: the `type` attribute word in `global.xml`
    /// (`config::ontology::SourceType::Markdown`).
    pub const SOURCE_TYPE: &'static str = "markdown";

    /// Creates a markdown source over the given chunker.
    pub fn new(chunker: Box<dyn Chunker>) -> Self {
        Self { chunker }
    }
}

impl Parser for MarkdownSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        MarkdownParser.parse(source_path)
    }

    fn supported_extensions(&self) -> &[&str] {
        MarkdownParser.supported_extensions()
    }
}

impl Chunker for MarkdownSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        self.chunker.chunk(content, metadata)
    }
}

impl Source for MarkdownSource {}

/// JSON source: [`JsonParser`] plus an injected JSON chunker
/// (oracle `sources.JsonSource`). See [`MarkdownSource`] for the composition
/// rationale.
pub struct JsonSource {
    chunker: Box<dyn Chunker>,
}

impl JsonSource {
    /// Registry key: the `type` attribute word in `global.xml` (the oracle
    /// registers `"json"` in its runner even though `global.xml` rarely uses
    /// the word; kept for parity of the registry surface).
    pub const SOURCE_TYPE: &'static str = "json";

    /// Creates a JSON source over the given chunker.
    pub fn new(chunker: Box<dyn Chunker>) -> Self {
        Self { chunker }
    }
}

impl Parser for JsonSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        JsonParser.parse(source_path)
    }

    fn supported_extensions(&self) -> &[&str] {
        JsonParser.supported_extensions()
    }
}

impl Chunker for JsonSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        self.chunker.chunk(content, metadata)
    }
}

impl Source for JsonSource {}

/// Maps a `global.xml` source-type word to its [`Source`] implementation
/// (oracle `sources.Registry`, design D5).
///
/// Built once at pipeline start (series change 3) with one entry per known
/// format, then queried for every configured `<source>` element. Unknown
/// types are an explicit [`IngestionError::UnknownSourceType`], never a
/// silent skip.
///
/// Storage is a [`BTreeMap`]: [`types`](Self::types) is sorted and therefore
/// deterministic — the oracle iterated a Go map in random order.
#[derive(Default)]
pub struct Registry {
    sources: BTreeMap<String, Box<dyn Source>>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `source` under `source_type` (the `global.xml` `type` word).
    ///
    /// Re-registering a known type is a programmer error surfaced as an
    /// explicit [`IngestionError::AlreadyRegistered`] (oracle contract:
    /// `Register` returns an error on duplicates).
    pub fn register(
        &mut self,
        source_type: impl Into<String>,
        source: Box<dyn Source>,
    ) -> Result<(), IngestionError> {
        let source_type = source_type.into();
        if self.sources.contains_key(&source_type) {
            return Err(IngestionError::AlreadyRegistered(source_type));
        }
        self.sources.insert(source_type, source);
        Ok(())
    }

    /// Returns the source registered under `source_type`.
    ///
    /// An unknown type is an explicit
    /// [`IngestionError::UnknownSourceType`](crate::IngestionError::UnknownSourceType)
    /// (design D5), never a silent skip.
    pub fn get(&self, source_type: &str) -> Result<&dyn Source, IngestionError> {
        self.sources
            .get(source_type)
            .map(|source| source.as_ref())
            .ok_or_else(|| IngestionError::UnknownSourceType(source_type.to_owned()))
    }

    /// All registered type names, sorted (deterministic; the oracle iterated
    /// a Go map in random order).
    pub fn types(&self) -> Vec<String> {
        self.sources.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::{ChunkingStrategy, JsonChunkerConfig, MarkdownChunkerConfig};

    use super::*;
    use crate::chunkers::json::JsonChunker;
    use crate::chunkers::markdown::MarkdownChunker;
    use crate::parsers::tests::TempTree;

    /// A markdown composite with the config-crate defaults plus an explicit
    /// strategy (the config loader normally normalizes the strategy; a
    /// directly constructed config must not rely on the chunker's tolerance).
    fn markdown_source() -> MarkdownSource {
        MarkdownSource::new(Box::new(MarkdownChunker::new(MarkdownChunkerConfig {
            strategy: ChunkingStrategy::Headers,
            max_chunk_size: 1000,
            overlap_size: 0,
            ..Default::default()
        })))
    }

    /// A JSON composite with the config-crate defaults (empty `text_fields`
    /// falls back to the oracle's four defaults inside the chunker).
    fn json_source() -> JsonSource {
        JsonSource::new(Box::new(JsonChunker::new(JsonChunkerConfig::default())))
    }

    #[test]
    fn registry_returns_registered_implementations() {
        let mut registry = Registry::new();
        registry
            .register(MarkdownSource::SOURCE_TYPE, Box::new(markdown_source()))
            .unwrap();
        registry
            .register(JsonSource::SOURCE_TYPE, Box::new(json_source()))
            .unwrap();

        // The registry keys are exactly the `global.xml` `type` words.
        assert_eq!(MarkdownSource::SOURCE_TYPE, "markdown");
        assert_eq!(JsonSource::SOURCE_TYPE, "json");

        let markdown = registry.get("markdown").unwrap();
        assert_eq!(markdown.supported_extensions(), [".md", ".markdown"]);

        let json = registry.get("json").unwrap();
        assert_eq!(json.supported_extensions(), [".json"]);

        // BTreeMap order: deterministic and sorted (the oracle iterated a Go
        // map in random order).
        assert_eq!(registry.types(), vec!["json", "markdown"]);
    }

    #[test]
    fn registry_unknown_type_is_an_explicit_error() {
        let mut registry = Registry::new();
        registry
            .register("markdown", Box::new(markdown_source()))
            .unwrap();

        // `dyn Source` is not `Debug`, so no `unwrap_err` — match instead.
        let error = match registry.get("nonexistent") {
            Err(error) => error,
            Ok(_) => panic!("unknown source type must be an explicit error"),
        };
        assert!(
            matches!(
                error,
                IngestionError::UnknownSourceType(ref word) if word == "nonexistent"
            ),
            "got {error:?}"
        );
        assert_eq!(error.to_string(), r#"unknown source type "nonexistent""#);
    }

    #[test]
    fn registry_rejects_duplicate_registration() {
        let mut registry = Registry::new();
        registry
            .register("markdown", Box::new(markdown_source()))
            .unwrap();

        let error = registry
            .register("markdown", Box::new(markdown_source()))
            .unwrap_err();
        assert!(
            matches!(
                error,
                IngestionError::AlreadyRegistered(ref word) if word == "markdown"
            ),
            "got {error:?}"
        );
        // The rejected re-registration leaves the original entry intact.
        assert!(registry.get("markdown").is_ok());
        assert_eq!(registry.types(), vec!["markdown"]);
    }

    #[test]
    fn markdown_source_parses_and_chunks_end_to_end() {
        let tree = TempTree::new();
        tree.write(
            "guide.md",
            "# Title\n\nBody of the first section.\n\n## More\n\nDeeper body.",
        );

        let source: Box<dyn Source> = Box::new(markdown_source());
        let result = source.parse(&tree.0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let document = &result.documents[0];
        assert_eq!(document.metadata.source_type, MarkdownSource::SOURCE_TYPE);
        assert_eq!(document.metadata.source_file, "guide.md");

        // The composite chunks through the injected chunker.
        let chunks = source.chunk(&document.content, &document.metadata).unwrap();
        // One chunk per section (no preamble: the file starts with a heading).
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract).
            assert_eq!(
                &document.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, MarkdownSource::SOURCE_TYPE);
        }
    }

    #[test]
    fn json_source_parses_and_chunks_end_to_end() {
        let tree = TempTree::new();
        tree.write(
            "items.json",
            r#"[{"id": 1, "title": "Alpha"}, {"id": 2, "title": "Beta"}]"#,
        );

        let source: Box<dyn Source> = Box::new(json_source());
        let result = source.parse(&tree.0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let document = &result.documents[0];
        assert_eq!(document.metadata.source_type, JsonSource::SOURCE_TYPE);
        assert_eq!(
            document
                .metadata
                .extra
                .get("structure")
                .and_then(serde_json::Value::as_str),
            Some("array")
        );

        // Per-field mode with the default field list: one `title` chunk per
        // array object.
        let chunks = source.chunk(&document.content, &document.metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract).
            assert_eq!(
                &document.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, JsonSource::SOURCE_TYPE);
        }
    }
}
