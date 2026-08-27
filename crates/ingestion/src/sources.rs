//! Format composites and the source registry
//! (oracle: `internal/ingestion/sources/`).
//!
//! A [`Source`] is the self-sufficient ingestion unit for one source type:
//! one parser plus its chunker (design D1). The format composites
//! ([`MarkdownSource`], [`JsonSource`], [`MediawikiSource`],
//! [`WebpageSource`], [`UnstructuredSource`]) wire a crate parser to an
//! injected chunker — the oracle's sources do the same. The unstructured
//! format is the only composite with *two* chunkers: it parses both
//! Markdown and JSON files and routes chunking on the document's
//! `source_type` (oracle `UnstructuredSource`, cf. the oracle's
//! `registry_test.go`). Mediawiki is the one deliberate deviation:
//! the oracle injected the *markdown* chunker as a "graceful degradation"
//! that never matched wikitext headings; the Rust pipeline injects the
//! dedicated [`MediawikiChunker`](crate::chunkers::mediawiki::MediawikiChunker)
//! instead (task 1.7).
//!
//! [`Registry`] maps the `type` attribute word of a `global.xml` `<source>`
//! element to its implementation (design D5, oracle `sources.Registry`).
//! Looking up an unknown type is an explicit
//! [`IngestionError::UnknownSourceType`] — never a silent skip (the oracle's
//! runner failed with `no source for type %q` in the same case).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::error::IngestionError;
use crate::parsers::json::JsonParser;
use crate::parsers::markdown::MarkdownParser;
use crate::parsers::mediawiki::MediawikiParser;
use crate::parsers::unstructured::UnstructuredParser;
use crate::parsers::webpage::WebpageParser;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};

/// Markdown source: [`MarkdownParser`] plus an injected Markdown chunker
/// (oracle `sources.MarkdownSource`).
///
/// The parser is a stateless unit struct, so the composite holds only the
/// chunker. The chunker arrives as `Box<dyn Chunker>` (oracle: an interface
/// parameter) so the pipeline (series change 3) can inject whatever the
/// config selects, and later formats that reuse the markdown chunker
/// (webpage) keep the same composition.
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

/// Mediawiki source: [`MediawikiParser`] plus an injected chunker (oracle
/// `sources.MediawikiSource`).
///
/// Deliberate deviation (task 1.7): the oracle injected the *markdown*
/// chunker as a "graceful degradation" that never matched wikitext headings,
/// collapsing heading-rich pages into one unsplit chunk. The Rust pipeline
/// injects the dedicated
/// [`MediawikiChunker`](crate::chunkers::mediawiki::MediawikiChunker)
/// instead; the composite shape (parser + `Box<dyn Chunker>`) is identical.
pub struct MediawikiSource {
    chunker: Box<dyn Chunker>,
}

impl MediawikiSource {
    /// Registry key: the `type` attribute word in `global.xml`
    /// (`config::ontology::SourceType::Mediawiki`).
    pub const SOURCE_TYPE: &'static str = "mediawiki";

    /// Creates a mediawiki source over the given chunker.
    pub fn new(chunker: Box<dyn Chunker>) -> Self {
        Self { chunker }
    }
}

impl Parser for MediawikiSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        MediawikiParser.parse(source_path)
    }

    fn supported_extensions(&self) -> &[&str] {
        MediawikiParser.supported_extensions()
    }
}

impl Chunker for MediawikiSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        self.chunker.chunk(content, metadata)
    }
}

impl Source for MediawikiSource {}

/// Webpage source: [`WebpageParser`] plus an injected Markdown chunker
/// (oracle `sources.WebpageSource`).
///
/// The oracle injects the *markdown* chunker, and so does the Rust
/// pipeline: the parser emits Markdown either way (raw `.md` pages or
/// `.html` pages converted to Markdown), so the markdown chunker's
/// ATX-heading structure-awareness applies to both page kinds. There is no
/// dedicated webpage chunker (the oracle has none either).
pub struct WebpageSource {
    chunker: Box<dyn Chunker>,
}

impl WebpageSource {
    /// Registry key: the `type` attribute word in `global.xml`
    /// (`config::ontology::SourceType::Webpages`).
    pub const SOURCE_TYPE: &'static str = "webpages";

    /// Creates a webpage source over the given (markdown) chunker.
    pub fn new(chunker: Box<dyn Chunker>) -> Self {
        Self { chunker }
    }
}

impl Parser for WebpageSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        WebpageParser.parse(source_path)
    }

    fn supported_extensions(&self) -> &[&str] {
        WebpageParser.supported_extensions()
    }
}

impl Chunker for WebpageSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        self.chunker.chunk(content, metadata)
    }
}

impl Source for WebpageSource {}

/// Unstructured source: [`UnstructuredParser`] (Markdown + JSON files) plus
/// two injected chunkers routed by the document's `source_type` (oracle
/// `sources.UnstructuredSource`).
///
/// The oracle composes an `UnstructuredParser` (`.md`) and a `JSONParser`
/// (`.json`) and routes chunking on `metadata["source_type"]`:
/// `"unstructured"` → the markdown chunker, `"json"` → the JSON chunker,
/// anything else is an explicit error (fail loud). The Rust composite keeps
/// that routing contract; the parse side is a single shared walk (see the
/// parser module docs) instead of the oracle's two walks.
pub struct UnstructuredSource {
    md_chunker: Box<dyn Chunker>,
    json_chunker: Box<dyn Chunker>,
}

impl UnstructuredSource {
    /// Registry key: the `type` attribute word in `global.xml`
    /// (`config::ontology::SourceType::Unstructured`).
    pub const SOURCE_TYPE: &'static str = "unstructured";

    /// Creates an unstructured source over the given markdown and JSON
    /// chunkers.
    pub fn new(md_chunker: Box<dyn Chunker>, json_chunker: Box<dyn Chunker>) -> Self {
        Self {
            md_chunker,
            json_chunker,
        }
    }
}

impl Parser for UnstructuredSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        UnstructuredParser.parse(source_path)
    }

    fn supported_extensions(&self) -> &[&str] {
        UnstructuredParser.supported_extensions()
    }
}

impl Chunker for UnstructuredSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        // Oracle `Chunk`: the routing key is the document's source type —
        // set by the parser, never guessed.
        match metadata.source_type.as_str() {
            Self::SOURCE_TYPE => self.md_chunker.chunk(content, metadata),
            JsonSource::SOURCE_TYPE => self.json_chunker.chunk(content, metadata),
            other => Err(IngestionError::ChunkRouting(other.to_owned())),
        }
    }
}

impl Source for UnstructuredSource {}

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
    /// An unknown type is an explicit [`IngestionError::UnknownSourceType`]
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

    /// The union of the file extensions every registered source handles,
    /// deduplicated and sorted (deterministic), exactly as the parsers
    /// report them (leading dot, e.g. `".md"`).
    ///
    /// Consumers that filter filesystem events by extension (the CLI file
    /// watcher) derive their accept list from this instead of keeping a
    /// parallel hardcoded list that can drift from the pipeline.
    pub fn supported_extensions(&self) -> Vec<String> {
        let mut exts: BTreeSet<String> = BTreeSet::new();
        for source in self.sources.values() {
            for ext in source.supported_extensions() {
                exts.insert((*ext).to_owned());
            }
        }
        exts.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::{ChunkingStrategy, JsonChunkerConfig, MarkdownChunkerConfig};

    use super::*;
    use crate::chunkers::json::JsonChunker;
    use crate::chunkers::markdown::MarkdownChunker;
    use crate::chunkers::mediawiki::MediawikiChunker;
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

    /// A mediawiki composite with the dedicated wikitext chunker (task 1.7
    /// deviation from the oracle's markdown-chunker reuse).
    fn mediawiki_source() -> MediawikiSource {
        MediawikiSource::new(Box::new(MediawikiChunker::new(MarkdownChunkerConfig {
            strategy: ChunkingStrategy::Headers,
            max_chunk_size: 1000,
            overlap_size: 0,
            ..Default::default()
        })))
    }

    /// A webpage composite with the injected markdown chunker (the oracle
    /// wires the markdown chunker: the parser emits Markdown either way).
    fn webpage_source() -> WebpageSource {
        WebpageSource::new(Box::new(MarkdownChunker::new(MarkdownChunkerConfig {
            strategy: ChunkingStrategy::Headers,
            max_chunk_size: 1000,
            overlap_size: 0,
            ..Default::default()
        })))
    }

    /// An unstructured composite with the two injected chunkers the oracle
    /// wires (registry_test.go: markdown chunker + JSON chunker).
    fn unstructured_source() -> UnstructuredSource {
        UnstructuredSource::new(
            Box::new(MarkdownChunker::new(MarkdownChunkerConfig {
                strategy: ChunkingStrategy::Headers,
                max_chunk_size: 1000,
                overlap_size: 0,
                ..Default::default()
            })),
            Box::new(JsonChunker::new(JsonChunkerConfig::default())),
        )
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
        registry
            .register(MediawikiSource::SOURCE_TYPE, Box::new(mediawiki_source()))
            .unwrap();
        registry
            .register(WebpageSource::SOURCE_TYPE, Box::new(webpage_source()))
            .unwrap();
        registry
            .register(
                UnstructuredSource::SOURCE_TYPE,
                Box::new(unstructured_source()),
            )
            .unwrap();

        // The registry keys are exactly the `global.xml` `type` words.
        assert_eq!(MarkdownSource::SOURCE_TYPE, "markdown");
        assert_eq!(JsonSource::SOURCE_TYPE, "json");
        assert_eq!(MediawikiSource::SOURCE_TYPE, "mediawiki");
        assert_eq!(WebpageSource::SOURCE_TYPE, "webpages");
        assert_eq!(UnstructuredSource::SOURCE_TYPE, "unstructured");

        let markdown = registry.get("markdown").unwrap();
        assert_eq!(markdown.supported_extensions(), [".md", ".markdown"]);

        let json = registry.get("json").unwrap();
        assert_eq!(json.supported_extensions(), [".json"]);

        let mediawiki = registry.get("mediawiki").unwrap();
        assert_eq!(mediawiki.supported_extensions(), [".json"]);

        let webpages = registry.get("webpages").unwrap();
        assert_eq!(webpages.supported_extensions(), [".md", ".html"]);

        let unstructured = registry.get("unstructured").unwrap();
        assert_eq!(unstructured.supported_extensions(), [".md", ".json"]);

        // BTreeMap order: deterministic and sorted (the oracle iterated a Go
        // map in random order).
        assert_eq!(
            registry.types(),
            vec!["json", "markdown", "mediawiki", "unstructured", "webpages"]
        );
    }

    #[test]
    fn registry_supported_extensions_is_union_of_parsers() {
        let mut registry = Registry::new();
        registry
            .register(MarkdownSource::SOURCE_TYPE, Box::new(markdown_source()))
            .unwrap();
        registry
            .register(JsonSource::SOURCE_TYPE, Box::new(json_source()))
            .unwrap();
        registry
            .register(MediawikiSource::SOURCE_TYPE, Box::new(mediawiki_source()))
            .unwrap();
        registry
            .register(WebpageSource::SOURCE_TYPE, Box::new(webpage_source()))
            .unwrap();
        registry
            .register(
                UnstructuredSource::SOURCE_TYPE,
                Box::new(unstructured_source()),
            )
            .unwrap();

        // The union of the per-parser extension lists, deduplicated and
        // sorted (`.md`/`.json` are claimed by several parsers).
        assert_eq!(
            registry.supported_extensions(),
            vec![".html", ".json", ".markdown", ".md"]
        );

        // A partial registry reports only what is actually registered.
        let mut md_only = Registry::new();
        md_only
            .register(MarkdownSource::SOURCE_TYPE, Box::new(markdown_source()))
            .unwrap();
        // Lexicographic: `".markdown" < ".md"` (`a` < `d` after `".m"`).
        assert_eq!(md_only.supported_extensions(), vec![".markdown", ".md"]);

        // An empty registry reports no extensions.
        assert!(Registry::new().supported_extensions().is_empty());
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

    #[test]
    fn mediawiki_source_parses_and_chunks_end_to_end() {
        let tree = TempTree::new();
        tree.write(
            "space/wiki-type/graph.json",
            r#"{"API Gateway": ["Service Catalog"]}"#,
        );
        tree.write(
            "space/wiki-type/by-type/services/api_gateway.json",
            r#"{
                "title": "API Gateway",
                "url": "https://example.com/API_Gateway",
                "wikitext": "== API Gateway ==\nA service mesh component.\n\n== Config ==\nSettings live here.",
                "images": ["gateway.png"],
                "links": ["Service Catalog"],
                "categories": ["Services"]
            }"#,
        );

        let source: Box<dyn Source> = Box::new(mediawiki_source());
        let result = source.parse(&tree.0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let document = &result.documents[0];
        assert_eq!(document.metadata.source_type, MediawikiSource::SOURCE_TYPE);
        assert_eq!(
            document.metadata.source_file,
            "space/wiki-type/by-type/services/api_gateway.json"
        );
        assert_eq!(
            document.metadata.extra.get("graph_relations"),
            Some(&serde_json::Value::Array(vec![serde_json::Value::String(
                "Service Catalog".to_owned()
            )]))
        );

        // The composite chunks the wikitext through the injected chunker:
        // one chunk per `== section ==`.
        let chunks = source.chunk(&document.content, &document.metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract).
            assert_eq!(
                &document.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, MediawikiSource::SOURCE_TYPE);
        }
        assert_eq!(
            chunks
                .iter()
                .map(|c| {
                    c.metadata
                        .extra
                        .get("section_title")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>(),
            vec![Some("API Gateway"), Some("Config")]
        );
    }

    #[test]
    fn webpage_source_parses_and_chunks_end_to_end() {
        let tree = TempTree::new();
        tree.write(
            "pages/index.md",
            "# Home\n\nWelcome to the site.\n\n## Team\n\nThe people.",
        );
        tree.write(
            "pages/about.html",
            "<h1>About</h1><p>We build things.</p><h2>Team</h2><p>The people behind it.</p>",
        );
        tree.write("static/logo.png", "binary data");

        let source: Box<dyn Source> = Box::new(webpage_source());
        let result = source.parse(&tree.0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 2);

        // Both page kinds carry the webpage metadata.
        for document in &result.documents {
            assert_eq!(document.metadata.source_type, WebpageSource::SOURCE_TYPE);
            assert!(document.metadata.file_size.is_some());
            assert!(document.metadata.modified_at.is_some());
        }

        // The .html document's content is converted Markdown — the chunk
        // offsets are relative to it (never the raw HTML), and the injected
        // markdown chunker splits it on the converted ATX headings.
        let html_doc = result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == "pages/about.html")
            .unwrap();
        assert!(
            html_doc.content.contains("# About"),
            "converted content: {:?}",
            html_doc.content
        );

        let chunks = source.chunk(&html_doc.content, &html_doc.metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract), relative to the
            // converted content.
            assert_eq!(
                &html_doc.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, WebpageSource::SOURCE_TYPE);
        }
        assert_eq!(
            chunks
                .iter()
                .map(|c| {
                    c.metadata
                        .extra
                        .get("section_title")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>(),
            vec![Some("About"), Some("Team")]
        );
    }

    #[test]
    fn unstructured_source_parses_and_chunks_end_to_end() {
        let tree = TempTree::new();
        tree.write(
            "docs/article.md",
            "# Title\n\nBody of the first section.\n\n## More\n\nDeeper body.",
        );
        tree.write("docs/hero.png", "image data");
        tree.write(
            "data/items.json",
            r#"[{"id": 1, "title": "Alpha"}, {"id": 2, "title": "Beta"}]"#,
        );

        let source: Box<dyn Source> = Box::new(unstructured_source());
        let result = source.parse(&tree.0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 2);

        // The Markdown document carries the unstructured metadata, including
        // the images of its directory.
        let md_doc = result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == "docs/article.md")
            .unwrap();
        assert_eq!(md_doc.metadata.source_type, UnstructuredSource::SOURCE_TYPE);
        assert_eq!(
            md_doc
                .metadata
                .extra
                .get("image_paths")
                .and_then(serde_json::Value::as_array)
                .map(|images| images.len()),
            Some(1)
        );

        // The JSON document keeps the JSON parser's metadata.
        let json_doc = result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == "data/items.json")
            .unwrap();
        assert_eq!(json_doc.metadata.source_type, JsonSource::SOURCE_TYPE);

        // Routing: the Markdown document goes to the markdown chunker — one
        // chunk per section.
        let chunks = source.chunk(&md_doc.content, &md_doc.metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract).
            assert_eq!(
                &md_doc.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, UnstructuredSource::SOURCE_TYPE);
        }
        assert_eq!(
            chunks
                .iter()
                .map(|c| {
                    c.metadata
                        .extra
                        .get("section_title")
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>(),
            vec![Some("Title"), Some("More")]
        );

        // Routing: the JSON document goes to the JSON chunker — one `title`
        // chunk per array object.
        let chunks = source.chunk(&json_doc.content, &json_doc.metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.sequence_num, index);
            // Byte-offset invariant (crate contract).
            assert_eq!(
                &json_doc.content[chunk.start_offset..chunk.end_offset],
                chunk.text
            );
            assert_eq!(chunk.metadata.source_type, JsonSource::SOURCE_TYPE);
        }
    }

    #[test]
    fn unstructured_chunk_routing_rejects_unknown_source_type() {
        // Oracle `Chunk`: an unknown routing key is an explicit error, never
        // a guessed chunker.
        let source: Box<dyn Source> = Box::new(unstructured_source());
        let metadata = DocumentMetadata {
            source_type: "mediawiki".to_owned(),
            ..Default::default()
        };

        let error = match source.chunk("content", &metadata) {
            Err(error) => error,
            Ok(_) => panic!("unknown routing key must be an explicit error"),
        };
        assert!(
            matches!(
                error,
                IngestionError::ChunkRouting(ref word) if word == "mediawiki"
            ),
            "got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            r#"unknown source type "mediawiki" for unstructured chunk routing"#
        );
    }
}
