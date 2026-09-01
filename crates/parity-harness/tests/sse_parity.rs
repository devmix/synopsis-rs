//! Cross-transport parity test (add-legacy-sse-transport task 1.4).
//!
//! Proves the legacy HTTP+SSE transport serves the SAME tool responses as the
//! Streamable HTTP transport against the same fixture DB:
//!
//! 1. `tools/list` via SSE == `tools/list` via Streamable HTTP (deep-equal JSON);
//! 2. `initialize` via SSE: serverInfo name/version == the Streamable HTTP
//!    initialize serverInfo;
//! 3. three read-only `tools/call`s (`catalog_overview`, `catalog_documents`,
//!    `get_chunk_by_id`) via BOTH transports → deep-equal JSON.
//!
//! Both transports are served by ONE `mcp::Server` instance (design D1/D5: one
//! dispatch seam, two transports), so any drift would be a real bug. The
//! fixture is a seeded in-memory knowledge DB (one document, two chunks) — no
//! ONNX model, no ANN, fully deterministic, so no graceful skip is needed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use db::{ChunkDao, ConnectionOrTx, DbError, DocumentDao};
use graph::GraphIndex;
use mcp::Server;
use parity_harness::diff::json_diff;
use parity_harness::mcp_client::McpClient;
use parity_harness::sse_client::SseClient;
use search::{SearchError, SearchResult, Searcher};
use serde_json::{Value, json};

/// A `Searcher` stub: the three tools exercised here never reach the searcher
/// (they read the catalog/chunks tables directly), so an empty result is
/// sufficient for the injectable handle.
struct StubSearcher;

impl Searcher for StubSearcher {
    fn hybrid_search(
        &self,
        _query: &str,
        _top_k: i32,
        _domain: Option<&str>,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Ok(Vec::new())
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

/// Seed an in-memory knowledge DB with one document and two chunks; returns the
/// handle and the first chunk's id (for the per-id tool).
fn seeded_db() -> (db::Db, i64) {
    let db = db::test_util::in_memory_db();
    let chunk_id = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let doc =
                DocumentDao::new(exec).create("markdown", "/docs/hr-policy.md", None, None)?;
            let chunks = ChunkDao::new(exec);
            let first = chunks.create(doc, "quarterly hiring policy", 0, Some(0), Some(25))?;
            chunks.create(doc, "vacation rules and carry-over", 1, None, None)?;
            Ok::<i64, DbError>(first)
        })
        .expect("db connection")
        .expect("seed document and chunks");
    (db, chunk_id)
}

/// The tool payload (the `content[0].text` JSON) of an SSE `tools/call` result.
fn sse_payload(result: &Value) -> Value {
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0] is text");
    serde_json::from_str(text).expect("the text is the payload JSON")
}

/// The tool payload (the `content[0].text` JSON) of an rmcp `tools/call` result.
fn http_payload(result: &rmcp::model::CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .find_map(rmcp::model::ContentBlock::as_text)
        .expect("text content");
    serde_json::from_str(&text.text).expect("the text is the payload JSON")
}

/// Boot the product server over the seeded DB on a random loopback port and
/// drive the same three operations through BOTH transports, asserting the
/// responses are deep-equal.
#[tokio::test]
async fn sse_and_streamable_http_serve_identical_tool_responses() {
    let (db, chunk_id) = seeded_db();
    let server = Server::new(
        "synopsis-sse-parity".to_owned(),
        "0.1.0".to_owned(),
        db,
        Arc::new(StubSearcher),
        Arc::new(GraphIndex::Unavailable),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = tokio::spawn(async move {
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // One server, two transports: the SSE client on /sse + /message, the rmcp
    // client on /mcp.
    let mut sse = SseClient::connect(&format!("http://{addr}"))
        .await
        .expect("SSE connect");
    let mut http = McpClient::connect(&format!("http://{addr}/mcp"))
        .await
        .expect("Streamable HTTP connect");

    // 1. tools/list: deep-equal JSON (same 12 names + schemas).
    let sse_list = sse.list_tools().await.expect("SSE tools/list");
    let http_list = http.list_tools().await.expect("HTTP tools/list");
    let http_list_json = serde_json::to_value(&http_list).expect("serialize HTTP tools");
    let diff = json_diff(&sse_list["tools"], &http_list_json);
    assert!(
        diff.is_empty(),
        "tools/list differs between transports:\n{}",
        diff.join("\n")
    );
    assert_eq!(
        sse_list["tools"]
            .as_array()
            .expect("tools is an array")
            .len(),
        12,
        "the frozen tool set"
    );

    // 2. initialize: serverInfo name/version equal across transports.
    let sse_init = sse.initialize().await.expect("SSE initialize");
    let http_info = http
        .peer_info()
        .expect("peer info after connect")
        .server_info
        .expect("server info after initialize");
    assert_eq!(
        sse_init["serverInfo"]["name"],
        json!(http_info.name),
        "serverInfo.name must match"
    );
    assert_eq!(
        sse_init["serverInfo"]["version"],
        json!(http_info.version),
        "serverInfo.version must match"
    );

    // 3. Three read-only tools/call: deep-equal payload across transports.
    //    All three are deterministic on the seeded DB (no ANN scores), so a
    //    plain deep-equal is exact.
    let calls: [(&str, Value); 3] = [
        ("catalog_overview", json!({})),
        ("catalog_documents", json!({})),
        (
            "get_chunk_by_id",
            json!({ "chunk_id": chunk_id.to_string() }),
        ),
    ];
    for (name, args) in calls {
        let sse_result = sse
            .call_tool(name, args.clone())
            .await
            .expect("SSE tools/call");
        let http_args = args.as_object().expect("arguments are an object").clone();
        let http_result = http
            .call_tool(name, http_args)
            .await
            .expect("HTTP tools/call");

        // Both transports report a successful (non-error) result.
        assert_eq!(sse_result["isError"], json!(false), "{name} SSE isError");

        let diff = json_diff(&sse_payload(&sse_result), &http_payload(&http_result));
        assert!(
            diff.is_empty(),
            "`{name}` differs between transports:\n{}",
            diff.join("\n")
        );
    }

    // Graceful shutdown: close the HTTP session, drop the SSE stream, stop the
    // server.
    http.close().await.expect("HTTP close");
    drop(sse);
    shutdown_tx.send(()).unwrap();
    serve.await.expect("serve task join");
}
