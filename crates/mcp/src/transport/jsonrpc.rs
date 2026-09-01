//! Minimal JSON-RPC 2.0 method table for the legacy SSE transport (design D4).
//!
//! Wire reference: mcp-go v0.57.0 (pinned in `../synopsis/go.mod`),
//! `server/request_handler.go` `MCPServer.HandleMessage` + `server/server.go`
//! handlers. The oracle's server is tools-only: `initialize`, `ping`,
//! `tools/list`, `tools/call` are the only request methods; everything else
//! (resources/prompts/completions/logging) is `-32601`.
//!
//! Results are framed exactly as the Streamable HTTP path frames them for the
//! same call (rmcp model types, design D1: one dispatch seam, two
//! transports), so the two transports cannot drift. Every SSE peer is a
//! legacy peer (protocol ≤ 2025-11-25), so the SEP-2322 `resultType`
//! discriminator that rmcp strips for legacy Streamable HTTP peers is
//! stripped here too.

use rmcp::ServerHandler;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::McpError;
use crate::server::Server;

// --- JSON-RPC 2.0 error codes (mcp-go v0.57.0 mcp/types.go) ---

/// JSON-RPC 2.0 parse error (mcp-go v0.57.0 `mcp.PARSE_ERROR`).
pub const PARSE_ERROR: i32 = -32700;
/// Invalid JSON-RPC request (mcp-go v0.57.0 `mcp.INVALID_REQUEST`).
pub const INVALID_REQUEST: i32 = -32600;
/// Unknown method (mcp-go v0.57.0 `mcp.METHOD_NOT_FOUND`).
pub const METHOD_NOT_FOUND: i32 = -32601;
/// Invalid parameters (mcp-go v0.57.0 `mcp.INVALID_PARAMS`).
pub const INVALID_PARAMS: i32 = -32602;

/// mcp-go v0.57.0 mcp/types.go `ValidProtocolVersions` (pinned).
const VALID_PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// mcp-go v0.57.0 mcp/types.go `LATEST_PROTOCOL_VERSION` (pinned).
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// mcp-go v0.57.0 server.go `protocolVersion`: an empty client version falls
/// back to "2025-03-26" (the spec's backwards-compat default), NOT the
/// server's latest.
const FALLBACK_PROTOCOL_VERSION: &str = "2025-03-26";

/// A JSON-RPC 2.0 message as received on `POST /message` (mcp-go v0.57.0
/// request_handler.go `baseMessage`). All members are optional on purpose:
/// any JSON object decodes, and the semantic checks (version, id, method)
/// happen in [`dispatch`] with the pinned error codes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct JsonRpcRequest {
    /// The `"jsonrpc"` member; must be `"2.0"` (else -32600, pinned).
    pub jsonrpc: Option<String>,
    /// The request id; a missing/null id marks a notification (no response —
    /// JSON-RPC 2.0 spec, mcp-go v0.57.0).
    pub id: Option<Value>,
    /// The method to dispatch; absent on client-sent responses.
    pub method: Option<String>,
    /// Method parameters (opaque to the envelope; each method validates its
    /// own).
    pub params: Option<Value>,
    /// Present when the client answers a server-sent request (mcp-go
    /// v0.57.0: such a message gets no reply).
    pub result: Option<Value>,
}

/// A JSON-RPC 2.0 error object (mcp-go v0.57.0 mcp/types.go
/// `JSONRPCErrorDetails`: code + message; `data` is omitempty and the oracle
/// never sets it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// The JSON-RPC error code (one of the constants above).
    pub code: i32,
    /// A short, human-readable description (mcp-go: "SHOULD be limited to a
    /// concise single sentence").
    pub message: String,
}

/// A JSON-RPC 2.0 response envelope (mcp-go v0.57.0 mcp/types.go
/// `JSONRPCResponse`/`JSONRPCError`): `jsonrpc` + `id` + exactly one of
/// `result` / `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"` (mcp-go `mcp.JSONRPC_VERSION`).
    pub jsonrpc: String,
    /// Echoes the request id (null on errors with no id, e.g. parse errors).
    pub id: Value,
    /// The result of a successful request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error of a failed request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// A success response: `{"jsonrpc":"2.0","id":…,"result":…}`.
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response: `{"jsonrpc":"2.0","id":…,"error":{…}}`.
    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Serialize a response to its wire `Value` (the no-panic rule, design D7:
/// a response is plain strings and values, so serialization cannot fail —
/// the fallback is unreachable but keeps the code panic-free).
fn response_value(response: JsonRpcResponse) -> Value {
    serde_json::to_value(&response).unwrap_or(Value::Null)
}

/// Dispatch one raw JSON-RPC 2.0 message (mcp-go v0.57.0
/// `MCPServer.HandleMessage`, tools-only method table, design D4).
///
/// Returns the response object to push onto the session's SSE channel, or
/// `None` when no response is due (notifications, client-sent responses —
/// mcp-go v0.57.0 returns nil in both cases).
pub async fn dispatch(server: &Server, raw: &Value) -> Option<Value> {
    let request = match serde_json::from_value::<JsonRpcRequest>(raw.clone()) {
        Ok(request) => request,
        // mcp-go v0.57.0 request_handler.go HandleMessage: a message that
        // does not unmarshal into the base object (a batch array, a scalar,
        // a wrong-typed member) → -32700 "Failed to parse message", id null.
        // mcp-go has no batch support (pinned: the oracle's clients never
        // send batches).
        Err(_) => {
            return Some(response_value(JsonRpcResponse::error(
                Value::Null,
                PARSE_ERROR,
                "Failed to parse message",
            )));
        }
    };

    // mcp-go v0.57.0 request_handler.go: `jsonrpc` must be "2.0" → else
    // -32600 "Invalid JSON-RPC version" (the id is echoed).
    if request.jsonrpc.as_deref() != Some("2.0") {
        return Some(response_value(JsonRpcResponse::error(
            request.id.clone().unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Invalid JSON-RPC version",
        )));
    }

    let id = request.id.clone().unwrap_or(Value::Null);

    // mcp-go v0.57.0 request_handler.go: a missing/null id is a notification
    // → `handleNotification` returns nil (JSON-RPC 2.0 spec: a compliant
    // server MUST NOT reply to a valid notification).
    if id.is_null() {
        return None;
    }

    // mcp-go v0.57.0 request_handler.go: a non-null `result` member means the
    // client answers a server-sent request (e.g. a keep-alive ping) → nil.
    if request
        .result
        .as_ref()
        .is_some_and(|value| !value.is_null())
    {
        return None;
    }

    // mcp-go v0.57.0 request_handler.go: the tools-only method table.
    Some(match request.method.as_deref().unwrap_or("") {
        "initialize" => handle_initialize(server, id, request.params.as_ref()),
        // mcp-go v0.57.0 server.go handlePing: EmptyResult → {}.
        "ping" => response_value(JsonRpcResponse::result(id, Value::Object(Map::new()))),
        "tools/list" => response_value(JsonRpcResponse::result(id, list_tools_result(server))),
        "tools/call" => handle_tool_call(server, id, request.params.as_ref()).await,
        // mcp-go v0.57.0 request_handler.go default arm: everything else
        // (resources/prompts/completions/logging — the oracle's tools-only
        // server registers none of them) → -32601.
        other => response_value(JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            format!("Method {other} not found"),
        )),
    })
}

/// `initialize` (mcp-go v0.57.0 server.go handleInitialize).
fn handle_initialize(server: &Server, id: Value, params: Option<&Value>) -> Value {
    // mcp-go v0.57.0 request_handler.go MethodInitialize: the message must
    // unmarshal into InitializeRequest — params (when present) must be an
    // object with a string protocolVersion; structurally invalid params →
    // -32600 (design D4 says -32602; the pinned oracle uses -32600 for
    // unmarshal failures). The oracle's message is Go-specific
    // (`UnparsableMessageError`: "unparsable initialize request: <go json
    // error>"); the code is pinned exactly, the message is the stable
    // equivalent. Absent params unmarshal to the zero value (empty
    // protocolVersion → the backwards-compat default).
    let client_version = match params {
        None => String::new(),
        Some(Value::Object(obj)) => match obj.get("protocolVersion") {
            None => String::new(),
            Some(Value::String(version)) => version.clone(),
            Some(_) => {
                return response_value(JsonRpcResponse::error(
                    id,
                    INVALID_REQUEST,
                    "Failed to parse message",
                ));
            }
        },
        Some(_) => {
            return response_value(JsonRpcResponse::error(
                id,
                INVALID_REQUEST,
                "Failed to parse message",
            ));
        }
    };
    response_value(JsonRpcResponse::result(
        id,
        initialize_result(server, &client_version),
    ))
}

/// The `initialize` result object (pinned from mcp-go v0.57.0 server.go
/// handleInitialize): `{"protocolVersion": <rule>, "capabilities":
/// <tools-only>, "serverInfo": {"name","version"}}` (instructions is
/// omitempty and the oracle sets none; `_meta` is omitempty and never set).
fn initialize_result(server: &Server, client_version: &str) -> Value {
    // mcp-go v0.57.0 server.go protocolVersion (pinned rule):
    //  - empty client version → "2025-03-26" (the spec's backwards-compat
    //    default, NOT the latest);
    //  - a known version (mcp/types.go ValidProtocolVersions) → echoed;
    //  - anything else → the server's latest ("2025-11-25").
    let protocol_version = if client_version.is_empty() {
        FALLBACK_PROTOCOL_VERSION
    } else if VALID_PROTOCOL_VERSIONS.contains(&client_version) {
        client_version
    } else {
        LATEST_PROTOCOL_VERSION
    };

    // serverInfo + capabilities: the same values the Streamable HTTP path
    // reports (ServerHandler::get_info — name/version from Server, tools-only
    // capabilities with tools.listChanged = true), serialized through rmcp's
    // types so the two transports cannot drift. (The oracle additionally
    // advertises an empty `resources: {}` object because the Go code passes
    // WithResourceCapabilities(false, false) explicitly; the Rust server is
    // tools-only — design D4 — and registers no resources at all.)
    let info = ServerHandler::get_info(server);
    let capabilities =
        serde_json::to_value(&info.capabilities).unwrap_or(Value::Object(Map::new()));
    json!({
        "protocolVersion": protocol_version,
        "capabilities": capabilities,
        "serverInfo": {
            "name": info.server_info.name,
            "version": info.server_info.version,
        },
    })
}

/// The `tools/list` result (mcp-go v0.57.0 server.go handleListTools):
/// `{"tools": [ … ]}`. The oracle sets no pagination limit, so all tools are
/// returned and nextCursor is absent (omitempty). The same 12 Tool
/// definitions the Streamable HTTP path serves (`Server::tools`), serialized
/// through rmcp's `Tool` so both transports produce identical JSON.
fn list_tools_result(server: &Server) -> Value {
    let tools = serde_json::to_value(server.tools()).unwrap_or(Value::Array(Vec::new()));
    json!({ "tools": tools })
}

/// `tools/call` (mcp-go v0.57.0 server.go handleToolCall).
async fn handle_tool_call(server: &Server, id: Value, params: Option<&Value>) -> Value {
    // mcp-go v0.57.0 request_handler.go MethodToolsCall: the message must
    // unmarshal into CallToolRequest — params (when present) must be an
    // object with a string name; structurally invalid params → -32600 (the
    // oracle's `UnparsableMessageError` message is Go-specific, as above —
    // the code is pinned, the message is the stable equivalent). Absent
    // params unmarshal to the zero value (empty name).
    let (name, arguments) = match params {
        None => (String::new(), None),
        Some(Value::Object(obj)) => match obj.get("name") {
            None => (String::new(), obj.get("arguments").cloned()),
            Some(Value::String(name)) => (name.clone(), obj.get("arguments").cloned()),
            Some(_) => {
                return response_value(JsonRpcResponse::error(
                    id,
                    INVALID_REQUEST,
                    "Failed to parse message",
                ));
            }
        },
        Some(_) => {
            return response_value(JsonRpcResponse::error(
                id,
                INVALID_REQUEST,
                "Failed to parse message",
            ));
        }
    };

    // mcp-go v0.57.0 server.go handleToolCall: an unregistered (or missing →
    // "") tool name is a JSON-RPC INVALID_PARAMS error — NOT a tool result:
    // `tool '<name>' not found: tool not found` (pinned message shape,
    // mcp.ErrToolNotFound).
    if !server.tools().iter().any(|tool| tool.name.as_ref() == name) {
        return response_value(JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            format!("tool '{name}' not found: tool not found"),
        ));
    }

    // The design D1 dispatch seam: the same blocking worker hop the
    // Streamable HTTP path uses (SQLite/ONNX/vector index may block).
    let server = server.clone();
    let dispatched =
        tokio::task::spawn_blocking(move || server.dispatch(&name, arguments.as_ref()))
            .await
            .map_err(|join_err| McpError::Internal(format!("tool dispatch failed: {join_err}")));

    // MCP convention (pinned: the oracle's Go handlers return
    // CallToolResult{IsError: true} and mcp-go v0.57.0 passes handler results
    // through as results): a tool-level failure is an `isError: true`
    // result, not a JSON-RPC error. The result is framed exactly as the
    // Streamable HTTP path frames it for the same call (rmcp
    // CallToolResult — content[0].text = the serialized dispatch payload).
    let mut result = match dispatched {
        Ok(Ok(payload)) => {
            // A serde_json::Value always serializes; the fallback keeps the
            // no-panic rule (design D7) without an unwrap.
            let text = serde_json::to_string(&payload).unwrap_or_else(|err| err.to_string());
            CallToolResult::success(vec![ContentBlock::text(text)])
        }
        Ok(Err(err)) | Err(err) => {
            let message = err.to_string();
            match err.into_tool_result() {
                rmcp::model::CallToolResponse::Complete(result) => result,
                // into_tool_result only produces Complete results (error.rs);
                // this arm keeps the non-exhaustive enum total.
                _ => CallToolResult::error(vec![ContentBlock::text(message)]),
            }
        }
    };
    // Every SSE peer is a legacy peer (protocol ≤ 2025-11-25): strip the
    // SEP-2322 resultType discriminator exactly as rmcp does for legacy
    // Streamable HTTP peers (rmcp handler/server.rs), so the result JSON is
    // identical to what the Streamable HTTP path sends the same client.
    result.result_type = None;
    response_value(JsonRpcResponse::result(
        id,
        serde_json::to_value(&result).unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::transport::test_util::test_server;

    /// A valid request gets a response (ping → {}).
    #[tokio::test]
    async fn valid_request_gets_a_response() {
        let server = test_server();
        let response = dispatch(&server, &json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await;
        let response = response.expect("a request gets a response");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"], json!({}));
        assert!(response.get("error").is_none());
    }

    /// A method-bearing message without an id is a notification (JSON-RPC
    /// 2.0 spec: a compliant server MUST NOT reply; mcp-go v0.57.0
    /// request_handler.go returns nil) — no channel message.
    #[tokio::test]
    async fn request_without_id_is_a_notification_and_gets_no_response() {
        let server = test_server();
        for raw in [
            json!({"jsonrpc":"2.0","method":"ping"}),
            json!({"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}),
        ] {
            assert!(
                dispatch(&server, &raw).await.is_none(),
                "no response for: {raw}"
            );
        }
    }

    /// A body that does not parse as a JSON-RPC object → -32700 (mcp-go
    /// v0.57.0 request_handler.go: batch arrays and scalars fail the base
    /// message unmarshal → "Failed to parse message", id null).
    #[tokio::test]
    async fn parse_error_is_minus_32700_with_null_id() {
        let server = test_server();
        for raw in [
            json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]), // batch
            json!(5),                                          // scalar
        ] {
            let response = dispatch(&server, &raw)
                .await
                .expect("parse error gets a response");
            assert_eq!(response["error"]["code"], json!(-32700), "got: {response}");
            assert_eq!(response["error"]["message"], "Failed to parse message");
            assert!(response["id"].is_null());
            assert_eq!(response["jsonrpc"], "2.0");
        }
    }

    /// A wrong/missing `jsonrpc` version → -32600 (mcp-go v0.57.0
    /// request_handler.go, the id is echoed).
    #[tokio::test]
    async fn invalid_jsonrpc_version_is_minus_32600() {
        let server = test_server();
        let response = dispatch(&server, &json!({"jsonrpc":"1.0","id":7,"method":"ping"})).await;
        let response = response.expect("a request gets a response");
        assert_eq!(response["error"]["code"], json!(-32600));
        assert_eq!(response["error"]["message"], "Invalid JSON-RPC version");
        assert_eq!(response["id"], json!(7));
    }

    /// A client-sent response (non-null `result` member) gets no reply
    /// (mcp-go v0.57.0 request_handler.go).
    #[tokio::test]
    async fn client_sent_response_gets_no_reply() {
        let server = test_server();
        let raw = json!({"jsonrpc":"2.0","id":9,"result":{}});
        assert!(dispatch(&server, &raw).await.is_none());
    }

    /// The `initialize` result shape (pinned from mcp-go v0.57.0 server.go
    /// handleInitialize): protocolVersion rule, tools-only capabilities,
    /// serverInfo = the same name/version the Streamable HTTP path reports.
    #[tokio::test]
    async fn initialize_result_shape_is_pinned() {
        let server = test_server();
        let raw = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"t","version":"1"}}});
        let response = dispatch(&server, &raw)
            .await
            .expect("initialize gets a response");
        let result = &response["result"];
        // A known version is echoed.
        assert_eq!(result["protocolVersion"], "2025-06-18");
        // The same capabilities + serverInfo the Streamable HTTP path reports.
        let info = ServerHandler::get_info(&server);
        assert_eq!(
            result["capabilities"],
            serde_json::to_value(&info.capabilities).unwrap(),
            "capabilities must match the Streamable HTTP path"
        );
        assert_eq!(
            result["capabilities"],
            json!({"tools":{"listChanged":true}})
        );
        assert_eq!(
            result["serverInfo"],
            json!({"name":"synopsis-sse-test","version":"0.2.0"})
        );
        assert_eq!(result["serverInfo"]["name"], info.server_info.name);
        assert_eq!(result["serverInfo"]["version"], info.server_info.version);
        // instructions is omitempty and the server sets none.
        assert!(result.get("instructions").is_none());
    }

    /// The protocolVersion negotiation rule (pinned from mcp-go v0.57.0
    /// server.go protocolVersion): empty → 2025-03-26; known → echoed;
    /// unknown → the server's latest (2025-11-25).
    #[tokio::test]
    async fn initialize_protocol_version_rule_is_pinned() {
        let server = test_server();
        // Absent params → empty client version → the backwards-compat
        // default (NOT the latest).
        let response = dispatch(
            &server,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        )
        .await;
        let response = response.expect("initialize gets a response");
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        // Known versions are echoed.
        for known in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
            let raw = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":known}});
            let response = dispatch(&server, &raw)
                .await
                .expect("initialize gets a response");
            assert_eq!(
                response["result"]["protocolVersion"], known,
                "{known} must be echoed"
            );
        }
        // An unknown version → the server's latest.
        let raw = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}});
        let response = dispatch(&server, &raw)
            .await
            .expect("initialize gets a response");
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    }

    /// `tools/list` returns exactly the 12 frozen tools, the same names as
    /// `Server::tools()` (design D4), with no pagination cursor.
    #[tokio::test]
    async fn list_tools_returns_the_twelve_frozen_tools() {
        let server = test_server();
        let response = dispatch(
            &server,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let response = response.expect("tools/list gets a response");
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools is an array");
        assert_eq!(tools.len(), 12);
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = server
            .tools()
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(names, expected);
        // The oracle sets no pagination limit; nextCursor is omitempty.
        assert!(response["result"].get("nextCursor").is_none());
    }

    /// `tools/call` success: the result's content[0].text is the same JSON
    /// the Streamable HTTP path produces for the same call (the dispatch
    /// payload), framed as an rmcp CallToolResult (`isError: false`).
    #[tokio::test]
    async fn tool_call_success_is_the_dispatch_payload_as_result() {
        let server = test_server();
        let raw = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"catalog_overview"}});
        let response = dispatch(&server, &raw)
            .await
            .expect("tools/call gets a response");
        let result = &response["result"];
        assert_eq!(result["isError"], json!(false));
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content[0] is text");
        let payload: Value = serde_json::from_str(text).expect("the text is the payload JSON");
        // The same JSON the Streamable HTTP path produces for the same call.
        assert_eq!(
            payload,
            server
                .dispatch("catalog_overview", None)
                .expect("dispatch works")
        );
    }

    /// `tools/call` tool-level failure: an `isError: true` result with the
    /// error text (MCP convention, pinned from the oracle's Go handlers).
    #[tokio::test]
    async fn tool_call_tool_error_is_an_error_result() {
        let server = test_server();
        let raw = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_entity_links"}});
        let response = dispatch(&server, &raw)
            .await
            .expect("tools/call gets a response");
        let result = &response["result"];
        assert_eq!(result["isError"], json!(true));
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content[0] is text");
        assert!(text.contains("get_entity_links"), "got: {text}");
        assert!(text.contains("invalid arguments"), "got: {text}");
    }

    /// `tools/call` unknown tool: a JSON-RPC INVALID_PARAMS error (pinned
    /// from mcp-go v0.57.0 server.go handleToolCall: `tool '<name>' not
    /// found: tool not found`), NOT a tool result.
    #[tokio::test]
    async fn tool_call_unknown_tool_is_an_invalid_params_error() {
        let server = test_server();
        let raw =
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"no_such_tool"}});
        let response = dispatch(&server, &raw)
            .await
            .expect("unknown tool gets an error response");
        assert_eq!(response["error"]["code"], json!(-32602));
        assert_eq!(
            response["error"]["message"],
            "tool 'no_such_tool' not found: tool not found"
        );
        assert_eq!(response["id"], json!(5));
        assert!(response.get("result").is_none());
    }

    /// Any other method → -32601 (mcp-go v0.57.0 request_handler.go default
    /// arm: `Method <m> not found`) — resources/prompts/completions included
    /// (the oracle's tools-only server registers none of them).
    #[tokio::test]
    async fn unknown_method_is_minus_32601() {
        let server = test_server();
        for method in [
            "resources/list",
            "prompts/list",
            "completion/complete",
            "logging/setLevel",
            "foo/bar",
        ] {
            let raw = json!({"jsonrpc":"2.0","id":6,"method":method});
            let response = dispatch(&server, &raw)
                .await
                .expect("unknown method gets an error response");
            assert_eq!(response["error"]["code"], json!(-32601), "{method}");
            assert_eq!(
                response["error"]["message"],
                format!("Method {method} not found")
            );
        }
    }

    /// Structurally invalid params (present but not an object, or a
    /// wrong-typed member) → -32600 (pinned from mcp-go v0.57.0
    /// request_handler.go: the typed request unmarshal fails →
    /// INVALID_REQUEST).
    #[tokio::test]
    async fn structurally_invalid_params_is_minus_32600() {
        let server = test_server();
        for raw in [
            json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":"nope"}),
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":[1,2]}),
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":42}}),
        ] {
            let response = dispatch(&server, &raw)
                .await
                .expect("invalid params get an error response");
            assert_eq!(response["error"]["code"], json!(-32600), "got: {response}");
        }
    }
}
