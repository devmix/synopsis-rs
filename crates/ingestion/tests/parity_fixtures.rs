//! Unified parity suite (ingestion-sources task 1.10).
//!
//! One test per source format (markdown, json, mediawiki, webpage,
//! unstructured) runs the FULL pipeline through the public [`Source`]
//! composite — parse → chunk, exactly what the pipeline (series change 3)
//! will call — with fixtures and expectations pinned from the Go oracle's
//! tests; a final cross-cutting test re-runs all five formats and asserts
//! the crate's byte-offset invariant on every chunk.
//!
//! **Fixture provenance.** The oracle's ingestion tests are inline-only
//! (`t.TempDir()` + inline strings); the only on-disk testdata is
//! `internal/ingestion/runner/testdata/global.xml`, a runner *config*
//! fixture outside this crate's scope. The fixtures below are the oracle's
//! inline strings, copied verbatim from
//! `internal/ingestion/parsers/{markdown,json,mediawiki,webpage,unstructured}_parser_test.go`,
//! `internal/ingestion/chunkers/{markdown,json}_chunker_test.go` and
//! `internal/ingestion/runner/runner_test.go` (e2e JSON fixture).
//!
//! **Deliberate deviations pinned here** (documented in the module docs of
//! `src/parsers/*.rs` / `src/chunkers/*.rs`): where the oracle's behavior
//! conflicts with the crate's byte-offset invariant (`text` is a pure
//! slice of the content, offsets always valid), the span and the metadata
//! are asserted instead of the oracle's prefixed text: (1) the oracle
//! prefixed breadcrumbs / `**field**: value` labels / file names into
//! `Text`; (2) invalid JSON is a non-fatal parse-stage error (the oracle
//! ingested it with structure `"unknown"` and hard-errored in the chunker);
//! (3) the mediawiki source injects the dedicated `MediawikiChunker` (the
//! oracle's markdown chunker never matched wikitext headings); (4) JSON
//! scalars produce one chunk (the oracle hard-errored). Oracle-pinned
//! expectations that survive the deviations (chunk counts, section titles,
//! breadcrumbs, field routing, document counts) are asserted as-is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use config::preset::{ChunkingStrategy, JsonChunkerConfig, MarkdownChunkerConfig};
use ingestion::chunkers::json::JsonChunker;
use ingestion::chunkers::markdown::MarkdownChunker;
use ingestion::chunkers::mediawiki::MediawikiChunker;
use ingestion::sources::{
    JsonSource, MarkdownSource, MediawikiSource, UnstructuredSource, WebpageSource,
};
use ingestion::{Document, DocumentChunk, IngestionError, ParseResult, Source};
use serde_json::Value;

/// A temporary source tree that removes itself on drop (the `pub(crate)`
/// helper under `src/parsers` is not visible from integration tests).
struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "synopsis-parity-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create parity temp tree");
        Self(path)
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, content).expect("write fixture file");
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The section-aware chunker config the pipeline injects (config-crate
/// defaults plus an explicit strategy).
fn section_config() -> MarkdownChunkerConfig {
    MarkdownChunkerConfig {
        strategy: ChunkingStrategy::Headers,
        max_chunk_size: 1000,
        overlap_size: 0,
        ..Default::default()
    }
}

fn markdown_source() -> MarkdownSource {
    MarkdownSource::new(Box::new(MarkdownChunker::new(section_config())))
}

fn json_source(config: JsonChunkerConfig) -> JsonSource {
    JsonSource::new(Box::new(JsonChunker::new(config)))
}

fn mediawiki_source() -> MediawikiSource {
    MediawikiSource::new(Box::new(MediawikiChunker::new(section_config())))
}

fn webpage_source() -> WebpageSource {
    WebpageSource::new(Box::new(MarkdownChunker::new(section_config())))
}

fn unstructured_source() -> UnstructuredSource {
    UnstructuredSource::new(
        Box::new(MarkdownChunker::new(section_config())),
        Box::new(JsonChunker::new(JsonChunkerConfig::default())),
    )
}

/// The document of `result` whose `source_file` is `file`.
fn document<'a>(result: &'a ParseResult, file: &str) -> &'a Document {
    result
        .documents
        .iter()
        .find(|doc| doc.metadata.source_file == file)
        .unwrap_or_else(|| panic!("no document for {file}"))
}

/// The `source_file` of every document, in document order.
fn source_files(result: &ParseResult) -> Vec<String> {
    result
        .documents
        .iter()
        .map(|doc| doc.metadata.source_file.clone())
        .collect()
}

/// A string extra of the metadata, if present.
fn extra_str<'a>(metadata: &'a ingestion::DocumentMetadata, key: &str) -> Option<&'a str> {
    metadata.extra.get(key).and_then(Value::as_str)
}

/// A string-array extra of the metadata (empty when absent).
fn extra_strings(metadata: &ingestion::DocumentMetadata, key: &str) -> Vec<String> {
    metadata
        .extra
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The `section_title` extra of every chunk, in chunk order.
fn section_titles(chunks: &[DocumentChunk]) -> Vec<Option<&str>> {
    chunks
        .iter()
        .map(|chunk| extra_str(&chunk.metadata, "section_title"))
        .collect()
}

/// The `breadcrumb` extra of every chunk, in chunk order.
fn breadcrumbs(chunks: &[DocumentChunk]) -> Vec<Option<&str>> {
    chunks
        .iter()
        .map(|chunk| extra_str(&chunk.metadata, "breadcrumb"))
        .collect()
}

/// Crate invariant: every chunk is a pure byte-offset slice of `content`,
/// numbered consecutively, with no document id yet.
fn assert_invariant(content: &str, chunks: &[DocumentChunk]) {
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(
            &content[chunk.start_offset..chunk.end_offset],
            chunk.text,
            "chunk {index} must be a pure slice of the content"
        );
        assert_eq!(chunk.sequence_num, index, "chunk {index} sequence");
        assert_eq!(chunk.doc_id, None, "chunk {index} doc_id");
    }
}

/// Full pipeline over `tree`: parse (asserting the expected error shape)
/// and chunk every document, asserting the invariant on every chunk.
/// Returns the total chunk count.
fn pipeline(source: &dyn Source, tree: &TempTree, expect_errors: bool) -> usize {
    let result = source.parse(&tree.0);
    assert_eq!(
        result.errors.is_empty(),
        !expect_errors,
        "errors: {:?}",
        result.errors
    );
    let mut total = 0;
    for doc in &result.documents {
        let chunks = source.chunk(&doc.content, &doc.metadata).unwrap();
        assert_invariant(&doc.content, &chunks);
        total += chunks.len();
    }
    total
}

/// Oracle `markdown_parser_test.go` "multiple md files" (b.md, c/nested.md)
/// plus the `markdown_chunker_test.go` `TestMarkdownChunker_Breadcrumbs`
/// "two-level hierarchy" content for the chunking half (a.md).
fn markdown_tree() -> TempTree {
    let tree = TempTree::new();
    tree.write(
        "a.md",
        "# A\n\n## A.1\ntext under a1\n\n## A.2\ntext under a2",
    );
    tree.write("b.md", "# B");
    tree.write("c/nested.md", "# C Nested");
    tree
}

/// Oracle `json_parser_test.go` "single json file" / "array json file"
/// plus `json_chunker_test.go` `TestJSONChunker_ChunkObject` content and
/// `TestJSONChunker_InvalidJSON` input.
fn json_tree() -> TempTree {
    let tree = TempTree::new();
    tree.write(
        "data.json",
        r#"{"title": "Page Title", "description": "Full description here."}"#,
    );
    tree.write("items.json", r#"[{"id": 1}, {"id": 2}]"#);
    tree.write("bad.json", "{invalid json}");
    tree
}

/// Oracle `mediawiki_parser_test.go` "single page" (rich fields),
/// "skip graph.json" and `TestMediawikiParser_GraphJSON` (the graph fixture
/// keyed by page title).
fn mediawiki_tree() -> TempTree {
    let tree = TempTree::new();
    tree.write(
        "space/wiki-type/graph.json",
        r#"{
            "API Gateway": ["Service Catalog", "Load Balancer"],
            "Database": ["Storage"]
        }"#,
    );
    tree.write(
        "space/wiki-type/by-type/services/api_gateway.json",
        r#"{
            "title": "API Gateway",
            "url": "https://example.com/API_Gateway",
            "wikitext": "== API Gateway ==\nA service mesh component.",
            "html": "<div>HTML content</div>",
            "images": ["gateway.png"],
            "links": ["Service Catalog"],
            "categories": ["Services", "Networking"],
            "entity_type": "service"
        }"#,
    );
    tree.write(
        "space/wiki-type/by-type/systems/database.json",
        r#"{
            "title": "Database",
            "wikitext": "A data storage system."
        }"#,
    );
    tree
}

/// Oracle `webpage_parser_test.go` "mixed md and html pages",
/// "md preferred over html for same page name" and "static directory
/// excluded".
fn webpage_tree() -> TempTree {
    let tree = TempTree::new();
    tree.write("pages/home.md", "# Home MD");
    tree.write("pages/pricing.html", "<h1>Pricing</h1><p>Plans below.</p>");
    tree.write("pages/docs.md", "# Docs\nAPI reference.");
    tree.write("pages/page-1.md", "# MD Content\nThis should win.");
    tree.write(
        "pages/page-1.html",
        "<h1>HTML Content</h1><p>This should lose.</p>",
    );
    tree.write("static/logo.png", "binary data");
    tree
}

/// Oracle `unstructured_parser_test.go` "single md file" (readme.md) and
/// `TestUnstructuredParser_ImageAssociation` (article.md + images) plus the
/// `runner_test.go` e2e JSON fixture (policies.json).
fn unstructured_tree() -> TempTree {
    let tree = TempTree::new();
    tree.write("docs/readme.md", "# Hello\nSome text.");
    tree.write("docs/article.md", "# Article\n![hero](banner.png)");
    tree.write("docs/banner.png", "image data");
    tree.write("docs/logo.svg", "<svg/>");
    tree.write(
        "data/policies.json",
        r#"[{"title":"NDA Policy","description":"Confidentiality agreement"}]"#,
    );
    tree
}

#[test]
fn markdown_pipeline_matches_oracle() {
    let tree = markdown_tree();
    let source: Box<dyn Source> = Box::new(markdown_source());

    let result = source.parse(&tree.0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    // Oracle "multiple md files": one document per .md file, nested dirs
    // walked.
    assert_eq!(source_files(&result), vec!["a.md", "b.md", "c/nested.md"]);
    for doc in &result.documents {
        assert_eq!(doc.metadata.source_type, MarkdownSource::SOURCE_TYPE);
    }

    // Oracle TestMarkdownChunker_Breadcrumbs "two-level hierarchy" (a.md):
    // 2 chunks with breadcrumbs "> A\n > A.1" / "> A\n > A.2" (the same
    // expectations are pinned by TestMarkdownChunker_AcceptanceCriteria).
    // Deviation: the oracle prefixes the breadcrumb into Text; here the text
    // is a pure slice and the breadcrumb lives in the metadata.
    let a = document(&result, "a.md");
    let chunks = source.chunk(&a.content, &a.metadata).unwrap();
    assert_invariant(&a.content, &chunks);
    assert_eq!(chunks.len(), 2);
    assert_eq!(section_titles(&chunks), vec![Some("A.1"), Some("A.2")]);
    assert_eq!(
        breadcrumbs(&chunks),
        vec![Some("> A\n > A.1"), Some("> A\n > A.2")]
    );
    assert_eq!(chunks[0].text, "## A.1\ntext under a1\n\n");
    assert_eq!(chunks[1].text, "## A.2\ntext under a2");

    // Oracle TestMarkdownChunker_HeaderOnlySkip (b.md, c/nested.md):
    // header-only documents produce no chunks.
    for file in ["b.md", "c/nested.md"] {
        let doc = document(&result, file);
        let chunks = source.chunk(&doc.content, &doc.metadata).unwrap();
        assert!(chunks.is_empty(), "{file}: {chunks:?}");
    }
}

#[test]
fn json_pipeline_matches_oracle() {
    let tree = json_tree();
    let source: Box<dyn Source> = Box::new(json_source(JsonChunkerConfig::default()));

    let result = source.parse(&tree.0);
    // Oracle "single json file" + "array json file": both files parse.
    // Deviation: bad.json is a non-fatal parse-stage error (the oracle
    // ingested it with structure "unknown" and hard-errored in the chunker).
    assert_eq!(source_files(&result), vec!["data.json", "items.json"]);
    assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
    assert!(
        matches!(
            result.errors[0],
            IngestionError::Json { ref path, .. } if path.ends_with("bad.json")
        ),
        "got {:?}",
        result.errors[0]
    );
    for doc in &result.documents {
        assert_eq!(doc.metadata.source_type, JsonSource::SOURCE_TYPE);
    }
    assert_eq!(
        extra_str(&document(&result, "data.json").metadata, "structure"),
        Some("object")
    );
    assert_eq!(
        extra_str(&document(&result, "items.json").metadata, "structure"),
        Some("array")
    );

    // Oracle TestJSONChunker_ChunkObject "single object per field": 2 chunks
    // (title + description). Deviation: the oracle's text is
    // "**title**: Page Title\n\n**description**: ..." (not a slice); here
    // the text is the raw JSON value, the field name lives in the metadata,
    // and the configured field order puts description before title.
    let data = document(&result, "data.json");
    let chunks = source.chunk(&data.content, &data.metadata).unwrap();
    assert_invariant(&data.content, &chunks);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, r#""Full description here.""#);
    assert_eq!(
        extra_str(&chunks[0].metadata, "field_name"),
        Some("description")
    );
    assert_eq!(chunks[1].text, r#""Page Title""#);
    assert_eq!(extra_str(&chunks[1].metadata, "field_name"), Some("title"));

    // Oracle "single object combined": 1 chunk for the whole object.
    let combined: Box<dyn Source> = Box::new(json_source(JsonChunkerConfig {
        combine_fields: true,
        ..Default::default()
    }));
    let chunks = combined.chunk(&data.content, &data.metadata).unwrap();
    assert_invariant(&data.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, data.content);
    assert_eq!(
        extra_strings(&chunks[0].metadata, "text_fields"),
        vec!["description".to_owned(), "title".to_owned()]
    );

    // Objects without any configured text field produce no chunks (oracle
    // "empty array" case, per object).
    let items = document(&result, "items.json");
    let chunks = source.chunk(&items.content, &items.metadata).unwrap();
    assert!(chunks.is_empty(), "{chunks:?}");

    // Deviation pin: a valid JSON scalar produces one chunk for the whole
    // content (the oracle hard-errored on non-array/non-object values — see
    // the chunker module docs).
    let scalar = "42";
    let chunks = source.chunk(scalar, &data.metadata).unwrap();
    assert_invariant(scalar, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, scalar);
}

#[test]
fn mediawiki_pipeline_matches_oracle() {
    let tree = mediawiki_tree();
    let source: Box<dyn Source> = Box::new(mediawiki_source());

    let result = source.parse(&tree.0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    // Oracle "skip graph.json": the graph file is not a page document.
    assert_eq!(
        source_files(&result),
        vec![
            "space/wiki-type/by-type/services/api_gateway.json",
            "space/wiki-type/by-type/systems/database.json",
        ]
    );
    for doc in &result.documents {
        assert_eq!(doc.metadata.source_type, MediawikiSource::SOURCE_TYPE);
    }

    let gateway = document(&result, "space/wiki-type/by-type/services/api_gateway.json");
    // Oracle TestMediawikiParser_ExtractContent "wikitext priority": the
    // wikitext wins over the html field.
    assert_eq!(
        gateway.content,
        "== API Gateway ==\nA service mesh component."
    );
    assert_eq!(extra_str(&gateway.metadata, "title"), Some("API Gateway"));
    assert_eq!(
        extra_str(&gateway.metadata, "url"),
        Some("https://example.com/API_Gateway")
    );
    // Oracle TestMediawikiParser_ExtractPathComponents "full path": the
    // by-type layer yields space / wiki type / entity.
    assert_eq!(extra_str(&gateway.metadata, "space"), Some("space"));
    assert_eq!(extra_str(&gateway.metadata, "wiki_type"), Some("wiki-type"));
    // Path layer first, the page JSON field as fallback (module docs).
    assert_eq!(
        extra_str(&gateway.metadata, "entity_type"),
        Some("services")
    );
    assert_eq!(
        extra_strings(&gateway.metadata, "page_links"),
        vec!["Service Catalog".to_owned()]
    );
    assert_eq!(
        extra_strings(&gateway.metadata, "image_paths"),
        vec!["gateway.png".to_owned()]
    );
    assert_eq!(
        extra_strings(&gateway.metadata, "categories"),
        vec!["Services".to_owned(), "Networking".to_owned()]
    );
    // Oracle TestMediawikiParser_GraphJSON: relations keyed by title.
    assert_eq!(
        extra_strings(&gateway.metadata, "graph_relations"),
        vec!["Service Catalog".to_owned(), "Load Balancer".to_owned()]
    );

    let database = document(&result, "space/wiki-type/by-type/systems/database.json");
    assert_eq!(database.content, "A data storage system.");
    assert_eq!(
        extra_str(&database.metadata, "entity_type"),
        Some("systems")
    );
    assert_eq!(
        extra_strings(&database.metadata, "graph_relations"),
        vec!["Storage".to_owned()]
    );

    // Chunking through the dedicated wikitext chunker (deviation: the
    // oracle's markdown chunker never matched wikitext headings).
    let chunks = source.chunk(&gateway.content, &gateway.metadata).unwrap();
    assert_invariant(&gateway.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        extra_str(&chunks[0].metadata, "section_title"),
        Some("API Gateway")
    );
    assert_eq!(
        extra_str(&chunks[0].metadata, "breadcrumb"),
        Some("> API Gateway")
    );
    assert_eq!(chunks[0].text, gateway.content);

    // Heading-less wikitext: one chunk for the whole content.
    let chunks = source.chunk(&database.content, &database.metadata).unwrap();
    assert_invariant(&database.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert!(extra_str(&chunks[0].metadata, "section_title").is_none());

    // Deviation pin: a heading-rich page is split at the wikitext headings
    // (the oracle's markdown chunker would emit one unsplit chunk).
    let rich = "== One ==\ntext one\n\n== Two ==\ntext two";
    let chunks = source.chunk(rich, &gateway.metadata).unwrap();
    assert_invariant(rich, &chunks);
    assert_eq!(chunks.len(), 2);
    assert_eq!(section_titles(&chunks), vec![Some("One"), Some("Two")]);
}

#[test]
fn webpage_pipeline_matches_oracle() {
    let tree = webpage_tree();
    let source: Box<dyn Source> = Box::new(webpage_source());

    let result = source.parse(&tree.0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    // Oracle "mixed md and html pages" (3 documents) + "md preferred over
    // html" (1) + "static directory excluded" (static/ never yields
    // documents).
    assert_eq!(
        source_files(&result),
        vec![
            "pages/docs.md",
            "pages/home.md",
            "pages/page-1.md",
            "pages/pricing.html",
        ]
    );
    for doc in &result.documents {
        assert_eq!(doc.metadata.source_type, WebpageSource::SOURCE_TYPE);
        assert!(doc.metadata.file_size.is_some());
        assert!(doc.metadata.modified_at.is_some());
    }

    // Oracle "md preferred over html for same page name": the md file wins.
    let page1 = document(&result, "pages/page-1.md");
    assert_eq!(page1.content, "# MD Content\nThis should win.");

    // Oracle "html page converted to markdown": structural conversion — the
    // heading becomes an ATX heading (what the injected markdown chunker
    // splits on) and the body text survives.
    let pricing = document(&result, "pages/pricing.html");
    assert!(
        pricing.content.contains("# Pricing"),
        "converted: {:?}",
        pricing.content
    );
    assert!(
        pricing.content.contains("Plans below."),
        "converted: {:?}",
        pricing.content
    );

    // Chunking: the converted html page splits on the converted heading.
    let chunks = source.chunk(&pricing.content, &pricing.metadata).unwrap();
    assert_invariant(&pricing.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        extra_str(&chunks[0].metadata, "section_title"),
        Some("Pricing")
    );

    // One chunk per md page with a body; the header-only page (home.md)
    // yields none (oracle TestMarkdownChunker_HeaderOnlySkip).
    let docs_md = document(&result, "pages/docs.md");
    let chunks = source.chunk(&docs_md.content, &docs_md.metadata).unwrap();
    assert_invariant(&docs_md.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        extra_str(&chunks[0].metadata, "section_title"),
        Some("Docs")
    );

    let home = document(&result, "pages/home.md");
    let chunks = source.chunk(&home.content, &home.metadata).unwrap();
    assert!(chunks.is_empty(), "{chunks:?}");
}

#[test]
fn unstructured_pipeline_matches_oracle() {
    let tree = unstructured_tree();
    let source: Box<dyn Source> = Box::new(unstructured_source());

    let result = source.parse(&tree.0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    // Oracle "single md file" (readme) + ImageAssociation (article) +
    // runner e2e (policies.json). One global sorted order (deviation: the
    // oracle concatenated all md documents before all json documents).
    assert_eq!(
        source_files(&result),
        vec!["data/policies.json", "docs/article.md", "docs/readme.md"]
    );

    let policies = document(&result, "data/policies.json");
    assert_eq!(policies.metadata.source_type, JsonSource::SOURCE_TYPE);

    for file in ["docs/article.md", "docs/readme.md"] {
        let doc = document(&result, file);
        assert_eq!(doc.metadata.source_type, UnstructuredSource::SOURCE_TYPE);
        // Oracle TestUnstructuredParser_ImageAssociation: the image files
        // of the same directory, sorted.
        assert_eq!(
            extra_strings(&doc.metadata, "image_paths"),
            vec!["banner.png".to_owned(), "logo.svg".to_owned()]
        );
    }

    // Routing: the markdown document goes to the markdown chunker.
    let readme = document(&result, "docs/readme.md");
    let chunks = source.chunk(&readme.content, &readme.metadata).unwrap();
    assert_invariant(&readme.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        extra_str(&chunks[0].metadata, "section_title"),
        Some("Hello")
    );
    // The parser's directory images survive on the chunk (no `![...]` in
    // the body to replace them).
    assert_eq!(
        extra_strings(&chunks[0].metadata, "image_paths"),
        vec!["banner.png".to_owned(), "logo.svg".to_owned()]
    );

    let article = document(&result, "docs/article.md");
    let chunks = source.chunk(&article.content, &article.metadata).unwrap();
    assert_invariant(&article.content, &chunks);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        extra_str(&chunks[0].metadata, "section_title"),
        Some("Article")
    );
    // The markdown chunker re-derives image_paths from the section body
    // (oracle sectionBody + imageRe): only the body's own image.
    assert_eq!(
        extra_strings(&chunks[0].metadata, "image_paths"),
        vec!["banner.png".to_owned()]
    );

    // Routing: the JSON document goes to the JSON chunker (oracle
    // runner_test.go e2e fixture; per-field mode, configured field order:
    // description before title).
    let chunks = source.chunk(&policies.content, &policies.metadata).unwrap();
    assert_invariant(&policies.content, &chunks);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, r#""Confidentiality agreement""#);
    assert_eq!(
        extra_str(&chunks[0].metadata, "field_name"),
        Some("description")
    );
    assert_eq!(chunks[1].text, r#""NDA Policy""#);
    assert_eq!(extra_str(&chunks[1].metadata, "field_name"), Some("title"));
}

#[test]
fn byte_offset_invariant_holds_for_every_format() {
    // Crate contract (design D2), swept across all five formats: every
    // chunk of every document is a pure byte-offset slice of its content,
    // numbered consecutively, with no document id yet. `pipeline` asserts
    // the invariant per chunk; the counts guard against a vacuous sweep.
    let markdown = pipeline(&markdown_source(), &markdown_tree(), false);
    let json = pipeline(
        &json_source(JsonChunkerConfig::default()),
        &json_tree(),
        true,
    );
    let mediawiki = pipeline(&mediawiki_source(), &mediawiki_tree(), false);
    let webpage = pipeline(&webpage_source(), &webpage_tree(), false);
    let unstructured = pipeline(&unstructured_source(), &unstructured_tree(), false);

    assert!(markdown >= 2, "markdown produced {markdown} chunks");
    assert!(json >= 2, "json produced {json} chunks");
    assert!(mediawiki >= 2, "mediawiki produced {mediawiki} chunks");
    assert!(webpage >= 2, "webpage produced {webpage} chunks");
    assert!(
        unstructured >= 3,
        "unstructured produced {unstructured} chunks"
    );
}
