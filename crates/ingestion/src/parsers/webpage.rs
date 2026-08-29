//! Webpage parser (oracle: `internal/ingestion/parsers/webpage_parser.go`).
//!
//! Walks a webpage dataset tree and returns one [`Document`] per page.
//! Expected layout (oracle):
//!
//! ```text
//! sourcePath/
//!   pages/
//!     [page-1].md, [page-2].md, ...   (preferred)
//!     [page-1].html, [page-2].html, ... (fallback, converted to Markdown)
//!   static/                            (skipped: site assets, not content)
//! ```
//!
//! Pages are grouped per directory by page name (file name without
//! extension, case-insensitive — the oracle lowercased the name). When both
//! `page.md` and `page.html` exist for the same page name, the `.md` file
//! wins and the `.html` twin is ignored (oracle `parsePage` preference).
//! `.html` pages are converted to Markdown with `html-to-markdown-rs` — the
//! Rust analogue of the oracle's `JohannesKaufmann/html-to-markdown` library
//! (html5ever-based, fault-tolerant, structural output) — so the document
//! content is Markdown and the injected markdown chunker can split it on ATX
//! headings. **Byte-offset invariant:** chunk offsets are relative to this
//! extracted/converted content (`Document.content`), never the raw HTML.
//!
//! **Deliberate deviations from the oracle** (functional copy, not code
//! copy):
//!
//! * **Exclusions** come from user `.synignore` files (human decision
//!   2026-08-23) via the shared walk; the oracle's hardcoded `skipDirs` list
//!   does not exist here. The `static/` skip is kept, but as a
//!   *format-specific* rule (it names part of the webpage dataset layout,
//!   like `graph.json` is for mediawiki), not a global skip list.
//! * **`file_size`** is the on-disk file size of the chosen page file; the
//!   oracle stored `len(content)` (the converted Markdown size for `.html`
//!   pages — inconsistent with its other parsers). **`modified_at`** is
//!   populated like the sibling parsers (the oracle left it unset).
//! * **Invalid UTF-8** in an `.html` file is a non-fatal
//!   [`IngestionError::Io`] (the Rust converter takes `&str`; the Go oracle
//!   string-cast the bytes).
//! * The oracle's `isImageExt` helper is defined in this file but never used
//!   by the webpage parser (its only caller is `unstructured_parser.go`'s
//!   `collectImages`); the Rust port therefore lives in the unstructured
//!   module, where it is live.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Map;

use crate::error::IngestionError;
use crate::parsers::{format_rfc3339_utc, source_file_name, walk_matched_files};
use crate::types::{Document, DocumentMetadata, ParseResult, Parser};

/// File extensions the webpage parser accepts, in the order reported by
/// [`Parser::supported_extensions`] (oracle: `.md` preferred, `.html`
/// converted to Markdown).
const WEBPAGE_EXTENSIONS: &[&str] = &[".md", ".html"];

/// Directory name that holds static site assets (css/js/images), never
/// content pages (oracle: `d.Name() == "static"` pruned the subtree).
const STATIC_DIR: &str = "static";

/// Candidate files collected for one page name in one directory (oracle
/// `pageFiles`). At least one is `Some` for every grouped entry.
#[derive(Debug, Default)]
struct PageFiles {
    /// Path to the `.md` file, if present (preferred).
    md: Option<PathBuf>,
    /// Path to the `.html` file, if present (fallback).
    html: Option<PathBuf>,
}

/// Parses webpage dataset trees into page documents.
///
/// Stateless: all inputs arrive through [`Parser::parse`], so the parser is
/// a `Copy` unit struct.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebpageParser;

impl WebpageParser {
    /// True for a page file: a `.md` or `.html` file, case-insensitively
    /// (oracle: `strings.ToLower(name)` + extension check).
    fn is_page_file(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("html"))
    }

    /// True when `path` lies under a directory named `static` at or below
    /// the walk root (oracle: the `static` directory prunes its whole
    /// subtree, the root included).
    fn under_static_dir(path: &Path, root: &Path, root_is_static: bool) -> bool {
        if root_is_static {
            return true;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        // The directory portion of the relative path: every directory
        // component between the root and the file (empty for a file that
        // sits directly in the root).
        let Some(rel_dir) = rel.parent() else {
            return false;
        };
        rel_dir
            .components()
            .any(|component| component.as_os_str() == STATIC_DIR)
    }

    /// Groups the walked page files by `dir/page-name` (lowercased page
    /// name; `.md` preferred over `.html`) — oracle `collectPages`.
    fn collect_pages(
        source_path: &Path,
        errors: &mut Vec<IngestionError>,
    ) -> BTreeMap<String, PageFiles> {
        let root_is_static = std::fs::symlink_metadata(source_path).is_ok_and(|meta| meta.is_dir())
            && source_path.file_name().and_then(|name| name.to_str()) == Some(STATIC_DIR);

        let mut pages: BTreeMap<String, PageFiles> = BTreeMap::new();
        walk_matched_files(
            source_path,
            |path| {
                Self::is_page_file(path)
                    && !Self::under_static_dir(path, source_path, root_is_static)
            },
            |path| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_lowercase();
                let ext = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or_default()
                    .to_lowercase();
                let page_name = name.strip_suffix(&format!(".{ext}")).unwrap_or(&name);
                // A bare file name has an empty parent; the walk never
                // leaves the root otherwise.
                let dir = path.parent().unwrap_or_else(|| Path::new(""));
                let key = format!("{dir}/{page_name}", dir = dir.display());
                let page = pages.entry(key).or_default();
                if ext == "md" {
                    page.md = Some(path.to_path_buf());
                } else {
                    page.html = Some(path.to_path_buf());
                }
                Ok(())
            },
            errors,
        );
        pages
    }

    /// Reads the content for one grouped page, preferring `.md` over `.html`
    /// and converting HTML to Markdown (oracle `parsePage` content branch).
    /// `Ok(None)` is unreachable: every grouped entry has at least one
    /// candidate file.
    fn page_content(page: &PageFiles) -> Result<Option<(PathBuf, String)>, IngestionError> {
        if let Some(md) = &page.md {
            let content = std::fs::read_to_string(md).map_err(|source| IngestionError::Io {
                path: md.clone(),
                source,
            })?;
            return Ok(Some((md.clone(), content)));
        }
        let Some(html) = &page.html else {
            return Ok(None);
        };
        let raw = std::fs::read_to_string(html).map_err(|source| IngestionError::Io {
            path: html.clone(),
            source,
        })?;
        let content = Self::convert_html(&raw, html)?;
        Ok(Some((html.clone(), content)))
    }

    /// Converts one HTML page to Markdown (oracle `conv.ConvertString`).
    /// `None` output (an empty document) degrades to empty content, like
    /// the oracle's empty string.
    fn convert_html(html: &str, path: &Path) -> Result<String, IngestionError> {
        let result = html_to_markdown_rs::convert(html, None).map_err(|error| {
            IngestionError::HtmlConversion {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        })?;
        Ok(result.content.unwrap_or_default())
    }

    /// Builds the document for one read page (oracle `parsePage` tail).
    fn document(file_path: PathBuf, content: String, root: &Path) -> Document {
        // `metadata` is best-effort: a read that succeeded but a stat that
        // fails (e.g. the file vanished mid-walk) yields a document with
        // absent size/mtime rather than discarding the content we already
        // have (same degradation as the sibling parsers).
        let meta = std::fs::metadata(&file_path).ok();
        let file_size = meta.as_ref().map(|meta| meta.len());
        let modified_at = meta
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .and_then(format_rfc3339_utc);

        Document {
            source_path: file_path.clone(),
            content,
            metadata: DocumentMetadata {
                source_type: "webpages".to_owned(),
                source_file: source_file_name(&file_path, root),
                file_size,
                modified_at,
                extra: Map::new(),
            },
        }
    }
}

impl Parser for WebpageParser {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut errors = Vec::new();
        let pages = Self::collect_pages(source_path, &mut errors);

        // BTreeMap iteration is sorted by the `dir/page-name` key — the
        // oracle's `slices.Sort(keys)` order.
        let mut documents = Vec::new();
        for page in pages.into_values() {
            match Self::page_content(&page) {
                Ok(Some((file_path, content))) => {
                    documents.push(Self::document(file_path, content, source_path));
                }
                Ok(None) => {}                    // unreachable (module docs)
                Err(error) => errors.push(error), // per-page failure: non-fatal
            }
        }
        ParseResult { documents, errors }
    }

    fn parse_file(&self, path: &Path, root: &Path) -> Result<Document, IngestionError> {
        // No tree walk: exactly one candidate file, chosen by extension
        // (`.md` read as-is, `.html` converted to Markdown).
        let mut page = PageFiles::default();
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        if ext.eq_ignore_ascii_case("md") {
            page.md = Some(path.to_path_buf());
        } else if ext.eq_ignore_ascii_case("html") {
            page.html = Some(path.to_path_buf());
        } else {
            return Err(IngestionError::UnsupportedExtension(format!(".{ext}")));
        }
        // One candidate is set, so a read is always produced.
        let Some((file_path, content)) = Self::page_content(&page)? else {
            unreachable!("a single-candidate page always yields a read");
        };
        Ok(Self::document(file_path, content, root))
    }

    fn supported_extensions(&self) -> &[&str] {
        WEBPAGE_EXTENSIONS
    }
}

#[cfg(test)]
mod tests {
    //! Differential tests against the Go oracle: inputs and expectations
    //! (document counts, md-over-html preference, `static/` exclusion,
    //! metadata shape) are taken from
    //! `../synopsis/internal/ingestion/parsers/webpage_parser_test.go`,
    //! which passes there (`go test ./internal/ingestion/parsers/`).

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use super::*;
    use crate::parsers::tests::TempTree;

    fn source_files(result: &ParseResult) -> Vec<String> {
        result
            .documents
            .iter()
            .map(|d| d.metadata.source_file.clone())
            .collect()
    }

    fn content_of(result: &ParseResult, file: &str) -> Option<String> {
        result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == file)
            .map(|d| d.content.clone())
    }

    #[test]
    fn supported_extensions_are_md_and_html() {
        let parser = WebpageParser;
        assert_eq!(parser.supported_extensions(), [".md", ".html"]);
    }

    #[test]
    fn flat_md_pages_produce_one_document_each() {
        // Oracle TestWebpageParser_Parse "flat md pages produce one document each".
        let tree = TempTree::new();
        tree.write("pages/index.md", "# Home\nWelcome to the site.");
        tree.write("pages/about.md", "# About\nOur story.");
        tree.write("pages/contact.md", "# Contact\nEmail us.");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 3);
        for doc in &result.documents {
            assert_eq!(doc.metadata.source_type, "webpages");
            assert!(!doc.metadata.source_file.is_empty());
            assert!(doc.metadata.file_size.is_some());
            assert!(doc.metadata.modified_at.is_some());
        }
    }

    #[test]
    fn html_page_is_converted_to_markdown() {
        // Oracle TestWebpageParser_Parse "html page converted to markdown":
        // the converted content is non-empty Markdown.
        let tree = TempTree::new();
        tree.write("pages/page-1.html", "<h1>Title</h1><p>Body text.</p>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let content = &result.documents[0].content;
        // Structural conversion: the heading is an ATX heading (this is what
        // the injected markdown chunker splits on), the body text survives.
        assert!(content.contains("# Title"), "got: {content:?}");
        assert!(content.contains("Body text."), "got: {content:?}");
        assert!(!content.contains('<'), "tags must not survive: {content:?}");
    }

    #[test]
    fn md_preferred_over_html_for_same_page_name() {
        // Oracle TestWebpageParser_Parse "md preferred over html for same page name".
        let tree = TempTree::new();
        tree.write("pages/page-1.md", "# MD Content\nThis should win.");
        tree.write(
            "pages/page-1.html",
            "<h1>HTML Content</h1><p>This should lose.</p>",
        );

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(
            result.documents[0].content,
            "# MD Content\nThis should win."
        );
        assert_eq!(result.documents[0].metadata.source_file, "pages/page-1.md");
    }

    #[test]
    fn static_directory_is_excluded() {
        // Oracle TestWebpageParser_Parse "static directory excluded".
        let tree = TempTree::new();
        tree.write("pages/index.md", "# Home");
        tree.write("static/logo.png", "binary data");
        tree.write("static/images/bg.jpg", "more binary");
        tree.write("static/hidden.md", "a page-looking file under static/");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(source_files(&result), vec!["pages/index.md"]);
    }

    #[test]
    fn a_root_named_static_yields_no_documents() {
        // Oracle: the `static` skip applies to the walk root itself
        // (`filepath.SkipDir` on the root entry prunes everything).
        let tree = TempTree::new();
        tree.write("static/pages/index.md", "# Home");

        let result = WebpageParser.parse(&tree.0.join("static"));

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert!(result.documents.is_empty());
    }

    #[test]
    fn mixed_md_and_html_pages() {
        // Oracle TestWebpageParser_Parse "mixed md and html pages".
        let tree = TempTree::new();
        tree.write("pages/home.md", "# Home MD");
        tree.write("pages/pricing.html", "<h1>Pricing</h1><p>Plans below.</p>");
        tree.write("pages/docs.md", "# Docs\nAPI reference.");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 3);
        assert_eq!(
            source_files(&result),
            vec!["pages/docs.md", "pages/home.md", "pages/pricing.html"]
        );
    }

    #[test]
    fn subdirectory_pages_are_collected() {
        // Oracle TestWebpageParser_Parse "subdirectory pages collected".
        let tree = TempTree::new();
        tree.write("pages/blog/post-1.md", "# Post One");
        tree.write("pages/blog/post-2.html", "<h1>Post Two</h1>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec!["pages/blog/post-1.md", "pages/blog/post-2.html"]
        );
    }

    #[test]
    fn non_md_html_files_are_ignored() {
        // Oracle TestWebpageParser_Parse "non-md-html files ignored".
        let tree = TempTree::new();
        tree.write("pages/index.md", "# Home");
        tree.write("pages/readme.txt", "not a content file");
        tree.write("pages/data.json", r#"{"key":"value"}"#);

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["pages/index.md"]);
    }

    #[test]
    fn extensions_and_page_names_are_case_insensitive() {
        // Oracle: `strings.ToLower(name)` before the extension check and the
        // page-name grouping.
        let tree = TempTree::new();
        tree.write("pages/UPPER.HTML", "<h1>Upper</h1>");
        tree.write("pages/About.MD", "# About MD");
        tree.write("pages/about.html", "<h1>About HTML</h1>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        // `About.MD` and `about.html` group under the same page name; the
        // md twin wins.
        assert_eq!(
            source_files(&result),
            vec!["pages/About.MD", "pages/UPPER.HTML"]
        );
        assert_eq!(
            content_of(&result, "pages/About.MD").as_deref(),
            Some("# About MD")
        );
    }

    #[test]
    fn source_file_metadata_is_relative() {
        // Oracle TestWebpageParser_SourceFileMetadata.
        let tree = TempTree::new();
        tree.write("pages/index.md", "# Home");
        tree.write("pages/about.html", "<h1>About</h1>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec!["pages/about.html", "pages/index.md"]
        );
    }

    #[test]
    fn valid_html_produces_no_errors() {
        // Oracle TestWebpageParser_HTMLConversion.
        let tree = TempTree::new();
        tree.write("pages/broken.html", "<h1>Valid HTML</h1><p>Content.</p>");

        let result = WebpageParser.parse(&tree.0);

        assert_eq!(result.documents.len(), 1);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    }

    #[test]
    fn malformed_html_still_converts() {
        // html5ever is fault-tolerant (like the oracle's x/net/html): broken
        // markup degrades to best-effort Markdown, not an error.
        let tree = TempTree::new();
        tree.write("pages/tangled.html", "<h1>Unclosed<div><p>Text</p>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let content = &result.documents[0].content;
        assert!(content.contains("Unclosed"), "got: {content:?}");
        assert!(content.contains("Text"), "got: {content:?}");
    }

    #[test]
    fn system_directories_are_walked_without_synignore() {
        // Oracle TestWebpageParser_SkipDirs (shape only): the oracle's
        // hardcoded skipDirs is gone (human decision 2026-08-23); without a
        // .synignore everything is walked. The test files are not page files
        // (no .md/.html), so the document count is unchanged either way.
        let tree = TempTree::new();
        tree.write("pages/index.md", "# Home");
        tree.write(".git/config", "[core]");
        tree.write("node_modules/pkg.js", "module.exports = {}");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
    }

    #[test]
    fn synignore_exclusions_are_inherited() {
        let tree = TempTree::new();
        tree.write(".synignore", "generated/\n*.draft.md\n");
        tree.write("pages/keep.md", "# kept");
        tree.write("pages/generated/drop.md", "# excluded directory");
        tree.write("pages/draft.draft.md", "# excluded pattern");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["pages/keep.md"]);
    }

    #[test]
    fn invalid_utf8_html_is_non_fatal() {
        // Deviation (module docs): the Rust converter takes `&str`, so an
        // invalid-UTF-8 file fails at the read with a non-fatal Io error.
        let tree = TempTree::new();
        tree.write("pages/good.html", "<h1>Good</h1>");
        let bad = tree.0.join("pages/bad.html");
        fs::write(&bad, [0xff, 0xfe, 0x00]).expect("write invalid utf-8 file");

        let result = WebpageParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1, "one non-fatal read error");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Io { ref path, .. } if path == &bad
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        assert_eq!(source_files(&result), vec!["pages/good.html"]);
    }

    #[test]
    fn missing_root_is_a_non_fatal_error() {
        let tree = TempTree::new();
        let missing = tree.0.join("does-not-exist");

        let result = WebpageParser.parse(&missing);

        assert!(result.documents.is_empty());
        assert_eq!(result.errors.len(), 1, "missing root -> one error");
    }

    #[test]
    fn single_md_file_source_is_parsed() {
        let tree = TempTree::new();
        let path = tree.write("standalone.md", "# Standalone");

        let result = WebpageParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.md");
        assert_eq!(result.documents[0].content, "# Standalone");
    }

    #[test]
    fn single_html_file_source_is_converted() {
        let tree = TempTree::new();
        let path = tree.write("standalone.html", "<h1>Standalone</h1>");

        let result = WebpageParser.parse(&path);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.html");
        assert!(result.documents[0].content.contains("Standalone"));
    }

    #[test]
    fn empty_tree_yields_no_documents_and_no_errors() {
        let tree = TempTree::new();
        let result = WebpageParser.parse(&tree.0);
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn parse_file_reads_one_md_page() {
        let tree = TempTree::new();
        let path = tree.write("pages/index.md", "# Home\nWelcome.");

        let doc = WebpageParser.parse_file(&path, &tree.0).unwrap();

        assert_eq!(doc.source_path, path);
        assert_eq!(doc.content, "# Home\nWelcome.");
        assert_eq!(doc.metadata.source_type, "webpages");
        assert_eq!(doc.metadata.source_file, "pages/index.md");
    }

    #[test]
    fn parse_file_converts_one_html_page() {
        let tree = TempTree::new();
        let path = tree.write("pages/page-1.html", "<h1>Title</h1><p>Body.</p>");

        let doc = WebpageParser.parse_file(&path, &tree.0).unwrap();

        assert_eq!(doc.source_path, path);
        assert_eq!(doc.metadata.source_file, "pages/page-1.html");
        assert!(doc.content.contains("# Title"), "got: {:?}", doc.content);
        assert!(doc.content.contains("Body."), "got: {:?}", doc.content);
    }

    #[test]
    fn parse_file_rejects_unsupported_extensions() {
        let tree = TempTree::new();
        let bad = tree.write("pages/notes.txt", "plain");

        let err = WebpageParser.parse_file(&bad, &tree.0).unwrap_err();
        assert!(
            matches!(err, IngestionError::UnsupportedExtension(ref ext) if ext == ".txt"),
            "got: {err:?}"
        );
    }

    #[test]
    fn pages_are_ordered_deterministically() {
        let tree = TempTree::new();
        // Deliberately non-sorted creation order.
        tree.write("pages/zeta.md", "# Z");
        tree.write("pages/alpha.html", "<h1>A</h1>");
        tree.write("pages/beta/gamma.md", "# G");
        tree.write("pages/beta/delta.html", "<h1>D</h1>");

        let result = WebpageParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec![
                "pages/alpha.html",
                "pages/beta/delta.html",
                "pages/beta/gamma.md",
                "pages/zeta.md",
            ]
        );
    }
}
