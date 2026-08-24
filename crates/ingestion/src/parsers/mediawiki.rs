//! Mediawiki parser (oracle: `internal/ingestion/parsers/mediawiki_parser.go`).
//!
//! Walks a mediawiki dataset tree and returns one [`Document`] per page JSON
//! file. Expected layout (oracle):
//!
//! ```text
//! sourcePath/
//!   <space>/
//!     <wiki-type>/
//!       by-type/
//!         <entity-type>/
//!           <page-name>.json
//!       graph.json (optional) — title -> related titles
//! ```
//!
//! A page file is a JSON object with optional `title`, `url`, `wikitext`,
//! `html`, `images`, `links`, `categories`, `entity_type` and `description`
//! fields. The document content is the best available text (oracle
//! `extractContent`): `wikitext` > `html` > `title` + `description` joined by
//! a blank line (empty parts dropped).
//!
//! `graph.json` files (at any depth) are relations, not pages: every one is
//! parsed as a title -> related titles map and merged into a single map that
//! enriches each page's metadata (`graph_relations`).
//!
//! **Deliberate deviations from the oracle** (functional copy, not code
//! copy):
//!
//! * **Exclusions** come from user `.synignore` files (human decision
//!   2026-08-23) via the shared walk; the oracle's hardcoded `skipDirs`
//!   list does not exist here.
//! * **Malformed `graph.json`** files yield a non-fatal
//!   [`IngestionError::Json`] (the oracle skipped them silently), consistent
//!   with the JSON parser's up-front validation (task 1.4).
//! * **`entity_type` fallback**: the oracle derived it from the `by-type/`
//!   path layer only — the page JSON's own `entity_type` field was parsed
//!   but never used (a dead field). When the path layer is absent, the JSON
//!   field is used instead.
//! * **`file_size`/`modified_at`** are populated like the sibling parsers
//!   (the oracle's mediawiki parser left them unset).
//! * A missing root is reported once, not once per pass (the oracle's second
//!   `loadGraphJSON` walk would have added a duplicate error).
//! * The oracle's `GraphEdge` struct is dead code (the graph is a plain
//!   title -> titles map) and is not ported.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::IngestionError;
use crate::parsers::{format_rfc3339_utc, source_file_name, walk_matched_files};
use crate::types::{Document, DocumentMetadata, ParseResult, Parser};

/// File extensions the mediawiki parser accepts (oracle: `.json` page dumps).
const MEDIAWIKI_EXTENSIONS: &[&str] = &[".json"];

/// File name (case-insensitive) of a relations file: walked for
/// `graph_relations`, never ingested as a page (oracle `loadGraphJSON`).
const GRAPH_FILE_NAME: &str = "graph.json";

/// Path layer that names a page's entity type (oracle `extractPathComponents`).
const BY_TYPE_DIR: &str = "by-type";

/// One mediawiki page as stored in the dataset (oracle `MediawikiPage`).
///
/// Every field is optional: a missing field degrades to an empty content
/// component exactly like the oracle's zero values. Unknown fields are
/// ignored, as Go's `json.Unmarshal` did.
#[derive(Debug, Default, Deserialize)]
struct Page {
    /// Page title.
    title: Option<String>,
    /// Page URL.
    url: Option<String>,
    /// Wikitext body — the preferred content.
    wikitext: Option<String>,
    /// Rendered HTML — the fallback content.
    html: Option<String>,
    /// Image paths referenced by the page.
    images: Option<Vec<String>>,
    /// Links to other pages.
    links: Option<Vec<String>>,
    /// Categories the page belongs to.
    categories: Option<Vec<String>>,
    /// Entity type declared by the dataset (path-layer fallback, module docs).
    entity_type: Option<String>,
    /// Short description — last-resort content.
    description: Option<String>,
}

/// Parses mediawiki page JSON files out of a dataset tree.
///
/// Stateless: all inputs arrive through [`Parser::parse`], so the parser is
/// a `Copy` unit struct.
#[derive(Debug, Default, Clone, Copy)]
pub struct MediawikiParser;

impl MediawikiParser {
    /// True for a page file: a `.json` file that is not a `graph.json`.
    fn is_page_json(path: &Path) -> bool {
        is_json_file(path) && !is_graph_file(path)
    }

    /// Merges every `graph.json` in the tree into title -> related titles
    /// (oracle `loadGraphJSON`). Deterministic: sorted walk, BTreeMap
    /// storage, and within one file the BTreeMap-backed serde_json key order.
    fn load_graph(
        source_path: &Path,
        errors: &mut Vec<IngestionError>,
    ) -> BTreeMap<String, Vec<String>> {
        let mut graph = BTreeMap::new();
        walk_matched_files(
            source_path,
            is_graph_file,
            |path| {
                let data = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                merge_graph(&mut graph, &data, path)?;
                Ok(())
            },
            errors,
        );
        graph
    }

    /// Reads one page JSON file into a [`Document`] (oracle `parsePageFile`).
    fn read_page(
        path: &Path,
        root: &Path,
        graph: &BTreeMap<String, Vec<String>>,
    ) -> Result<Document, IngestionError> {
        let data = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let page: Page = serde_json::from_str(&data).map_err(|source| IngestionError::Json {
            path: path.to_path_buf(),
            source,
        })?;

        let content = extract_content(&page);
        let (space, wiki_type, path_entity) = path_components(path, root);
        let title = page.title.clone().unwrap_or_default();

        let mut extra = Map::new();
        extra.insert("title".to_owned(), Value::String(title.clone()));
        if let Some(url) = &page.url {
            insert_str(&mut extra, "url", url);
        }
        if let Some(space) = &space {
            insert_str(&mut extra, "space", space);
        }
        if let Some(wiki_type) = &wiki_type {
            insert_str(&mut extra, "wiki_type", wiki_type);
        }
        // Path layer first, the page JSON field as fallback (module docs).
        let entity_type = path_entity.or(page.entity_type.clone()).unwrap_or_default();
        insert_str(&mut extra, "entity_type", &entity_type);
        if let Some(links) = &page.links {
            insert_strs(&mut extra, "page_links", links);
        }
        if let Some(images) = &page.images {
            insert_strs(&mut extra, "image_paths", images);
        }
        if let Some(categories) = &page.categories {
            insert_strs(&mut extra, "categories", categories);
        }
        // Graph relations keyed by title (absent when the title is not a
        // graph node — oracle contract).
        if let Some(relations) = graph.get(&title) {
            insert_strs(&mut extra, "graph_relations", relations);
        }

        // `metadata` is best-effort: a read that succeeded but a stat that
        // fails (e.g. the file vanished mid-walk) yields a document with
        // absent size/mtime rather than discarding the content we already
        // have (same degradation as the sibling parsers).
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
                source_type: "mediawiki".to_owned(),
                source_file: source_file_name(path, root),
                file_size,
                modified_at,
                extra,
            },
        })
    }
}

impl Parser for MediawikiParser {
    fn parse(&self, source_path: &Path) -> ParseResult {
        // A missing/unreadable root is reported once, not once per pass
        // (module docs).
        if let Err(source) = std::fs::symlink_metadata(source_path) {
            return ParseResult {
                documents: Vec::new(),
                errors: vec![IngestionError::Io {
                    path: source_path.to_path_buf(),
                    source,
                }],
            };
        }

        // First pass: relations (oracle `loadGraphJSON` before the page walk).
        let mut errors = Vec::new();
        let graph = Self::load_graph(source_path, &mut errors);
        let mut documents = Vec::new();
        walk_matched_files(
            source_path,
            Self::is_page_json,
            |path| {
                documents.push(Self::read_page(path, source_path, &graph)?);
                Ok(())
            },
            &mut errors,
        );
        ParseResult { documents, errors }
    }

    fn supported_extensions(&self) -> &[&str] {
        MEDIAWIKI_EXTENSIONS
    }
}

/// The best available text (oracle `extractContent`): `wikitext` > `html` >
/// `title` + `description` joined by a blank line, empty parts dropped.
/// Original (untrimmed) text is kept — the oracle trimmed only for the
/// emptiness filter.
fn extract_content(page: &Page) -> String {
    for text in [page.wikitext.as_deref(), page.html.as_deref()]
        .into_iter()
        .flatten()
    {
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in [
        page.title.as_deref().unwrap_or(""),
        page.description.as_deref().unwrap_or(""),
    ] {
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// `(space, wiki_type, entity_type)` from the path layout
/// `<space>/<wiki-type>/by-type/<entity-type>/page.json` (oracle
/// `extractPathComponents`). Each component is `None` when the layout does
/// not provide it; a single-file source (no walk root) provides none.
fn path_components(path: &Path, root: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(rel) = path.strip_prefix(root) else {
        return (None, None, None);
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let space = (parts.len() >= 2).then(|| parts[0].clone());
    let wiki_type = (parts.len() >= 3).then(|| parts[1].clone());
    let entity_type = (parts.len() >= 5
        && parts[parts.len() - 3].eq_ignore_ascii_case(BY_TYPE_DIR))
    .then(|| parts[parts.len() - 2].clone());
    (space, wiki_type, entity_type)
}

/// Merges one `graph.json` (title -> related titles) into `graph`
/// (oracle `loadGraphJSON` merge). A file whose shape is not
/// `map[string][]string` is rejected whole — the oracle's
/// `json.Unmarshal` into that type failed the file the same way.
fn merge_graph(
    graph: &mut BTreeMap<String, Vec<String>>,
    data: &str,
    path: &Path,
) -> Result<(), IngestionError> {
    let parsed: Map<String, Value> =
        serde_json::from_str(data).map_err(|source| IngestionError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    for (title, targets) in &parsed {
        let Some(list) = targets.as_array() else {
            return Err(shape_error(path, title));
        };
        let relations: Option<Vec<&str>> = list.iter().map(Value::as_str).collect();
        let Some(relations) = relations else {
            return Err(shape_error(path, title));
        };
        graph
            .entry(title.clone())
            .or_default()
            .extend(relations.into_iter().map(str::to_owned));
    }
    Ok(())
}

/// A `graph.json` entry that is not an array of strings.
///
/// `serde_json::Error` has no public `custom` constructor; an
/// `InvalidData` io error carries the same message through
/// [`IngestionError::Json`]'s display.
fn shape_error(path: &Path, title: &str) -> IngestionError {
    IngestionError::Json {
        path: path.to_path_buf(),
        source: serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("graph entry {title:?} is not an array of strings"),
        )),
    }
}

/// True for a `.json` file, case-insensitively (oracle:
/// `strings.ToLower(name)` + `HasSuffix(".json")`).
fn is_json_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// True for a relations file named exactly `graph.json` (case-insensitive;
/// oracle `strings.EqualFold(name, "graph.json")`).
fn is_graph_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(GRAPH_FILE_NAME))
}

/// Inserts a non-empty string extra (presence means meaning).
fn insert_str(extra: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        extra.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

/// Inserts a non-empty string list as a JSON array extra.
fn insert_strs(extra: &mut Map<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        extra.insert(
            key.to_owned(),
            Value::Array(values.iter().map(|v| Value::String(v.clone())).collect()),
        );
    }
}

#[cfg(test)]
mod tests {
    //! Differential tests against the Go oracle: inputs and expectations
    //! (document counts, content priority, path components, graph relations)
    //! are taken from
    //! `../synopsis/internal/ingestion/parsers/mediawiki_parser_test.go`,
    //! which passes there (`go test ./internal/ingestion/parsers/`).

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::parsers::tests::TempTree;

    /// The oracle's "single page" fixture.
    const PAGE_JSON: &str = r#"{
        "title": "API Gateway",
        "url": "https://example.com/API_Gateway",
        "wikitext": "== API Gateway ==\nA service mesh component.",
        "html": "<div>HTML content</div>",
        "images": ["gateway.png"],
        "links": ["Service Catalog"],
        "categories": ["Services", "Networking"],
        "entity_type": "service"
    }"#;

    fn source_files(result: &ParseResult) -> Vec<String> {
        result
            .documents
            .iter()
            .map(|d| d.metadata.source_file.clone())
            .collect()
    }

    fn extra_str(doc: &Document, key: &str) -> Option<String> {
        doc.metadata
            .extra
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn extra_strings(doc: &Document, key: &str) -> Vec<String> {
        doc.metadata
            .extra
            .get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn content_of(result: &ParseResult, file: &str) -> Option<String> {
        result
            .documents
            .iter()
            .find(|d| d.metadata.source_file == file)
            .map(|d| d.content.clone())
    }

    #[test]
    fn supported_extensions_are_json() {
        let parser = MediawikiParser;
        assert_eq!(parser.supported_extensions(), [".json"]);
    }

    #[test]
    fn single_page_produces_one_document() {
        // Oracle TestMediawikiParser_Parse "single page".
        let tree = TempTree::new();
        let path = tree.write(
            "space/wiki-type/by-type/services/api_gateway.json",
            PAGE_JSON,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        let doc = &result.documents[0];
        assert_eq!(doc.source_path, path);
        assert_eq!(doc.metadata.source_type, "mediawiki");
        assert_eq!(
            doc.metadata.source_file,
            "space/wiki-type/by-type/services/api_gateway.json"
        );
        // wikitext wins over html (oracle extractContent priority).
        assert_eq!(doc.content, "== API Gateway ==\nA service mesh component.");
        assert_eq!(extra_str(doc, "title"), Some("API Gateway".into()));
        assert_eq!(
            extra_str(doc, "url"),
            Some("https://example.com/API_Gateway".into())
        );
        assert_eq!(extra_str(doc, "space"), Some("space".into()));
        assert_eq!(extra_str(doc, "wiki_type"), Some("wiki-type".into()));
        assert_eq!(extra_str(doc, "entity_type"), Some("services".into()));
        assert_eq!(extra_strings(doc, "page_links"), vec!["Service Catalog"]);
        assert_eq!(extra_strings(doc, "image_paths"), vec!["gateway.png"]);
        assert_eq!(
            extra_strings(doc, "categories"),
            vec!["Services", "Networking"]
        );
        assert!(doc.metadata.file_size.is_some());
        assert!(doc.metadata.modified_at.is_some());
    }

    #[test]
    fn graph_json_is_not_a_page() {
        // Oracle TestMediawikiParser_Parse "skip graph.json".
        let tree = TempTree::new();
        tree.write("space/wiki-type/graph.json", r#"{"page1": ["page2"]}"#);
        tree.write(
            "space/wiki-type/by-type/systems/database.json",
            r#"{"title": "Database", "wikitext": "A data storage system."}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 1);
        assert_eq!(
            source_files(&result),
            vec!["space/wiki-type/by-type/systems/database.json"]
        );
    }

    #[test]
    fn content_falls_back_html_then_title_description() {
        // Oracle TestMediawikiParser_ExtractContent (html fallback and
        // title + description last resort).
        let tree = TempTree::new();
        tree.write(
            "a/html_only.json",
            r#"{"title": "T", "html": "<p>Fallback HTML</p>"}"#,
        );
        tree.write(
            "b/last_resort.json",
            r#"{"title": "Just Title", "description": "With description."}"#,
        );
        tree.write("c/empty.json", "{}");

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            content_of(&result, "a/html_only.json"),
            Some("<p>Fallback HTML</p>".into())
        );
        assert_eq!(
            content_of(&result, "b/last_resort.json"),
            Some("Just Title\n\nWith description.".into())
        );
        assert_eq!(content_of(&result, "c/empty.json"), Some(String::new()));
    }

    #[test]
    fn extract_content_prefers_wikitext_and_drops_empty_parts() {
        let page = Page {
            title: Some("Title".into()),
            html: Some("<div>HTML</div>".into()),
            wikitext: Some("== Wiki Content ==".into()),
            ..Default::default()
        };
        assert_eq!(extract_content(&page), "== Wiki Content ==");

        // Empty parts are dropped from the join (oracle TrimSpace filter).
        let page = Page {
            title: Some("   ".into()),
            description: Some("Only description".into()),
            ..Default::default()
        };
        assert_eq!(extract_content(&page), "Only description");

        let page = Page::default();
        assert_eq!(extract_content(&page), "");
    }

    #[test]
    fn path_components_follow_the_layout() {
        // Oracle TestMediawikiParser_ExtractPathComponents "full path",
        // plus the shallow layouts the oracle's guards cover.
        assert_eq!(
            path_components(
                Path::new("/data/devmix/internal/by-type/services/api_gateway.json"),
                Path::new("/data"),
            ),
            (
                Some("devmix".to_owned()),
                Some("internal".to_owned()),
                Some("services".to_owned()),
            )
        );
        // A 2-part path still provides the space (oracle: `len(parts) >= 2`).
        assert_eq!(
            path_components(Path::new("/data/space/page.json"), Path::new("/data")),
            (Some("space".into()), None, None)
        );
        assert_eq!(
            path_components(Path::new("/data/space/wiki/page.json"), Path::new("/data")),
            (Some("space".into()), Some("wiki".into()), None)
        );
        // by-type match is case-insensitive (oracle EqualFold).
        assert_eq!(
            path_components(
                Path::new("/data/s/w/By-Type/entities/p.json"),
                Path::new("/data")
            ),
            (Some("s".into()), Some("w".into()), Some("entities".into()))
        );
        // Unrelated root (single-file source): no components.
        assert_eq!(
            path_components(Path::new("/x/p.json"), Path::new("/y")),
            (None, None, None)
        );
    }

    #[test]
    fn graph_relations_enrich_page_metadata() {
        // Oracle TestMediawikiParser_GraphJSON.
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
            r#"{"title": "API Gateway", "wikitext": "== API Gateway ==", "entity_type": "service"}"#,
        );
        tree.write(
            "space/wiki-type/by-type/systems/database.json",
            r#"{"title": "Database", "wikitext": "== Database ==", "entity_type": "system"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.documents.len(), 2);
        assert_eq!(
            extra_strings(&result.documents[0], "graph_relations"),
            vec!["Service Catalog", "Load Balancer"]
        );
        assert_eq!(
            extra_strings(&result.documents[1], "graph_relations"),
            vec!["Storage"]
        );
    }

    #[test]
    fn multiple_graph_files_merge() {
        // Oracle TestMediawikiParser_LoadGraphJSON (the append-merge of
        // `result[source] = append(...)` across graph files).
        let tree = TempTree::new();
        tree.write("space/a/graph.json", r#"{"A": ["B", "C"], "D": ["E"]}"#);
        tree.write("space/b/graph.json", r#"{"A": ["F"]}"#);
        tree.write(
            "space/a/by-type/x/page.json",
            r#"{"title": "A", "wikitext": "x"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            extra_strings(&result.documents[0], "graph_relations"),
            vec!["B", "C", "F"]
        );
    }

    #[test]
    fn without_graph_no_relations_key() {
        // Oracle TestMediawikiParser_LoadGraphJSON_Missing.
        let tree = TempTree::new();
        tree.write(
            "space/by-type/x/page.json",
            r#"{"title": "P", "wikitext": "text"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty());
        assert!(
            result.documents[0]
                .metadata
                .extra
                .get("graph_relations")
                .is_none()
        );
    }

    #[test]
    fn broken_page_json_is_non_fatal() {
        let tree = TempTree::new();
        tree.write("space/good.json", r#"{"title": "Good", "wikitext": "ok"}"#);
        let bad = tree.write("space/broken.json", "not json");

        let result = MediawikiParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1, "one non-fatal JSON error");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Json { ref path, .. } if path == &bad
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        assert_eq!(source_files(&result), vec!["space/good.json"]);
    }

    #[test]
    fn broken_graph_json_is_non_fatal_and_recorded() {
        // Deviation (module docs): the oracle skipped malformed graph files
        // silently; here the error is recorded and pages still parse.
        let tree = TempTree::new();
        let bad = tree.write("space/graph.json", "{invalid");
        tree.write(
            "space/by-type/x/page.json",
            r#"{"title": "P", "wikitext": "text"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1, "one non-fatal graph error");
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Json { ref path, .. } if path == &bad
            ),
            "error must carry the failing path, got {:?}",
            result.errors[0]
        );
        assert_eq!(source_files(&result), vec!["space/by-type/x/page.json"]);
        assert!(
            result.documents[0]
                .metadata
                .extra
                .get("graph_relations")
                .is_none()
        );
    }

    #[test]
    fn graph_entries_must_be_string_arrays() {
        // Oracle: `json.Unmarshal` into `map[string][]string` fails the
        // whole file when a value is not a string array.
        let tree = TempTree::new();
        let bad = tree.write("space/graph.json", r#"{"A": "not-an-array"}"#);
        tree.write(
            "space/by-type/x/page.json",
            r#"{"title": "A", "wikitext": "x"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert_eq!(result.errors.len(), 1);
        assert!(
            matches!(
                result.errors[0],
                IngestionError::Json { ref path, .. } if path == &bad
            ),
            "got {:?}",
            result.errors[0]
        );
        assert_eq!(source_files(&result), vec!["space/by-type/x/page.json"]);
        assert!(
            result.documents[0]
                .metadata
                .extra
                .get("graph_relations")
                .is_none()
        );
    }

    #[test]
    fn entity_type_falls_back_to_the_page_field() {
        // Deviation (module docs): without the by-type/ path layer the page
        // JSON's own entity_type field is used (the oracle left it dead).
        let tree = TempTree::new();
        tree.write(
            "space/wiki/loose.json",
            r#"{"title": "P", "wikitext": "x", "entity_type": "service"}"#,
        );

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty());
        assert_eq!(
            extra_str(&result.documents[0], "entity_type"),
            Some("service".into())
        );
    }

    #[test]
    fn non_json_files_are_skipped() {
        let tree = TempTree::new();
        tree.write("page.json", r#"{"title": "P", "wikitext": "x"}"#);
        tree.write("readme.md", "# Hello");
        tree.write("notes.txt", "plain text");

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["page.json"]);
    }

    #[test]
    fn nested_pages_are_walked_in_deterministic_order() {
        let tree = TempTree::new();
        // Deliberately non-sorted creation order.
        tree.write("space/zeta/page.json", r#"{"title": "Z"}"#);
        tree.write("space/alpha/page.json", r#"{"title": "A"}"#);
        tree.write("space/alpha/deep/page.json", r#"{"title": "AD"}"#);
        tree.write("space/beta/page.json", r#"{"title": "B"}"#);

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(
            source_files(&result),
            vec![
                "space/alpha/deep/page.json",
                "space/alpha/page.json",
                "space/beta/page.json",
                "space/zeta/page.json",
            ]
        );
    }

    #[test]
    fn synignore_exclusions_are_inherited() {
        let tree = TempTree::new();
        tree.write(".synignore", "generated/\n*.draft.json\n");
        tree.write("keep.json", r#"{"title": "K"}"#);
        tree.write("generated/a.json", r#"{"title": "X"}"#);
        tree.write("notes.draft.json", r#"{"title": "D"}"#);

        let result = MediawikiParser.parse(&tree.0);

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(source_files(&result), vec!["keep.json"]);
    }

    #[test]
    fn missing_root_is_a_single_non_fatal_error() {
        let tree = TempTree::new();
        let missing = tree.0.join("does-not-exist");

        let result = MediawikiParser.parse(&missing);

        assert!(result.documents.is_empty());
        assert_eq!(result.errors.len(), 1, "missing root -> one error");
    }

    #[test]
    fn single_file_source_is_parsed() {
        let tree = TempTree::new();
        let path = tree.write("standalone.json", r#"{"title": "S", "wikitext": "body"}"#);

        let result = MediawikiParser.parse(&path);

        assert!(result.errors.is_empty());
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].metadata.source_file, "standalone.json");
        assert_eq!(result.documents[0].content, "body");
    }

    #[test]
    fn single_graph_file_source_yields_no_pages() {
        let tree = TempTree::new();
        let path = tree.write("graph.json", r#"{"A": ["B"]}"#);

        let result = MediawikiParser.parse(&path);

        assert!(result.errors.is_empty());
        assert!(result.documents.is_empty());
    }

    #[test]
    fn empty_tree_yields_no_documents_and_no_errors() {
        let tree = TempTree::new();
        let result = MediawikiParser.parse(&tree.0);
        assert!(result.documents.is_empty());
        assert!(result.errors.is_empty());
    }
}
