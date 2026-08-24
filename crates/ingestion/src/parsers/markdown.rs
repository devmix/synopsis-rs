//! Markdown parser (oracle: `internal/ingestion/parsers/markdown_parser.go`).
//!
//! Recursively walks a source tree for Markdown files and returns one
//! [`Document`] per file. Per-file and per-directory failures are collected
//! in [`ParseResult::errors`] and never abort the walk (design D1, oracle
//! contract).
//!
//! **Deliberate deviation from the oracle:** the Go parser matches only the
//! `.md` suffix. The task body (the source of truth here) requires both
//! `.md` and `.markdown`, so [`MarkdownParser::supported_extensions`]
//! reports both and the walk accepts both. The relative `source_file`,
//! `file_size`/`modified_at` metadata, and the best-effort error collection
//! follow the oracle. Exclusions differ by design (human decision
//! 2026-08-23): the oracle's hardcoded `skipDirs` list is replaced by
//! user `.synignore` files with gitignore semantics, inherited from the
//! shared walk.

use std::path::Path;

use crate::error::IngestionError;
use crate::parsers::{format_rfc3339_utc, source_file_name, walk_matched_files};
use crate::types::{Document, DocumentMetadata, ParseResult, Parser};

/// File extensions the markdown parser accepts (leading dot, as reported by
/// [`Parser::supported_extensions`]).
const MARKDOWN_EXTENSIONS: &[&str] = &[".md", ".markdown"];

/// Parses Markdown files out of a source tree.
///
/// Stateless: all inputs arrive through [`Parser::parse`], so the parser is a
/// `Copy` unit struct.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkdownParser;

impl MarkdownParser {
    /// True if `path` ends with a supported Markdown extension, case
    /// insensitively (oracle: `strings.ToLower(name)` + `HasSuffix(".md")`).
    fn is_markdown(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                MARKDOWN_EXTENSIONS
                    .iter()
                    .any(|wanted| ext.eq_ignore_ascii_case(&wanted[1..]))
            })
    }

    /// Reads one Markdown file into a [`Document`] (oracle `parseFile`).
    fn read_file(path: &Path, root: &Path) -> Result<Document, IngestionError> {
        let content = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // `metadata` is best-effort: a read that succeeded but a stat that
        // fails (e.g. the file vanished mid-walk) yields a document with
        // absent size/mtime rather than discarding the content we already
        // have. The oracle treated stat failure as fatal; degrading is the
        // more useful behavior and is recorded as a deviation.
        let meta = std::fs::metadata(path).ok();
        let file_size = meta.as_ref().map(|m| m.len());
        let modified_at = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(format_rfc3339_utc);

        Ok(Document {
            source_path: path.to_path_buf(),
            content,
            metadata: DocumentMetadata {
                source_type: "markdown".to_owned(),
                source_file: source_file_name(path, root),
                file_size,
                modified_at,
                extra: serde_json::Map::new(),
            },
        })
    }
}

impl Parser for MarkdownParser {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        walk_matched_files(
            source_path,
            Self::is_markdown,
            |path| {
                documents.push(Self::read_file(path, source_path)?);
                Ok(())
            },
            &mut errors,
        );
        ParseResult { documents, errors }
    }

    fn supported_extensions(&self) -> &[&str] {
        MARKDOWN_EXTENSIONS
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn supported_extensions_are_md_and_markdown() {
        let parser = MarkdownParser;
        assert_eq!(parser.supported_extensions(), [".md", ".markdown"]);
    }

    #[test]
    fn discovers_both_extensions_case_insensitively() {
        let tree = TempTree::new();
        tree.write("a.md", "# A");
        tree.write("b.markdown", "# B");
        tree.write("sub/C.MD", "# C uppercase");
        tree.write("notes.txt", "not markdown");
        tree.write("data.json", "{}");

        let result = MarkdownParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        // All three markdown files found (both extensions, case-insensitive),
        // non-markdown files ignored, in deterministic sorted order.
        assert_eq!(
            source_files(&result),
            vec!["a.md", "b.markdown", "sub/C.MD"]
        );
    }

    #[test]
    fn metadata_is_populated() {
        let tree = TempTree::new();
        let content = "# Heading\n\nBody text here";
        let path = tree.write("doc.md", content);

        let result = MarkdownParser.parse(&tree.0);
        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);

        let doc = &result.documents[0];
        assert_eq!(doc.source_path, path);
        assert_eq!(doc.content, content);
        assert_eq!(doc.metadata.source_type, "markdown");
        assert_eq!(doc.metadata.source_file, "doc.md");
        assert_eq!(doc.metadata.file_size, Some(content.len() as u64));
        assert!(
            doc.metadata.modified_at.is_some(),
            "modified_at should be set"
        );
        // RFC 3339 UTC: ends with 'Z', has a 'T' separator.
        let mtime = doc.metadata.modified_at.as_ref().unwrap();
        assert!(mtime.ends_with('Z'), "not RFC 3339 UTC: {mtime}");
        assert!(mtime.contains('T'), "not RFC 3339: {mtime}");
    }

    #[test]
    fn single_file_error_does_not_break_the_walk() {
        let tree = TempTree::new();
        tree.write("good.md", "# Good");

        // Invalid UTF-8: `read_to_string` fails, which must be collected as a
        // non-fatal error while the walk continues.
        let bad = tree.0.join("bad.md");
        fs::write(&bad, [0xff, 0xfe, 0x00]).expect("write invalid utf-8 file");

        tree.write("after.md", "# After");

        let result = MarkdownParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1, "exactly one non-fatal read error");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Io { ref path, .. } if path == &bad
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        // Both readable files are still parsed, in deterministic sorted order.
        let files = source_files(&result);
        assert_eq!(files, vec!["after.md", "good.md"]);
    }

    #[test]
    fn without_synignore_everything_is_walked() {
        // Human decision 2026-08-23: the hardcoded SKIP_DIRS list is gone;
        // without a .synignore, even system-looking directories are walked.
        let tree = TempTree::new();
        tree.write(".git/HEAD.md", "# walked");
        tree.write("node_modules/pkg/readme.md", "# walked");
        tree.write("visible.md", "# visible");

        let result = MarkdownParser.parse(&tree.0);

        assert!(result.errors.is_empty());
        let files = source_files(&result);
        assert_eq!(
            files,
            vec![".git/HEAD.md", "node_modules/pkg/readme.md", "visible.md"]
        );
    }

    #[test]
    fn synignore_exclusions_are_inherited_end_to_end() {
        let tree = TempTree::new();
        tree.write(".synignore", "generated/\n*.draft.md\n");
        tree.write("keep.md", "# kept");
        tree.write("generated/a.md", "# excluded directory");
        tree.write("notes.draft.md", "# excluded pattern");
        tree.write("sub/keep2.md", "# kept at depth");

        let result = MarkdownParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        let files = source_files(&result);
        assert_eq!(files, vec!["keep.md", "sub/keep2.md"]);
    }

    #[test]
    fn missing_root_is_a_non_fatal_error() {
        let tree = TempTree::new();
        let missing = tree.0.join("does-not-exist");

        let result = MarkdownParser.parse(&missing);

        assert!(result.documents.is_empty());
        assert_eq!(result.errors.len(), 1, "missing root -> one error");
    }

    #[test]
    fn single_file_source_is_parsed() {
        let tree = TempTree::new();
        let path = tree.write("standalone.md", "# Standalone");

        let result = MarkdownParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.md");
        assert_eq!(result.documents[0].content, "# Standalone");
    }

    #[test]
    fn empty_tree_yields_no_documents_and_no_errors() {
        let tree = TempTree::new();
        let result = MarkdownParser.parse(&tree.0);
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }
}
