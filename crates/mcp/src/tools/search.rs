//! The `search` tool: hybrid (lexical + semantic) search over the knowledge
//! base (design D4).
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/search.go` plus the
//! `dbDomainValidator` in `../synopsis/internal/mcp/tools.go`.
//!
//! Thin handler per design D2: parse the frozen-schema arguments
//! (`query`, `top_k`, `domain`) → [`Searcher::hybrid_search`] → serialize
//! the oracle-shaped JSON (`results` / `total_count` / `search_time_ms` /
//! optional `warning`). The frozen schema exposes no search-mode argument,
//! so only the hybrid entry point is used.
//!
//! **Recorded deviations (error text):** the oracle prefixes its tool-error
//! messages with `"Error: "` (e.g. `"Error: 'query' argument is required
//! and must not be empty"`); this crate uses the [`McpError`] conventions
//! established by task 5.1 (e.g. `"invalid arguments for tool 'search':
//! …"`). The tool-error structure (is_error result with a text block) is
//! the same; internal message text is not part of the frozen contract.

use std::time::Instant;

use search::Searcher;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::McpError;

/// The frozen tool name (`mcp-contract`).
pub const TOOL_NAME: &str = "search";

/// `top_k` default (frozen schema: "default 10, range 1-100").
const DEFAULT_TOP_K: i32 = 10;
/// `top_k` minimum (frozen schema).
const MIN_TOP_K: i32 = 1;
/// `top_k` maximum (frozen schema).
const MAX_TOP_K: i32 = 100;

/// Parsed `search` arguments (frozen schema: `query`, `top_k`, `domain`).
/// Unknown keys are ignored, as in the oracle's `req.Get*` accessors.
#[derive(Debug, Deserialize)]
struct SearchArgs {
    /// The search query (required, non-empty).
    query: Option<String>,
    /// Maximum number of results (number, default 10, range 1-100).
    top_k: Option<Value>,
    /// Optional domain filter (empty = all domains).
    domain: Option<String>,
}

/// The `search` tool response (oracle `SearchResponse`): field order and
/// optionality match the Go struct, so the wire JSON matches the oracle's
/// marshal order.
#[derive(Debug, Serialize)]
struct SearchResponse {
    /// Ranked result chunks.
    results: Vec<ResultItem>,
    /// Number of results (always `results.len()`).
    total_count: usize,
    /// Wall-clock search duration in milliseconds.
    search_time_ms: u64,
    /// Set when the requested domain is unknown to the knowledge base.
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// One ranked result chunk (oracle `SearchResultItem`).
#[derive(Debug, Serialize)]
struct ResultItem {
    /// Owning document id.
    document_id: i64,
    /// Chunk row id.
    chunk_id: i64,
    /// The chunk text.
    text: String,
    /// Position of the chunk within its document.
    sequence_num: i64,
    /// Start offset in the original text, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset: Option<i64>,
    /// End offset in the original text, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    end_offset: Option<i64>,
    /// Document path (always present; empty until enriched).
    document_path: String,
    /// Final calibrated score (higher is better).
    score: f64,
    /// The wire source word (`"lexical" | "semantic" | "hybrid"`, possibly
    /// merged with the document source type by the enricher).
    source_type: String,
    /// Document domains, if the enricher filled them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    domains: Vec<String>,
    /// Entities attached to the chunk, if the enricher filled them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entities: Vec<EntityRef>,
}

/// A lightweight entity reference (oracle `EntityRef`).
#[derive(Debug, Serialize)]
struct EntityRef {
    /// Entity row id.
    id: i64,
    /// Entity name.
    name: String,
    /// Entity type.
    #[serde(rename = "type")]
    r#type: String,
}

/// Handle the `search` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the oracle-shaped payload the server serializes into the tool
/// response's text block.
pub fn handle_search(
    db: &db::Db,
    searcher: &(dyn Searcher + Send + Sync),
    args: Option<&Value>,
) -> Result<Value, McpError> {
    let args = parse_args(args)?;

    let query = args.query.unwrap_or_default();
    if query.is_empty() {
        return Err(McpError::InvalidArguments {
            tool: TOOL_NAME,
            reason: "'query' argument is required and must not be empty".to_owned(),
        });
    }

    let top_k = parse_top_k(args.top_k.as_ref());
    if !(MIN_TOP_K..=MAX_TOP_K).contains(&top_k) {
        return Err(McpError::InvalidArguments {
            tool: TOOL_NAME,
            reason: format!("'top_k' must be between {MIN_TOP_K} and {MAX_TOP_K}, got {top_k}"),
        });
    }

    let domain = args.domain.filter(|domain| !domain.is_empty());

    let started = Instant::now();
    let results = searcher.hybrid_search(&query, top_k, domain.as_deref())?;
    let search_time_ms = started.elapsed().as_millis() as u64;

    // Warn when the requested domain is unknown to the knowledge base
    // (oracle `dbDomainValidator`); the search itself is not affected.
    let warning = match domain.as_deref() {
        Some(domain) if !is_known_domain(db, domain)? => Some(format!(
            "unknown domain {domain:?}; results may be incomplete"
        )),
        _ => None,
    };

    let response = SearchResponse {
        results: results.iter().map(result_item).collect(),
        total_count: results.len(),
        search_time_ms,
        warning,
    };
    // All fields are primitives; a failure here is unreachable, but the
    // no-panic rule (design D7) keeps an error path instead of a panic.
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool: TOOL_NAME,
        reason: format!("serializing the search response failed: {err}"),
    })
}

/// Deserialize the argument object; `None` (no arguments) is the empty
/// object, so `query` is missing rather than a parse failure.
fn parse_args(args: Option<&Value>) -> Result<SearchArgs, McpError> {
    let value = args
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|err| McpError::InvalidArguments {
        tool: TOOL_NAME,
        reason: err.to_string(),
    })
}

/// Parse `top_k` (frozen schema: number, default 10). Mirrors the oracle's
/// `req.GetInt` leniency: missing or unparseable values fall back to the
/// default rather than erroring; floats truncate toward zero like Go's
/// `int(v)` conversion.
fn parse_top_k(value: Option<&Value>) -> i32 {
    let Some(top_k) = value.and_then(|value| match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    }) else {
        return DEFAULT_TOP_K;
    };
    top_k as i32
}

/// Whether `domain` is known to the knowledge base (oracle
/// `dbDomainValidator.IsKnownDomain`): the domain must appear as a
/// `$.domain` string or array member of some document's `metadata_json`.
/// Case-sensitive, as in the oracle's exact-match SQL.
fn is_known_domain(db: &db::Db, domain: &str) -> Result<bool, McpError> {
    let known = db
        .with_conn(|conn| {
            db::DocumentDao::new(db::ConnectionOrTx::Connection(conn)).unique_domains()
        })
        .and_then(|result| result)?;
    Ok(known.iter().any(|known| known == domain))
}

/// Map a fused search result to the wire item (oracle field mapping in
/// `handlers/search.go`, including the `metadata["domains"]` extraction).
fn result_item(result: &search::SearchResult) -> ResultItem {
    ResultItem {
        document_id: result.document_id,
        chunk_id: result.chunk_id,
        text: result.chunk_text.clone(),
        sequence_num: result.sequence_num,
        start_offset: result.start_offset,
        end_offset: result.end_offset,
        document_path: result.document_path.clone(),
        score: result.score,
        source_type: result.source_type.clone(),
        domains: result
            .metadata
            .get("domains")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        entities: result
            .entities
            .iter()
            .map(|entity| EntityRef {
                id: entity.id,
                name: entity.name.clone(),
                r#type: entity.entity_type.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use db::{ConnectionOrTx, DocumentDao, test_util};
    use search::{SearchError, SearchResult};

    use super::*;

    /// A Searcher stub with canned results (or a canned error) that records
    /// the arguments it received — the Rust form of the oracle's
    /// `mockSearcher` (which truncates its results to top_k).
    struct StubSearcher {
        results: Vec<SearchResult>,
        error: Mutex<Option<SearchError>>,
        recorded: Mutex<Option<(String, i32, Option<String>)>>,
    }

    impl StubSearcher {
        fn new(results: Vec<SearchResult>) -> Self {
            Self {
                results,
                error: Mutex::new(None),
                recorded: Mutex::new(None),
            }
        }

        fn failing(error: SearchError) -> Self {
            Self {
                results: Vec::new(),
                error: Mutex::new(Some(error)),
                recorded: Mutex::new(None),
            }
        }

        /// The (query, top_k, domain) triple passed to `hybrid_search`.
        fn recorded(&self) -> Option<(String, i32, Option<String>)> {
            self.recorded.lock().unwrap().clone()
        }
    }

    impl Searcher for StubSearcher {
        fn hybrid_search(
            &self,
            query: &str,
            top_k: i32,
            domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            *self.recorded.lock().unwrap() =
                Some((query.to_owned(), top_k, domain.map(str::to_owned)));
            if let Some(error) = self.error.lock().unwrap().take() {
                return Err(error);
            }
            // The oracle mock truncates to top_k (the real searcher does).
            Ok(self
                .results
                .iter()
                .take(top_k.max(0) as usize)
                .cloned()
                .collect())
        }

        fn lexical_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Lexical(
                "stub: lexical not used by the handler".to_owned(),
            ))
        }

        fn semantic_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Semantic(
                "stub: semantic not used by the handler".to_owned(),
            ))
        }
    }

    /// A canned fused result (oracle `TestHandleSearch_ResponseFields` shape).
    fn canned_result(chunk_id: i64) -> SearchResult {
        let mut metadata = serde_json::Map::new();
        metadata.insert("domains".to_owned(), serde_json::json!(["hr", "policy"]));
        SearchResult {
            chunk_id,
            chunk_text: format!("chunk {chunk_id} text"),
            document_id: chunk_id * 10,
            sequence_num: chunk_id,
            start_offset: Some(chunk_id * 100),
            end_offset: Some(chunk_id * 100 + 150),
            document_path: format!("/docs/{chunk_id}.md"),
            score: 0.85,
            rank: 1,
            source_type: "hybrid".to_owned(),
            metadata,
            entities: vec![db::Entity {
                id: chunk_id * 100,
                entity_type: "employee".to_owned(),
                name: "Alice".to_owned(),
                domain: "hr".to_owned(),
                description: None,
                confidence: None,
                metadata_json: None,
                created_at: "2026-01-01 00:00:00".to_owned(),
            }],
        }
    }

    /// An in-memory KB with documents covering the domains `hr`, `product`
    /// and `engineering` (string and array `$.domain` forms).
    fn seeded_db() -> db::Db {
        let db = test_util::in_memory_db();
        let seeded: Result<Result<(), db::DbError>, db::DbError> = db.with_conn(|conn| {
            let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
            documents.create("json", "/docs/hr.md", Some(r#"{"domain":"hr"}"#), None)?;
            documents.create(
                "json",
                "/docs/product.md",
                Some(r#"{"domain":["product","engineering"]}"#),
                None,
            )?;
            Ok(())
        });
        seeded.unwrap().unwrap();
        db
    }

    fn call(db: &db::Db, searcher: &StubSearcher, args: Option<Value>) -> Result<Value, McpError> {
        handle_search(db, searcher, args.as_ref())
    }

    // ── argument validation (oracle TestHandleSearch cases) ─────────────

    #[test]
    fn empty_query_is_an_error() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(Vec::new());
        let err = call(&db, &searcher, Some(serde_json::json!({ "query": "" }))).unwrap_err();
        assert!(
            err.to_string().contains("'query' argument is required"),
            "got: {err}"
        );
    }

    #[test]
    fn missing_query_is_an_error() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(Vec::new());
        for args in [None, Some(serde_json::json!({}))] {
            let err = call(&db, &searcher, args).unwrap_err();
            assert!(
                err.to_string().contains("'query' argument is required"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn top_k_out_of_range_is_an_error() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(Vec::new());
        for (top_k, got) in [
            (0, "got 0"),
            (-5, "got -5"),
            (101, "got 101"),
            (500, "got 500"),
        ] {
            let err = call(
                &db,
                &searcher,
                Some(serde_json::json!({ "query": "test", "top_k": top_k })),
            )
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("'top_k' must be between 1 and 100")
                    && err.to_string().contains(got),
                "top_k {top_k}: got {err}"
            );
        }
    }

    #[test]
    fn top_k_boundaries_pass() {
        let db = test_util::in_memory_db();
        for top_k in [1i64, 100] {
            let searcher = StubSearcher::new(vec![canned_result(1)]);
            let response = call(
                &db,
                &searcher,
                Some(serde_json::json!({ "query": "test", "top_k": top_k })),
            )
            .unwrap();
            assert_eq!(
                response["total_count"],
                serde_json::json!(1),
                "top_k {top_k}"
            );
            // The requested top_k reaches the searcher untouched.
            assert_eq!(searcher.recorded().unwrap().1, top_k as i32);
        }
    }

    #[test]
    fn missing_top_k_defaults_to_ten() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(Vec::new());
        call(&db, &searcher, Some(serde_json::json!({ "query": "test" }))).unwrap();
        assert_eq!(searcher.recorded().unwrap().1, 10);
    }

    // ── searcher results (oracle happy path / error / empty cases) ──────

    #[test]
    fn successful_search_shapes_the_oracle_response() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(vec![canned_result(1), canned_result(2)]);
        let response = call(
            &db,
            &searcher,
            Some(serde_json::json!({ "query": "employee policy", "top_k": 5 })),
        )
        .unwrap();

        assert_eq!(response["total_count"], serde_json::json!(2));
        assert!(response["search_time_ms"].is_u64());
        assert!(
            response.get("warning").is_none(),
            "no domain → no warning: {response}"
        );

        let item = &response["results"][0];
        assert_eq!(item["document_id"], serde_json::json!(10));
        assert_eq!(item["chunk_id"], serde_json::json!(1));
        assert_eq!(item["text"], "chunk 1 text");
        assert_eq!(item["sequence_num"], serde_json::json!(1));
        assert_eq!(item["start_offset"], serde_json::json!(100));
        assert_eq!(item["end_offset"], serde_json::json!(250));
        assert_eq!(item["document_path"], "/docs/1.md");
        assert_eq!(item["score"], serde_json::json!(0.85));
        assert_eq!(item["source_type"], "hybrid");
        assert_eq!(item["domains"], serde_json::json!(["hr", "policy"]));
        assert_eq!(item["entities"][0]["id"], serde_json::json!(100));
        assert_eq!(item["entities"][0]["name"], "Alice");
        assert_eq!(item["entities"][0]["type"], "employee");
    }

    #[test]
    fn empty_offsets_and_entities_are_omitted() {
        let db = test_util::in_memory_db();
        let mut result = canned_result(1);
        result.start_offset = None;
        result.end_offset = None;
        result.entities.clear();
        result.metadata.clear();
        let searcher = StubSearcher::new(vec![result]);
        let response = call(&db, &searcher, Some(serde_json::json!({ "query": "test" }))).unwrap();
        let item = &response["results"][0];
        assert!(item.get("start_offset").is_none());
        assert!(item.get("end_offset").is_none());
        assert!(item.get("domains").is_none());
        assert!(item.get("entities").is_none());
        // Always-present fields stay present even when empty.
        assert!(item.get("document_path").is_some());
        assert!(item.get("source_type").is_some());
    }

    #[test]
    fn empty_results_are_a_valid_zero_count_response() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::new(Vec::new());
        let response = call(
            &db,
            &searcher,
            Some(serde_json::json!({ "query": "nonexistent_xyz" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["results"], serde_json::json!([]));
    }

    #[test]
    fn searcher_error_is_a_tool_error() {
        let db = test_util::in_memory_db();
        let searcher = StubSearcher::failing(SearchError::BothSubSearchesFailed {
            lexical: "database connection failed".to_owned(),
            semantic: "embedding unavailable".to_owned(),
        });
        let err = call(&db, &searcher, Some(serde_json::json!({ "query": "test" }))).unwrap_err();
        assert!(matches!(err, McpError::Search(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("database connection failed"),
            "got: {err}"
        );
    }

    // ── domain handling (oracle TestHandleSearch_UnknownDomainWarning /
    //    TestHandleSearch_DomainFilter) ───────────────────────────────────

    #[test]
    fn unknown_domain_adds_a_warning() {
        let db = seeded_db();
        let searcher = StubSearcher::new(Vec::new());
        let response = call(
            &db,
            &searcher,
            Some(serde_json::json!({
                "query": "hiring",
                "domain": "nonexistent-domain"
            })),
        )
        .unwrap();
        assert_eq!(
            response["warning"],
            "unknown domain \"nonexistent-domain\"; results may be incomplete"
        );
    }

    #[test]
    fn known_domain_adds_no_warning() {
        let db = seeded_db();
        let searcher = StubSearcher::new(vec![canned_result(1)]);
        let response = call(
            &db,
            &searcher,
            Some(serde_json::json!({ "query": "hiring", "domain": "hr" })),
        )
        .unwrap();
        assert!(response.get("warning").is_none(), "{response}");
    }

    #[test]
    fn domain_is_forwarded_to_the_searcher() {
        let db = seeded_db();
        let searcher = StubSearcher::new(vec![canned_result(1)]);
        call(
            &db,
            &searcher,
            Some(serde_json::json!({ "query": "hiring", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(
            searcher.recorded().unwrap().2,
            Some("hr".to_owned()),
            "the raw domain must reach the searcher (it normalizes internally)"
        );

        let searcher = StubSearcher::new(Vec::new());
        call(
            &db,
            &searcher,
            Some(serde_json::json!({ "query": "hiring" })),
        )
        .unwrap();
        assert_eq!(searcher.recorded().unwrap().2, None);
    }
}
