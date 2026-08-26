//! End-to-end integration test (task 5.10): the full transport round-trip.
//!
//! Boots [`mcp::Server`] on an ephemeral localhost port (axum + rmcp
//! Streamable HTTP, design D1/D8), connects with the same rmcp SDK the
//! parity-harness uses (client role + reqwest-backed streamable-HTTP
//! transport), and drives the real wire path:
//! `initialize` → `tools/list` (exactly the 12 frozen tools) →
//! `tools/call` for `search` and `catalog_overview` against a seeded
//! in-memory KB → `GET /health` over plain HTTP on the same listener.
//!
//! Handler semantics are pinned by the per-tool unit tests; this test pins
//! only what those cannot see: the transport, the registry as served, the
//! tool-error shape over the wire, and the health endpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use db::{ChunkDao, ConnectionOrTx, DocumentDao, EntityDao, FactDao, test_util};
use graph::GraphIndex;
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
        Implementation,
    },
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
};
use search::{SearchError, SearchResult, Searcher};
use serde_json::{Map, Value, json};

/// The 12 frozen tool names (`mcp-contract`), sorted for set comparison.
const FROZEN_TOOL_NAMES: [&str; 12] = [
    "catalog_documents",
    "catalog_entities",
    "catalog_overview",
    "get_chunk_by_id",
    "get_document_context",
    "get_entity_dossier",
    "get_entity_links",
    "get_entity_relations",
    "get_fact_by_id",
    "search",
    "search_entities_by_type",
    "search_facts",
];

/// A Searcher stub returning one canned hit for the seeded chunk: the
/// transport is under test, not the search pipeline (the per-tool unit
/// tests cover handler semantics with their own stubs).
struct StubSearcher;

impl Searcher for StubSearcher {
    fn hybrid_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let mut metadata = Map::new();
        metadata.insert("domains".to_owned(), json!(["hr"]));
        Ok(vec![SearchResult {
            chunk_id: 1,
            chunk_text: "quarterly hiring policy".to_owned(),
            document_id: 1,
            sequence_num: 0,
            start_offset: Some(0),
            end_offset: Some(25),
            document_path: "/docs/hr-policy.md".to_owned(),
            score: 0.9,
            rank: 1,
            source_type: "hybrid".to_owned(),
            metadata,
            entities: Vec::new(),
        }])
    }

    fn lexical_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::Lexical(
            "stub: lexical not used by this test".to_owned(),
        ))
    }

    fn semantic_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::Semantic(
            "stub: semantic not used by this test".to_owned(),
        ))
    }
}

/// A seeded in-memory KB: 1 document (domain `hr`), 2 chunks, 2 entities,
/// 1 fact — the counts the `/health` and `catalog_overview` assertions
/// expect.
fn seeded_db() -> db::Db {
    let db = test_util::in_memory_db();
    let seeded = db.with_conn(|conn| -> Result<(), db::DbError> {
        let exec = ConnectionOrTx::Connection(conn);
        let doc = DocumentDao::new(exec).create(
            "markdown",
            "/docs/hr-policy.md",
            Some(r#"{"domain": "hr"}"#),
            None,
        )?;
        let chunks = ChunkDao::new(exec);
        chunks.create(doc, "quarterly hiring policy", 0, Some(0), Some(25))?;
        chunks.create(doc, "vacation rules", 1, None, None)?;
        let entities = EntityDao::new(exec);
        entities.create("employee", "Alice", "hr", None, None, None)?;
        entities.create("department", "Engineering", "hr", None, None, None)?;
        FactDao::new(exec).create(Some(1), "works_in", Some(2), "hr", None, None, None)?;
        Ok(())
    });
    seeded.expect("pool checkout").expect("seed rows");
    db
}

/// The integration server over the seeded KB: identity
/// `synopsis-integration 9.9.9`, stub searcher, no graph (the Unavailable
/// state degrades the graph tools; this test does not exercise them).
fn integration_server() -> mcp::Server {
    mcp::Server::new(
        "synopsis-integration".to_owned(),
        "9.9.9".to_owned(),
        seeded_db(),
        Arc::new(StubSearcher),
        Arc::new(GraphIndex::Unavailable),
    )
}

/// Boot the server on an ephemeral localhost port; returns the base URL
/// (the MCP transport is the fallback on any path, `/health` is the
/// explicit route) and a shutdown trigger for graceful axum termination.
async fn spawn_server(server: mcp::Server) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("http://{addr}"), shutdown_tx)
}

/// Connect and complete the `initialize` handshake against the Streamable
/// HTTP endpoint (the parity-harness pattern, inlined: the test needs plain
/// rmcp, no timing instrumentation).
async fn connect(url: &str) -> RunningService<RoleClient, ClientInfo> {
    let transport = StreamableHttpClientTransport::from_uri(url);
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("synopsis-mcp-integration-test", env!("CARGO_PKG_VERSION")),
    )
    .serve(transport)
    .await
    .unwrap()
}

/// The tool response's single text block, parsed as JSON (the server
/// serializes the oracle-shaped payload into one text block).
fn payload_json(result: &CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("a text content block");
    serde_json::from_str(&text.text).expect("the payload is JSON")
}

/// The joined text of a tool-error result (for message assertions).
fn error_detail(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(ContentBlock::as_text)
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

#[tokio::test]
async fn full_roundtrip_lists_twelve_tools_and_serves_search_catalog_health() {
    let (base, shutdown) = spawn_server(integration_server()).await;
    let mut client = connect(&format!("{base}/mcp")).await;

    // initialize: the negotiated identity is the server's name/version.
    let peer = client.peer_info().expect("peer info after initialize");
    let info = peer.server_info.as_ref().expect("server info");
    assert_eq!(info.name, "synopsis-integration");
    assert_eq!(info.version, "9.9.9");

    // tools/list: exactly the 12 frozen tools, no more, no fewer.
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, FROZEN_TOOL_NAMES);

    // search: the canned hit comes back through the wire, oracle-shaped.
    let mut args = Map::new();
    args.insert("query".to_owned(), Value::String("hiring policy".into()));
    args.insert("top_k".to_owned(), Value::from(5));
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    let payload = payload_json(&result);
    assert_eq!(payload["total_count"], json!(1));
    assert!(payload["search_time_ms"].is_u64());
    assert!(payload.get("warning").is_none(), "{payload}");
    let item = &payload["results"][0];
    assert_eq!(item["chunk_id"], json!(1));
    assert_eq!(item["document_id"], json!(1));
    assert_eq!(item["text"], "quarterly hiring policy");
    assert_eq!(item["document_path"], "/docs/hr-policy.md");
    assert_eq!(item["source_type"], "hybrid");
    assert_eq!(item["domains"], json!(["hr"]));

    // catalog_overview: the seeded counts, over the wire.
    let result = client
        .call_tool(CallToolRequestParams::new("catalog_overview").with_arguments(Map::new()))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    let overview = payload_json(&result);
    assert_eq!(overview["document_count"], json!(1));
    assert_eq!(overview["chunk_count"], json!(2));
    assert_eq!(overview["entity_count"], json!(2));
    assert_eq!(overview["fact_count"], json!(1));
    assert_eq!(overview["domains"], json!(["hr"]));
    assert_eq!(overview["documents_by_type"]["markdown"], json!(1));

    // Tool error over the wire: a missing chunk is an is_error result with
    // the human-readable message (design D7), not a protocol error.
    let mut args = Map::new();
    args.insert("chunk_id".to_owned(), Value::String("999".into()));
    let result = client
        .call_tool(CallToolRequestParams::new("get_chunk_by_id").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let detail = error_detail(&result);
    assert!(detail.contains("not found"), "got: {detail}");

    // GET /health on the same listener: design D5 shape with the seeded
    // counters.
    let health: Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], "9.9.9");
    assert_eq!(health["sync_state"], "idle");
    assert_eq!(health["counters"]["documents"], json!(1));
    assert_eq!(health["counters"]["chunks"], json!(2));
    assert_eq!(health["counters"]["entities"], json!(2));
    assert_eq!(health["counters"]["facts"], json!(1));

    client.close().await.unwrap();
    shutdown.send(()).unwrap();
}
