//! JSON parser (oracle: `internal/ingestion/parsers/json_parser.go`).
//!
//! Recursively walks a source tree for `.json` files and returns one
//! [`Document`] per file. The document content is the raw file text — the
//! JSON chunker (task 1.5) re-parses it as an array of objects or a single
//! object. The top-level structure (`"array"`, `"object"` or `"unknown"`)
//! is detected at parse time and stored in the metadata extras (oracle
//! `detectStructure`). Per-file failures are collected in
//! [`ParseResult::errors`] and never abort the walk (design D1, oracle
//! contract).
//!
//! **Deliberate deviation from the oracle:** the Go parser never validates
//! JSON syntax — a broken file is ingested with structure `"unknown"` and
//! the failure surfaces later as a *hard* chunker error that aborts the
//! document (`ingester.go`). This parser validates up front: a
//! syntactically invalid file (including an empty file) yields a non-fatal
//! [`IngestionError::Json`] in [`ParseResult::errors`] and no document
//! (task 1.4 acceptance criterion), so the pipeline fails the file once, at
//! the stage that can name the real cause.

use std::path::Path;

use crate::error::IngestionError;
use crate::parsers::{format_rfc3339_utc, source_file_name, walk_matched_files};
use crate::types::{Document, DocumentMetadata, ParseResult, Parser};

/// File extensions the JSON parser accepts (leading dot, as reported by
/// [`Parser::supported_extensions`]).
const JSON_EXTENSIONS: &[&str] = &[".json"];

/// Metadata extra key holding the detected top-level JSON structure.
const STRUCTURE_KEY: &str = "structure";

/// Top-level JSON structure of a parsed file (oracle `detectStructure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Structure {
    /// `[ ... ]`
    Array,
    /// `{ ... }`
    Object,
    /// Any other valid JSON value (string, number, bool, null).
    Scalar,
}

impl Structure {
    /// The metadata string the oracle stored in `metadata["structure"]`.
    fn as_str(self) -> &'static str {
        match self {
            Structure::Array => "array",
            Structure::Object => "object",
            Structure::Scalar => "unknown",
        }
    }
}

/// Parses JSON files out of a source tree.
///
/// Stateless: all inputs arrive through [`Parser::parse`], so the parser is
/// a `Copy` unit struct.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonParser;

impl JsonParser {
    /// True if `path` ends with a supported JSON extension, case
    /// insensitively (oracle: `strings.ToLower(name)` + `HasSuffix(".json")`).
    fn is_json(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    }

    /// Validates `content` as JSON and returns its top-level structure.
    ///
    /// Returns the serde deserializer error on syntactically invalid JSON
    /// (the caller maps it to [`IngestionError::Json`]).
    fn detect_structure(content: &str) -> Result<Structure, serde_json::Error> {
        serde_json::from_str::<serde_json::Value>(content).map(|value| match value {
            serde_json::Value::Array(_) => Structure::Array,
            serde_json::Value::Object(_) => Structure::Object,
            _ => Structure::Scalar,
        })
    }

    /// Reads one JSON file into a [`Document`] (oracle `parseFile`).
    fn read_file(path: &Path, root: &Path) -> Result<Document, IngestionError> {
        let content = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Validate syntax and detect the top-level structure. Malformed JSON
        // is a non-fatal parse error with no document (see module docs).
        let structure =
            Self::detect_structure(&content).map_err(|source| IngestionError::Json {
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

        let mut extra = serde_json::Map::new();
        extra.insert(
            STRUCTURE_KEY.to_owned(),
            serde_json::Value::String(structure.as_str().to_owned()),
        );

        Ok(Document {
            source_path: path.to_path_buf(),
            content,
            metadata: DocumentMetadata {
                source_type: "json".to_owned(),
                source_file: source_file_name(path, root),
                file_size,
                modified_at,
                extra,
            },
        })
    }
}

impl Parser for JsonParser {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        walk_matched_files(
            source_path,
            Self::is_json,
            |path| {
                documents.push(Self::read_file(path, source_path)?);
                Ok(())
            },
            &mut errors,
        );
        ParseResult { documents, errors }
    }

    fn supported_extensions(&self) -> &[&str] {
        JSON_EXTENSIONS
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::parsers::tests::TempTree;

    fn source_files(result: &ParseResult) -> Vec<String> {
        result
            .documents
            .iter()
            .map(|d| d.metadata.source_file.clone())
            .collect()
    }

    fn structure_of(doc: &Document) -> String {
        doc.metadata
            .extra
            .get(STRUCTURE_KEY)
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_owned()
    }

    #[test]
    fn supported_extensions_are_json() {
        let parser = JsonParser;
        assert_eq!(parser.supported_extensions(), [".json"]);
    }

    #[test]
    fn object_file_produces_document_with_structure_object() {
        let tree = TempTree::new();
        let content = r#"{"title": "Test"}"#;
        let path = tree.write("data.json", content);

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);

        let doc = &result.documents[0];
        assert_eq!(doc.source_path, path);
        assert_eq!(doc.content, content);
        assert_eq!(doc.metadata.source_type, "json");
        assert_eq!(doc.metadata.source_file, "data.json");
        assert_eq!(doc.metadata.file_size, Some(content.len() as u64));
        assert!(
            doc.metadata.modified_at.is_some(),
            "modified_at should be set"
        );
        // RFC 3339 UTC: ends with 'Z', has a 'T' separator.
        let mtime = doc.metadata.modified_at.as_ref().unwrap();
        assert!(mtime.ends_with('Z'), "not RFC 3339 UTC: {mtime}");
        assert!(mtime.contains('T'), "not RFC 3339: {mtime}");
        assert_eq!(structure_of(doc), "object");
    }

    #[test]
    fn array_file_produces_document_with_structure_array() {
        let tree = TempTree::new();
        let content = r#"[{"id": 1}, {"id": 2}]"#;
        tree.write("items.json", content);

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].content, content);
        assert_eq!(structure_of(&result.documents[0]), "array");
    }

    #[test]
    fn empty_object_and_array_are_ingested() {
        // Oracle TestJSONParser_DetectStructure cases: `{}` -> object, `[]`
        // -> array (both are valid JSON and are ingested).
        let tree = TempTree::new();
        tree.write("obj.json", "{}");
        tree.write("arr.json", "[]");

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["arr.json", "obj.json"]);
        assert_eq!(structure_of(&result.documents[0]), "array");
        assert_eq!(structure_of(&result.documents[1]), "object");
    }

    #[test]
    fn valid_scalar_json_is_ingested_with_structure_unknown() {
        // Oracle detectStructure: first byte `4` is neither `[` nor `{` ->
        // "unknown"; the file is still ingested.
        let tree = TempTree::new();
        tree.write("scalar.json", "42");

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(structure_of(&result.documents[0]), "unknown");
    }

    #[test]
    fn non_json_files_are_skipped() {
        let tree = TempTree::new();
        tree.write("data.json", r#"{"key": "value"}"#);
        tree.write("readme.md", "# Hello");
        tree.write("notes.txt", "plain text");

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["data.json"]);
    }

    #[test]
    fn broken_json_is_a_non_fatal_error_without_document() {
        let tree = TempTree::new();
        tree.write("good.json", "{}");
        let bad = tree.write("broken.json", "not json");
        tree.write("after.json", "[]");

        let result = JsonParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1, "one non-fatal JSON error");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Json { ref path, .. } if path == &bad
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        // The walk continues: both valid files are still parsed, in
        // deterministic sorted order.
        assert_eq!(source_files(&result), vec!["after.json", "good.json"]);
    }

    #[test]
    fn empty_file_is_a_non_fatal_json_error() {
        // An empty file is not valid JSON: flagged up front (see module
        // docs) rather than ingested and hard-failing at the chunker.
        let tree = TempTree::new();
        let empty = tree.write("empty.json", "");
        tree.write("fine.json", "{}");

        let result = JsonParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1);
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Json { ref path, .. } if path == &empty
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        assert_eq!(source_files(&result), vec!["fine.json"]);
    }

    #[test]
    fn nested_files_are_walked_in_deterministic_order() {
        let tree = TempTree::new();
        // Deliberately non-sorted creation order.
        tree.write("z.json", "{}");
        tree.write("sub/b.json", "[]");
        tree.write("a.json", "{}");
        tree.write("sub/a.json", "[1]");

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec!["a.json", "sub/a.json", "sub/b.json", "z.json"]
        );
    }

    #[test]
    fn missing_root_is_a_non_fatal_error() {
        let tree = TempTree::new();
        let missing = tree.0.join("does-not-exist");

        let result = JsonParser.parse(&missing);

        assert!(result.documents.is_empty());
        assert_eq!(result.errors.len(), 1, "missing root -> one error");
    }

    #[test]
    fn single_file_source_is_parsed() {
        let tree = TempTree::new();
        let path = tree.write("standalone.json", "[1, 2, 3]");

        let result = JsonParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.json");
        assert_eq!(result.documents[0].content, "[1, 2, 3]");
        assert_eq!(structure_of(&result.documents[0]), "array");
    }

    #[test]
    fn synignore_exclusions_are_inherited() {
        let tree = TempTree::new();
        tree.write(".synignore", "generated/\n*.draft.json\n");
        tree.write("keep.json", "{}");
        tree.write("generated/a.json", "{}");
        tree.write("notes.draft.json", "{}");
        tree.write("sub/keep2.json", "[]");

        let result = JsonParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["keep.json", "sub/keep2.json"]);
    }

    #[test]
    fn empty_tree_yields_no_documents_and_no_errors() {
        let tree = TempTree::new();
        let result = JsonParser.parse(&tree.0);
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn detect_structure_recognizes_top_level_types() {
        // Oracle TestJSONParser_DetectStructure expectations, plus the valid
        // scalar cases that map to "unknown".
        assert_eq!(
            JsonParser::detect_structure(r#"{"key": "value"}"#).unwrap(),
            Structure::Object
        );
        assert_eq!(
            JsonParser::detect_structure("[1, 2, 3]").unwrap(),
            Structure::Array
        );
        assert_eq!(
            JsonParser::detect_structure("{}").unwrap(),
            Structure::Object
        );
        assert_eq!(
            JsonParser::detect_structure("[]").unwrap(),
            Structure::Array
        );
        assert_eq!(
            JsonParser::detect_structure("42").unwrap(),
            Structure::Scalar
        );
        assert_eq!(
            JsonParser::detect_structure("null").unwrap(),
            Structure::Scalar
        );
        assert!(
            JsonParser::detect_structure("not json").is_err(),
            "invalid JSON must surface a deserializer error"
        );
    }
}
