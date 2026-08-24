//! Unstructured parser (oracle:
//! `internal/ingestion/parsers/unstructured_parser.go`).
//!
//! The oracle's `UnstructuredSource` merges two parsers: an
//! `UnstructuredParser` for `.md` files and the `JSONParser` for `.json`
//! files. This module is the Rust re-architecture of that merge: one shared
//! walk over the source tree that dispatches each file to the matching
//! reader, so the two formats are parsed in one pass with one global
//! deterministic order. Markdown documents carry the `image_paths` metadata:
//! the image file names sitting in the same directory as the file (oracle
//! `collectImages`). Per-file failures are collected in
//! [`ParseResult::errors`] and never abort the walk (design D1, oracle
//! contract).
//!
//! **Deliberate deviations from the oracle** (functional copy, not code
//! copy):
//!
//! * **Single walk, global sorted order.** The oracle runs two separate
//!   walks (one per parser) and concatenates all Markdown documents before
//!   all JSON documents. Rust does one walk and dispatches per file, so the
//!   documents come out in one global sorted path order. Document order is
//!   not a frozen contract (the oracle's registry iteration was map-ordered
//!   anyway).
//! * **Exclusions** come from user `.synignore` files (human decision
//!   2026-08-23) via the shared walk; the oracle's hardcoded `skipDirs` list
//!   does not exist here.
//! * **`stat` failure degrades** to absent `file_size`/`modified_at` instead
//!   of discarding the already-read content (same deviation as the markdown
//!   parser).
//! * The oracle's public `GroupSections` helper is not ported: chunking is
//!   the job of the chunkers injected into
//!   [`UnstructuredSource`](crate::sources::UnstructuredSource), and the
//!   markdown chunker's structure-aware splitting (headings + size caps) is
//!   a superset of `GroupSections` (which only cut at heading boundaries).
//! * The oracle matches only `.md` (not `.markdown`) for the unstructured
//!   format; that restriction is preserved here.
//! * The oracle's `isImageExt` helper lives in the webpage file (where it is
//!   dead code) but is *used* by this parser; the extension list is ported
//!   here, where it is live.

use std::path::Path;

use serde_json::{Map, Value};

use crate::error::IngestionError;
use crate::parsers::json::JsonParser;
use crate::parsers::{format_rfc3339_utc, source_file_name, walk_matched_files};
use crate::types::{Document, DocumentMetadata, ParseResult, Parser};

/// File extensions the unstructured parser accepts, in the order reported by
/// [`Parser::supported_extensions`] (oracle: `.md` from the unstructured
/// parser, `.json` from the JSON parser).
const UNSTRUCTURED_EXTENSIONS: &[&str] = &[".md", ".json"];

/// Image file extensions collected into the `image_paths` metadata (oracle
/// `isImageExt`).
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];

/// Metadata extra key holding the image file names found in the same
/// directory as the Markdown file (oracle `metadata["image_paths"]`).
const IMAGE_PATHS_KEY: &str = "image_paths";

/// Parses unstructured (Markdown + JSON) source trees into documents.
///
/// Stateless: all inputs arrive through [`Parser::parse`], so the parser is
/// a `Copy` unit struct.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnstructuredParser;

impl UnstructuredParser {
    /// True if `path` ends with a supported extension, case insensitively
    /// (oracle: `strings.ToLower(name)` + suffix checks).
    fn is_candidate(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("json"))
    }

    /// True if `path` is a Markdown file (the JSON half is everything else
    /// that matched [`is_candidate`](Self::is_candidate)).
    fn is_markdown(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    }

    /// Reads one Markdown file into a [`Document`] with the image files of
    /// its directory in the metadata (oracle `parseMarkdownFile`).
    fn read_markdown_file(path: &Path, root: &Path) -> Result<Document, IngestionError> {
        let content = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // `metadata` is best-effort: a read that succeeded but a stat that
        // fails (e.g. the file vanished mid-walk) yields a document with
        // absent size/mtime rather than discarding the content we already
        // have (same degradation as the markdown parser).
        let meta = std::fs::metadata(path).ok();
        let file_size = meta.as_ref().map(|m| m.len());
        let modified_at = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(format_rfc3339_utc);

        let mut extra = Map::new();
        // Oracle `collectImages`: image file names in the *same* directory,
        // not recursive. A directory that cannot be read yields no images
        // (oracle returns nil), not an error.
        if let Some(dir) = path.parent() {
            let images = Self::collect_images(dir);
            if !images.is_empty() {
                extra.insert(
                    IMAGE_PATHS_KEY.to_owned(),
                    Value::Array(images.into_iter().map(Value::String).collect()),
                );
            }
        }

        Ok(Document {
            source_path: path.to_path_buf(),
            content,
            metadata: DocumentMetadata {
                source_type: "unstructured".to_owned(),
                source_file: source_file_name(path, root),
                file_size,
                modified_at,
                extra,
            },
        })
    }

    /// The image file names (base names, sorted) in `dir` (oracle
    /// `collectImages`). A directory that cannot be read yields no images.
    fn collect_images(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<std::ffi::OsString> = entries
            .flatten()
            .filter(|entry| {
                // `file_type` does not follow symlinks, like the oracle's
                // `DirEntry.IsDir`; subdirectories are never collected.
                entry.file_type().is_ok_and(|file_type| !file_type.is_dir())
                    && Path::new(&entry.file_name())
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            IMAGE_EXTENSIONS
                                .iter()
                                .any(|wanted| ext.eq_ignore_ascii_case(wanted))
                        })
            })
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }
}

impl Parser for UnstructuredParser {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        walk_matched_files(
            source_path,
            Self::is_candidate,
            |path| {
                if Self::is_markdown(path) {
                    documents.push(Self::read_markdown_file(path, source_path)?);
                } else {
                    // The JSON half reuses the JSON parser's reader verbatim
                    // (source_type "json" + structure detection).
                    documents.push(JsonParser::read_file(path, source_path)?);
                }
                Ok(())
            },
            &mut errors,
        );
        ParseResult { documents, errors }
    }

    fn supported_extensions(&self) -> &[&str] {
        UNSTRUCTURED_EXTENSIONS
    }
}

#[cfg(test)]
mod tests {
    //! Differential tests against the Go oracle: inputs and expectations
    //! (document counts, `source_type` metadata, image association) are taken
    //! from
    //! `../synopsis/internal/ingestion/parsers/unstructured_parser_test.go`,
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

    fn doc_of<'a>(result: &'a ParseResult, file: &str) -> &'a Document {
        result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == file)
            .unwrap()
    }

    #[test]
    fn supported_extensions_are_md_and_json() {
        let parser = UnstructuredParser;
        assert_eq!(parser.supported_extensions(), [".md", ".json"]);
    }

    #[test]
    fn md_files_produce_unstructured_documents() {
        // Oracle TestUnstructuredParser_Parse: single md file -> 1 document;
        // multiple md files + a .txt -> 2 documents (the .txt is ignored).
        let tree = TempTree::new();
        tree.write("docs/readme.md", "# Hello\nSome text.");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_type, "unstructured");

        let tree = TempTree::new();
        tree.write("docs/a.md", "# A");
        tree.write("docs/b.md", "# B");
        tree.write("docs/c.txt", "not markdown");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["docs/a.md", "docs/b.md"]);
    }

    #[test]
    fn json_files_produce_json_documents() {
        let tree = TempTree::new();
        tree.write("data/items.json", r#"[{"id": 1}, {"id": 2}]"#);

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let doc = &result.documents[0];
        assert_eq!(doc.metadata.source_type, "json");
        assert_eq!(
            doc.metadata.extra.get("structure").and_then(Value::as_str),
            Some("array")
        );
    }

    #[test]
    fn metadata_is_populated() {
        let tree = TempTree::new();
        let content = "# Heading\n\nBody text here";
        let path = tree.write("doc.md", content);

        let result = UnstructuredParser.parse(&tree.0);
        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);

        let doc = &result.documents[0];
        assert_eq!(doc.source_path, path);
        assert_eq!(doc.content, content);
        assert_eq!(doc.metadata.source_type, "unstructured");
        assert_eq!(doc.metadata.source_file, "doc.md");
        assert_eq!(doc.metadata.file_size, Some(content.len() as u64));
        // RFC 3339 UTC: ends with 'Z', has a 'T' separator.
        let mtime = doc.metadata.modified_at.as_ref().unwrap();
        assert!(mtime.ends_with('Z'), "not RFC 3339 UTC: {mtime}");
        assert!(mtime.contains('T'), "not RFC 3339: {mtime}");
    }

    #[test]
    fn images_in_same_directory_are_associated() {
        // Oracle TestUnstructuredParser_ImageAssociation: article.md with
        // banner.png and logo.svg next to it -> 1 document with 2 image
        // paths (base names).
        let tree = TempTree::new();
        tree.write("docs/article.md", "# Article\n![hero](banner.png)");
        tree.write("docs/banner.png", "image data");
        tree.write("docs/logo.svg", "<svg/>");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(
            doc_of(&result, "docs/article.md")
                .metadata
                .extra
                .get(IMAGE_PATHS_KEY)
                .and_then(Value::as_array)
                .map(|images| {
                    images
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                        .collect::<Vec<_>>()
                }),
            Some(vec!["banner.png".to_owned(), "logo.svg".to_owned()])
        );
    }

    #[test]
    fn image_collection_is_flat_and_extension_filtered() {
        // Oracle `collectImages`: the same directory only (no recursion),
        // image extensions only (case-insensitive), base names only.
        let tree = TempTree::new();
        tree.write("docs/article.md", "# Article");
        tree.write("docs/photo.JPG", "uppercase extension");
        tree.write("docs/notes.txt", "not an image");
        tree.write("docs/sub/inner.png", "subdirectory: not collected");
        tree.write("docs/archive.tar.png", "extension is the last one");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        let images = doc_of(&result, "docs/article.md")
            .metadata
            .extra
            .get(IMAGE_PATHS_KEY)
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        // Sorted base names; the .txt and the nested .png are absent.
        assert_eq!(images, vec!["archive.tar.png", "photo.JPG"]);
    }

    #[test]
    fn documents_are_in_global_sorted_order() {
        // Deviation (module docs): one global sorted path order instead of
        // the oracle's "all markdown, then all json" concatenation.
        let tree = TempTree::new();
        tree.write("b/items.json", "[]");
        tree.write("a/guide.md", "# Guide");
        tree.write("c/data.json", "{}");
        tree.write("d/notes.md", "# Notes");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec!["a/guide.md", "b/items.json", "c/data.json", "d/notes.md"]
        );
        assert_eq!(
            result
                .documents
                .iter()
                .map(|d| d.metadata.source_type.as_str())
                .collect::<Vec<_>>(),
            vec!["unstructured", "json", "json", "unstructured"]
        );
    }

    #[test]
    fn single_file_error_does_not_break_the_walk() {
        // Invalid UTF-8 markdown plus a broken JSON file: both are collected
        // as non-fatal errors, the walk continues, the rest is parsed.
        let tree = TempTree::new();
        tree.write("good.md", "# Good");
        let bad_md = tree.0.join("bad.md");
        fs::write(&bad_md, [0xff, 0xfe, 0x00]).expect("write invalid utf-8 file");
        let bad_json = tree.write("broken.json", "not json");
        tree.write("after.md", "# After");

        let result = UnstructuredParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 2, "two non-fatal errors");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Io { ref path, .. } if path == &bad_md
            ),
            "first error must be the invalid markdown, got {:?}",
            result.errors[0]
        );
        assert!(
            matches!(
                result.errors[1],
                IngestionError::Json { ref path, .. } if path == &bad_json
            ),
            "second error must be the broken JSON, got {:?}",
            result.errors[1]
        );
        assert_eq!(source_files(&result), vec!["after.md", "good.md"]);
    }

    #[test]
    fn missing_root_is_a_non_fatal_error() {
        let tree = TempTree::new();
        let missing = tree.0.join("does-not-exist");

        let result = UnstructuredParser.parse(&missing);

        assert!(result.documents.is_empty());
        assert_eq!(result.errors.len(), 1, "missing root -> one error");
    }

    #[test]
    fn single_md_file_source_is_parsed() {
        let tree = TempTree::new();
        tree.write("docs/pic.png", "image data");
        let path = tree.write("docs/standalone.md", "# Standalone");

        let result = UnstructuredParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.md");
        assert_eq!(result.documents[0].content, "# Standalone");
        // The single-file source still associates the images of its
        // directory.
        assert_eq!(
            result.documents[0]
                .metadata
                .extra
                .get(IMAGE_PATHS_KEY)
                .and_then(Value::as_array)
                .map(|images| images.len()),
            Some(1)
        );
    }

    #[test]
    fn single_json_file_source_is_parsed() {
        let tree = TempTree::new();
        let path = tree.write("standalone.json", "[1, 2, 3]");

        let result = UnstructuredParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.json");
        assert_eq!(result.documents[0].content, "[1, 2, 3]");
    }

    #[test]
    fn synignore_exclusions_are_inherited() {
        let tree = TempTree::new();
        tree.write(".synignore", "generated/\n*.draft.md\n");
        tree.write("keep.md", "# kept");
        tree.write("keep.json", "{}");
        tree.write("generated/a.md", "# excluded directory");
        tree.write("generated/b.json", "{}");
        tree.write("notes.draft.md", "# excluded pattern");

        let result = UnstructuredParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["keep.json", "keep.md"]);
    }

    #[test]
    fn empty_tree_yields_no_documents_and_no_errors() {
        let tree = TempTree::new();
        let result = UnstructuredParser.parse(&tree.0);
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }
}
