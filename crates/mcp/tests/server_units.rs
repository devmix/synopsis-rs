//! Unit tests for the MCP server (extracted from `src/server.rs` by
//! test-hygiene-phase-2 task 2.3). The 8 tests previously lived in the
//! inline `#[cfg(test)]` module; they now run against the public API
//! (`mcp::server::{Server, tool_definitions}`, `mcp::McpError`) plus the
//! crate's production and dev dependencies (axum, rmcp, tower, tokio).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use db::test_util;
use graph::GraphIndex;
use rmcp::ServerHandler;
use search::{SearchError, SearchResult, Searcher};
use serde_json::{Map, Value, json};

use mcp::McpError;
use mcp::server::{Server, tool_definitions};

/// The 12 frozen tool names (`mcp-contract` "Набор инструментов"), in
/// registration order.
const FROZEN_NAMES: [&str; 12] = [
    "search",
    "catalog_overview",
    "catalog_documents",
    "catalog_entities",
    "search_entities_by_type",
    "search_facts",
    "get_document_context",
    "get_chunk_by_id",
    "get_fact_by_id",
    "get_entity_dossier",
    "get_entity_relations",
    "get_entity_links",
];

/// A Searcher stub: the scaffold only needs an injectable handle.
struct StubSearcher;

impl Searcher for StubSearcher {
    fn hybrid_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::Lexical("stub".to_owned()))
    }

    fn lexical_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::Lexical("stub".to_owned()))
    }

    fn semantic_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::Semantic("stub".to_owned()))
    }
}

/// A scaffold server over an empty in-memory KB with no graph.
fn test_server() -> Server {
    Server::new(
        "synopsis-test".to_owned(),
        "0.1.0".to_owned(),
        test_util::in_memory_db(),
        Arc::new(StubSearcher),
        Arc::new(GraphIndex::Unavailable),
    )
}

#[test]
fn tool_registry_matches_frozen_contract() {
    let tools = tool_definitions();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, FROZEN_NAMES);

    let schema = |name: &str| -> Value {
        tools
            .iter()
            .find(|t| t.name.as_ref() == name)
            .unwrap()
            .schema_as_json_value()
    };

    // Property types transcribed from tools.go.
    for (tool, prop, r#type) in [
        ("search", "query", "string"),
        ("search", "top_k", "number"),
        ("search", "domain", "string"),
        ("catalog_documents", "page_size", "number"),
        ("catalog_documents", "cursor", "string"),
        ("catalog_documents", "domain", "string"),
        ("catalog_documents", "source_type", "string"),
        ("catalog_documents", "name", "string"),
        ("catalog_entities", "page_size", "number"),
        ("catalog_entities", "cursor", "string"),
        ("catalog_entities", "type", "string"),
        ("catalog_entities", "domain", "string"),
        ("catalog_entities", "name", "string"),
        ("search_entities_by_type", "entity_type", "string"),
        ("search_entities_by_type", "domain", "string"),
        ("search_entities_by_type", "page_size", "number"),
        ("search_entities_by_type", "cursor", "string"),
        ("search_facts", "predicate", "string"),
        ("search_facts", "entity_name", "string"),
        ("search_facts", "status", "string"),
        ("search_facts", "domain", "string"),
        ("search_facts", "page_size", "number"),
        ("search_facts", "cursor", "string"),
        ("get_document_context", "document_id", "string"),
        ("get_document_context", "include_chunks", "boolean"),
        ("get_document_context", "include_entities", "boolean"),
        ("get_document_context", "include_facts", "boolean"),
        ("get_chunk_by_id", "chunk_id", "string"),
        ("get_fact_by_id", "fact_id", "string"),
        ("get_entity_dossier", "entity_id", "string"),
        ("get_entity_dossier", "entity_name", "string"),
        ("get_entity_dossier", "domain", "string"),
        ("get_entity_dossier", "depth", "number"),
        ("get_entity_dossier", "include_facts", "boolean"),
        ("get_entity_dossier", "include_sources", "boolean"),
        ("get_entity_relations", "entity_id", "string"),
        ("get_entity_relations", "entity_name", "string"),
        ("get_entity_relations", "domain", "string"),
        ("get_entity_relations", "depth", "number"),
        ("get_entity_relations", "include_cross_domain", "boolean"),
        ("get_entity_links", "entity_id", "string"),
        ("get_entity_links", "entity_name", "string"),
        ("get_entity_links", "domain", "string"),
    ] {
        assert_eq!(
            schema(tool)["properties"][prop]["type"],
            json!(r#type),
            "{tool}.{prop}"
        );
    }

    // Defaults transcribed from tools.go.
    assert_eq!(
        schema("search")["properties"]["top_k"]["default"],
        json!(10)
    );
    for tool in [
        "catalog_documents",
        "catalog_entities",
        "search_entities_by_type",
        "search_facts",
    ] {
        assert_eq!(
            schema(tool)["properties"]["page_size"]["default"],
            json!(20),
            "{tool}"
        );
    }
    assert_eq!(
        schema("get_document_context")["properties"]["include_chunks"]["default"],
        json!(true)
    );
    assert_eq!(
        schema("get_document_context")["properties"]["include_entities"]["default"],
        json!(true)
    );
    assert_eq!(
        schema("get_document_context")["properties"]["include_facts"]["default"],
        json!(false)
    );
    assert_eq!(
        schema("get_entity_dossier")["properties"]["depth"]["default"],
        json!(2)
    );
    assert_eq!(
        schema("get_entity_relations")["properties"]["depth"]["default"],
        json!(2)
    );

    // Required fields transcribed from tools.go.
    assert_eq!(schema("search")["required"], json!(["query"]));
    assert_eq!(
        schema("search_entities_by_type")["required"],
        json!(["entity_type"])
    );
    assert_eq!(
        schema("get_document_context")["required"],
        json!(["document_id"])
    );
    assert_eq!(schema("get_chunk_by_id")["required"], json!(["chunk_id"]));
    assert_eq!(schema("get_fact_by_id")["required"], json!(["fact_id"]));
    for tool in [
        "catalog_overview",
        "catalog_documents",
        "catalog_entities",
        "search_facts",
        "get_entity_dossier",
        "get_entity_relations",
        "get_entity_links",
    ] {
        assert_eq!(
            schema(tool)["required"],
            json!(Vec::<String>::new()),
            "{tool}"
        );
    }

    // Wire shape invariants (mcp-go parity): inputSchema.type = object,
    // properties and required always present, annotations always {}.
    for t in &tools {
        let value = serde_json::to_value(t).unwrap();
        assert_eq!(value["inputSchema"]["type"], json!("object"));
        assert!(value["inputSchema"]["properties"].is_object());
        assert!(value["inputSchema"]["required"].is_array());
        assert_eq!(value["annotations"], json!({}));
        assert!(value["description"].is_string());
    }
}

#[test]
fn get_info_advertises_tools_and_identity() {
    let server = test_server();
    let info = server.get_info();
    assert_eq!(info.server_info.name, "synopsis-test");
    assert_eq!(info.server_info.version, "0.1.0");
    let capabilities = serde_json::to_value(&info.capabilities).unwrap();
    assert_eq!(capabilities["tools"]["listChanged"], json!(true));
}

#[test]
fn get_tool_resolves_registry_entries() {
    let server = test_server();
    assert!(server.get_tool("search").is_some());
    assert!(server.get_tool("no_such_tool").is_none());
}

/// Wiring test (replaces the task 5.9 stub test): dispatch routes the
/// graph tools to their real handlers — the no-graph degradation error
/// comes from the handler, not the stub — and the defensive catch-all
/// still reports unregistered names as not-yet-implemented.
#[test]
fn dispatch_routes_graph_tools_to_real_handlers() {
    let server = test_server();

    // `get_entity_relations` reaches the handler: the Unavailable graph
    // degrades to the handler's not-found tool error.
    let err = server.dispatch("get_entity_relations", None).unwrap_err();
    assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
    let result = err.into_tool_result();
    let rmcp::model::CallToolResponse::Complete(call) = result else {
        panic!("expected a complete tool result");
    };
    assert_eq!(call.is_error, Some(true));

    // `get_entity_links` reaches the handler: missing args are an
    // argument error, not the stub's not-yet-implemented.
    let err = server.dispatch("get_entity_links", None).unwrap_err();
    assert!(
        matches!(err, McpError::InvalidArguments { .. }),
        "got: {err:?}"
    );

    // The defensive catch-all is preserved for unregistered names.
    let err = server.dispatch("no_such_tool", None).unwrap_err();
    assert!(
        matches!(
            err,
            McpError::NotYetImplemented(ref name) if name == "no_such_tool"
        ),
        "got: {err:?}"
    );
}

/// A searcher that answers hybrid search with one canned result (the
/// swap target for the hot-swap test).
struct CannedSearcher;

impl Searcher for CannedSearcher {
    fn hybrid_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Ok(vec![SearchResult {
            chunk_id: 7,
            chunk_text: "canned".to_owned(),
            chunk_metadata: Map::new(),
            document_id: 1,
            sequence_num: 0,
            start_offset: None,
            end_offset: None,
            document_path: String::new(),
            score: 1.0,
            rank: 1,
            source_type: "lexical".to_owned(),
            metadata: Map::new(),
            entities: Vec::new(),
        }])
    }

    fn lexical_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Ok(Vec::new())
    }

    fn semantic_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Ok(Vec::new())
    }
}

/// The design D8 hot-swap seam: a swapped-in searcher and graph are what
/// `dispatch` serves from then on, and every session clone (the rmcp
/// factory clones the server per session) shares the swapped handles.
#[test]
fn set_searcher_and_set_graph_swap_the_active_handles() {
    use graph::Graph;

    let server = test_server();

    // Before the swap: the stub's lexical error surfaces as a tool
    // error and the scaffold graph is Unavailable.
    let err = server
        .dispatch("search", Some(&json!({ "query": "q" })))
        .unwrap_err();
    assert!(
        matches!(err, McpError::Search(SearchError::Lexical(_))),
        "got: {err:?}"
    );
    assert!(!server.graph().is_available());

    // Swap the searcher: the search now resolves with the canned hit.
    server.set_searcher(Arc::new(CannedSearcher));
    let payload = server
        .dispatch("search", Some(&json!({ "query": "q" })))
        .unwrap();
    assert_eq!(payload["total_count"], json!(1));
    assert_eq!(payload["results"][0]["chunk_id"], json!(7));

    // Swap the graph: the server reports the new (Ready) handle.
    server.set_graph(Arc::new(GraphIndex::Ready(Graph::from_rows(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ))));
    assert!(
        server.graph().is_available(),
        "the swapped graph must be served"
    );

    // A session clone sees the same swapped handles.
    let session = server.clone();
    assert!(session.graph().is_available());
    assert_eq!(
        session
            .dispatch("search", Some(&json!({ "query": "q" })))
            .unwrap()["total_count"],
        json!(1)
    );
}

/// Behavior test of the axum composition: `GET /health` is served by the
/// explicit route (not the MCP fallback) with the design D5 shape. The
/// full rmcp round-trip is task 5.10's integration test.
#[tokio::test]
async fn router_serves_health_on_explicit_route() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let response = test_server()
        .router()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], "0.1.0");
    assert_eq!(json["sync_state"], "idle");
    // The scaffold test server has an empty in-memory KB.
    assert_eq!(json["counters"]["documents"], 0);
    assert_eq!(json["counters"]["chunks"], 0);
    assert_eq!(json["counters"]["entities"], 0);
    assert_eq!(json["counters"]["facts"], 0);
}

// --- dual-transport coexistence (add-legacy-sse-transport task 1.3) ---

/// A temp fixture DB: 1 document + 2 chunks. The in-memory pool is the
/// temp storage; the seeded rows give the `/health` leg real counters to
/// assert (the same seed shape as `tests/server_integration.rs`).
fn fixture_db() -> db::Db {
    use db::{ChunkDao, ConnectionOrTx, DocumentDao};

    let db = test_util::in_memory_db();
    let seeded = db.with_conn(|conn| -> Result<(), db::DbError> {
        let exec = ConnectionOrTx::Connection(conn);
        let doc = DocumentDao::new(exec).create("markdown", "/docs/hr-policy.md", None, None)?;
        let chunks = ChunkDao::new(exec);
        chunks.create(doc, "quarterly hiring policy", 0, Some(0), Some(25))?;
        chunks.create(doc, "vacation rules", 1, None, None)?;
        Ok(())
    });
    seeded.expect("pool checkout").expect("seed rows");
    db
}

/// The coexistence test server: the fixture DB, the stub searcher, no
/// graph (identity `synopsis-router-test 9.9.9`).
fn fixture_server() -> Server {
    Server::new(
        "synopsis-router-test".to_owned(),
        "9.9.9".to_owned(),
        fixture_db(),
        Arc::new(StubSearcher),
        Arc::new(GraphIndex::Unavailable),
    )
}

/// Reads the next complete SSE frame (terminated by the blank line) from
/// a body data stream, buffering partial chunks (hyper may split or
/// coalesce frames across chunk boundaries).
async fn next_sse_frame<S>(data: &mut S, buffer: &mut String) -> String
where
    S: tokio_stream::StreamExt<Item = std::result::Result<axum::body::Bytes, axum::Error>> + Unpin,
{
    loop {
        if let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            *buffer = buffer[end + 2..].to_owned();
            return frame;
        }
        let Some(Ok(chunk)) = tokio::time::timeout(std::time::Duration::from_secs(2), data.next())
            .await
            .expect("a frame arrives in time")
        else {
            panic!("the stream ended before a complete frame");
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// Dual-transport coexistence on the REAL `Server::router()` (task 1.3
/// acceptance): one router serves `GET /health`, the legacy SSE pair
/// (`GET /sse` + `POST /message`), and the Streamable HTTP fallback
/// (rmcp client) at the same time. The oneshot legs run on clones of the
/// same router — axum `Router` clones share the router state (one
/// `Arc<Server>`, one session map) — and the rmcp leg serves the
/// original router value on a listener.
#[tokio::test]
async fn router_serves_health_sse_and_streamable_http_together() {
    use axum::body::Body;
    use axum::http::header;
    use axum::http::{Request, StatusCode};
    use rmcp::ServiceExt as _;
    use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
    use rmcp::transport::StreamableHttpClientTransport;
    use tower::ServiceExt as _;

    let router = fixture_server().router();

    // 1. GET /health → 200 with the seeded counters (existing behavior
    //    unchanged).
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let health: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], "9.9.9");
    assert_eq!(health["counters"]["documents"], json!(1));
    assert_eq!(health["counters"]["chunks"], json!(2));

    // 2. GET /sse → 200 text/event-stream; the first frame is the
    //    endpoint event (proxy-aware absolute URL, no proxy headers).
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sse")
                .header(header::HOST, "localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let mut data = response.into_body().into_data_stream();
    let mut buffer = String::new();
    let endpoint = next_sse_frame(&mut data, &mut buffer).await;
    assert!(
        endpoint.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
        "got: {endpoint}"
    );
    let session_id = endpoint
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .and_then(|url| url.split("sessionId=").nth(1))
        .expect("the endpoint URL carries a sessionId")
        .to_owned();

    // 3. POST /message?sessionId → 202 Accepted; the initialize response
    //    arrives as an `event: message` frame on the same stream.
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" }
    });
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/message?sessionId={session_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&initialize).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    drop(response);
    let frame = next_sse_frame(&mut data, &mut buffer).await;
    assert!(frame.starts_with("event: message\n"), "got: {frame}");
    let reply: Value = serde_json::from_str(
        frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("the message frame carries data"),
    )
    .unwrap();
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], json!(1));
    assert_eq!(
        reply["result"]["serverInfo"]["name"],
        "synopsis-router-test"
    );

    // 4. Streamable HTTP on the same router instance: the rmcp client
    //    (the tests/server_integration.rs pattern) completes initialize
    //    + tools/list.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let mut client = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new(
            "synopsis-router-coexistence-test",
            env!("CARGO_PKG_VERSION"),
        ),
    )
    .serve(transport)
    .await
    .unwrap();
    let peer = client.peer_info().expect("peer info after initialize");
    assert_eq!(
        peer.server_info.as_ref().expect("server info").name,
        "synopsis-router-test"
    );
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 12);
    client.close().await.unwrap();

    // Drop the SSE stream: the client disconnect removes the session.
    drop(data);
}

// --- idle reaper on the real router (add-legacy-sse-transport task 1.5) ---

/// The idle reaper is live on the REAL `Server::router()` (task 1.5
/// acceptance): the threshold is not injectable through `Server` (no
/// config surface is invented — the oracle has none), so the production
/// defaults (300 s threshold / 30 s tick, design D9) are exercised on a
/// paused clock: a `/sse` session with no activity is reaped at the first
/// reaper pass past the threshold (t=330 s), and the client observes the
/// stream end. Short-threshold behavior is covered by the map-level unit
/// tests in `transport/sse.rs`.
#[tokio::test(start_paused = true)]
async fn router_spawns_idle_reaper() {
    use axum::body::Body;
    use axum::http::header;
    use axum::http::{Request, StatusCode};
    use tokio_stream::StreamExt as _;
    use tower::ServiceExt as _;

    let router = test_server().router();

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/sse")
                .header(header::HOST, "localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut data = response.into_body().into_data_stream();
    let first = data
        .next()
        .await
        .expect("the endpoint frame arrives")
        .expect("no frame error");
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(
        first.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
        "got: {first}"
    );

    // The production defaults (design D9): 300 s threshold, 30 s tick —
    // the first reaping pass lands at t=330 s. The paused clock
    // auto-advances through each reaper tick as the test sleeps (no real
    // waiting); sleeping to t=331 s guarantees that pass is complete.
    tokio::time::sleep(std::time::Duration::from_secs(331)).await;
    // The reaper dropped the session's sender: the stream ends (EOF).
    assert!(
        data.next().await.is_none(),
        "the idle session on the real router must be reaped"
    );
}
