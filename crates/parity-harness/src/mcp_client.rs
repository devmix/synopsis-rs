//! MCP client wrapper over the official rmcp SDK (design D8).
//!
//! The harness talks to any Streamable HTTP endpoint with `initialize`,
//! `tools/list` and `tools/call`, instruments every operation with a wall-clock
//! duration, and exposes p50/p95 percentiles per operation. No hand-rolled
//! SSE/HTTP parsing: the transport is rmcp's reqwest-backed streamable-HTTP
//! client. Failures are typed [`HarnessError`] values; nothing panics.
//!
//! Operation names recorded in [`TimingStats`]: `initialize`, `tools/list`, and
//! one key per call, `tools/call:<tool-name>` (failed calls are latency too).

use std::time::{Duration, Instant};

use rmcp::{
    ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, ErrorCode,
        Implementation, ServerPeerInfo, Tool,
    },
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
};

use crate::HarnessError;

/// Connected MCP client session with per-operation timing statistics.
#[derive(Debug)]
pub struct McpClient {
    inner: RunningService<RoleClient, ClientInfo>,
    stats: TimingStats,
}

impl McpClient {
    /// Connect to a Streamable HTTP endpoint and complete the `initialize`
    /// handshake. The duration of the whole handshake is recorded as
    /// `initialize`.
    pub async fn connect(uri: &str) -> Result<Self, HarnessError> {
        let transport = StreamableHttpClientTransport::from_uri(uri);
        let client_info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("synopsis-parity-harness", env!("CARGO_PKG_VERSION")),
        );
        let started = Instant::now();
        let inner = client_info
            .serve(transport)
            .await
            .map_err(|err| HarnessError::McpService {
                detail: err.to_string(),
            })?;
        let mut stats = TimingStats::default();
        stats.record("initialize", started.elapsed());
        Ok(Self { inner, stats })
    }

    /// Identity negotiated during `connect` (server name/version, protocol
    /// version), once available. Returns a clone: the SDK stores the peer info
    /// behind an `Arc`, so callers get an owned value rather than a borrow of
    /// internal state.
    pub fn peer_info(&self) -> Option<ServerPeerInfo> {
        self.inner.peer_info().map(|arc| arc.as_ref().clone())
    }

    /// Fetch the full tool list (`tools/list`, pagination followed).
    /// Recorded as one `tools/list` sample per call.
    pub async fn list_tools(&mut self) -> Result<Vec<Tool>, HarnessError> {
        let started = Instant::now();
        let tools = match self.inner.list_all_tools().await {
            Ok(tools) => tools,
            Err(err) => return Err(service_error_to_harness(err, None)),
        };
        self.stats.record("tools/list", started.elapsed());
        Ok(tools)
    }

    /// Call one tool with JSON arguments. Recorded as a `tools/call:<name>`
    /// sample regardless of outcome (failed calls are latency too).
    ///
    /// Failures are typed: an unknown tool becomes [`HarnessError::UnknownTool`]
    /// and a tool-level error result (`is_error = true`) becomes
    /// [`HarnessError::ToolFailed`].
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<rmcp::model::CallToolResult, HarnessError> {
        let started = Instant::now();
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        // Every outcome — success, tool-level error, protocol error — is a
        // latency sample: failed calls are latency too.
        let outcome = match self.inner.call_tool(params).await {
            Ok(result) if result.is_error == Some(true) => {
                let detail = result
                    .content
                    .iter()
                    .filter_map(ContentBlock::as_text)
                    .map(|text| text.text.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(HarnessError::ToolFailed {
                    tool: name.to_owned(),
                    detail,
                })
            }
            Ok(result) => Ok(result),
            Err(err) => Err(service_error_to_harness(err, Some(name))),
        };
        self.stats
            .record(format!("tools/call:{name}"), started.elapsed());
        outcome
    }

    /// Timing statistics collected so far.
    pub fn stats(&self) -> &TimingStats {
        &self.stats
    }

    /// Gracefully close the session and wait for cleanup to complete. Recorded
    /// statistics remain available on `self` afterwards.
    pub async fn close(&mut self) -> Result<(), HarnessError> {
        self.inner
            .close()
            .await
            .map_err(|err| HarnessError::McpService {
                detail: err.to_string(),
            })?;
        Ok(())
    }
}

/// Map an rmcp [`ServiceError`] to a typed harness error. When `tool` is set,
/// a JSON-RPC `METHOD_NOT_FOUND` response is classified as unknown tool.
fn service_error_to_harness(err: ServiceError, tool: Option<&str>) -> HarnessError {
    match (err, tool) {
        (ServiceError::McpError(data), Some(tool)) if data.code == ErrorCode::METHOD_NOT_FOUND => {
            HarnessError::UnknownTool {
                tool: tool.to_owned(),
                message: data.message.to_string(),
            }
        }
        (other, _) => HarnessError::McpService {
            detail: other.to_string(),
        },
    }
}

/// Per-operation latency samples with nearest-rank percentile queries.
#[derive(Debug, Default)]
pub struct TimingStats {
    samples: std::collections::BTreeMap<String, Vec<Duration>>,
}

impl TimingStats {
    /// Record one `duration` sample under an operation name (e.g. `tools/list`).
    pub fn record(&mut self, operation: impl Into<String>, duration: Duration) {
        self.samples
            .entry(operation.into())
            .or_default()
            .push(duration);
    }

    /// Number of samples recorded for `operation`.
    pub fn count(&self, operation: &str) -> usize {
        self.samples.get(operation).map_or(0, Vec::len)
    }

    /// Median (p50) latency for `operation`, if any sample was recorded.
    pub fn p50(&self, operation: &str) -> Option<Duration> {
        self.percentile(operation, 50)
    }

    /// 95th-percentile latency for `operation`, if any sample was recorded.
    pub fn p95(&self, operation: &str) -> Option<Duration> {
        self.percentile(operation, 95)
    }

    /// Nearest-rank percentile `p` (0..=100) of the samples for `operation`.
    pub fn percentile(&self, operation: &str, p: u32) -> Option<Duration> {
        self.samples
            .get(operation)
            .and_then(|values| nearest_rank_percentile(values, p))
    }

    /// All recorded operation names, sorted.
    pub fn operations(&self) -> impl Iterator<Item = &String> {
        self.samples.keys()
    }
}

/// Nearest-rank percentile of `values` (unsorted input accepted), ported from
/// the Go oracle's benchmark runner (`../synopsis/internal/benchmark/runner.go`,
/// `percentile()`): rank = ceil(p/100 * n) clamped to 1..=n. Returns `None` for
/// empty input — the oracle renders that as `0 ms`; the harness keeps it typed.
pub fn nearest_rank_percentile(values: &[Duration], p: u32) -> Option<Duration> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    // Port of the oracle's math.Ceil(p/100.0 * float64(n)); usize -> f64 is exact
    // for any realistic sample count, and ceil() never overflows back to usize.
    let rank = (f64::from(p) / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted.len());
    Some(sorted[idx - 1])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn percentile_empty_input_is_none() {
        assert_eq!(nearest_rank_percentile(&[], 50), None);
        assert_eq!(nearest_rank_percentile(&[], 95), None);
    }

    #[test]
    fn percentile_single_value_is_that_value_for_any_p() {
        for p in [0, 1, 50, 95, 99, 100] {
            assert_eq!(nearest_rank_percentile(&[ms(42)], p), Some(ms(42)));
        }
    }

    #[test]
    fn percentile_known_dataset_1_to_10() {
        let values: Vec<Duration> = (1..=10).map(ms).collect();
        // rank = ceil(p/100 * 10): p25 -> ceil(2.5) = 3, p50 -> 5, p95 -> ceil(9.5) = 10, p99 -> 10.
        assert_eq!(nearest_rank_percentile(&values, 25), Some(ms(3)));
        assert_eq!(nearest_rank_percentile(&values, 50), Some(ms(5)));
        assert_eq!(nearest_rank_percentile(&values, 95), Some(ms(10)));
        assert_eq!(nearest_rank_percentile(&values, 99), Some(ms(10)));
    }

    #[test]
    fn percentile_sorts_unsorted_input() {
        let values = [ms(9), ms(1), ms(5)];
        // sorted: [1, 5, 9]; p50 -> ceil(1.5) = 2nd element; p95 -> ceil(2.85) = 3rd.
        assert_eq!(nearest_rank_percentile(&values, 50), Some(ms(5)));
        assert_eq!(nearest_rank_percentile(&values, 95), Some(ms(9)));
    }

    #[test]
    fn percentile_two_values_matches_oracle_ranks() {
        let values = [ms(10), ms(20)];
        // Go oracle: p50 -> ceil(0.5*2) = 1st element; p95 -> ceil(1.9) = 2nd.
        assert_eq!(nearest_rank_percentile(&values, 50), Some(ms(10)));
        assert_eq!(nearest_rank_percentile(&values, 95), Some(ms(20)));
    }

    #[test]
    fn percentile_extreme_p_values_clamp_to_ends() {
        let values = [ms(7), ms(8)];
        // p=0 -> rank ceil(0) clamped to 1; p=100 -> last element.
        assert_eq!(nearest_rank_percentile(&values, 0), Some(ms(7)));
        assert_eq!(nearest_rank_percentile(&values, 100), Some(ms(8)));
    }

    #[test]
    fn timing_stats_counts_and_percentiles_per_operation() {
        let mut stats = TimingStats::default();
        for n in [3u64, 1, 2] {
            stats.record("tools/call:echo", ms(n));
        }
        stats.record("tools/list", ms(50));

        assert_eq!(stats.count("tools/call:echo"), 3);
        assert_eq!(stats.count("tools/list"), 1);
        // sorted samples [1, 2, 3]: p50 -> 2nd element; p95 -> 3rd.
        assert_eq!(stats.p50("tools/call:echo"), Some(ms(2)));
        assert_eq!(stats.p95("tools/call:echo"), Some(ms(3)));
        assert_eq!(stats.percentile("tools/list", 50), Some(ms(50)));

        // Unknown operation has no samples.
        assert_eq!(stats.count("nope"), 0);
        assert_eq!(stats.p50("nope"), None);
        assert_eq!(stats.p95("nope"), None);
    }

    #[test]
    fn timing_stats_operations_are_sorted() {
        let mut stats = TimingStats::default();
        stats.record("tools/call:z", ms(1));
        stats.record("initialize", ms(2));
        stats.record("tools/list", ms(3));
        assert_eq!(
            stats.operations().collect::<Vec<_>>(),
            ["initialize", "tools/call:z", "tools/list"]
        );
    }

    /// Round-trip integration test (task 3.1 acceptance b): in-process rmcp
    /// Streamable HTTP server with two dummy tools; the harness wrapper runs
    /// initialize/tools/list/tools/call through success and error paths with
    /// p50/p95 timing collected per operation.
    mod roundtrip {
        #![allow(clippy::unwrap_used, clippy::expect_used)]

        use super::*;
        use rmcp::{
            ErrorData, ServerHandler,
            model::{
                CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
                PaginatedRequestParams, ServerCapabilities, ServerInfo,
            },
            service::{RequestContext, RoleServer},
            transport::streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            },
        };

        /// Minimal in-process MCP server with two dummy tools.
        #[derive(Debug)]
        struct DummyServer;

        fn tool(name: &str) -> Tool {
            let schema = serde_json::Map::from_iter([(
                "type".to_owned(),
                serde_json::Value::String("object".into()),
            )]);
            Tool::new_with_raw(
                name.to_owned(),
                Some(format!("dummy {name} tool").into()),
                std::sync::Arc::from(schema),
            )
        }

        impl ServerHandler for DummyServer {
            fn get_info(&self) -> ServerInfo {
                let capabilities = ServerCapabilities::builder().enable_tools().build();
                ServerInfo::new(capabilities)
                    .with_server_info(Implementation::new("dummy-parity-server", "0.1.0"))
            }

            async fn list_tools(
                &self,
                _request: Option<PaginatedRequestParams>,
                _context: RequestContext<RoleServer>,
            ) -> Result<ListToolsResult, ErrorData> {
                let tools = vec![tool("echo"), tool("boom")];
                Ok(ListToolsResult::with_all_items(tools))
            }

            async fn call_tool(
                &self,
                request: CallToolRequestParams,
                _context: RequestContext<RoleServer>,
            ) -> Result<CallToolResponse, ErrorData> {
                let tool_name = &*request.name; // Cow<'static, str> -> &str
                match tool_name {
                    "echo" => {
                        let args = serde_json::to_string(
                            request
                                .arguments
                                .as_ref()
                                .unwrap_or(&serde_json::Map::new()),
                        )
                        .expect("arguments are a valid JSON object");
                        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                            ContentBlock::text(format!("echo:{args}")),
                        ])))
                    }
                    // Tool-level error: routed fine, execution failed.
                    "boom" => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                        ContentBlock::text("boom exploded"),
                    ]))),
                    other => Err(ErrorData::new(
                        ErrorCode::METHOD_NOT_FOUND,
                        format!("tool `{other}` not found"),
                        None,
                    )),
                }
            }
        }

        /// Boot the dummy server on a random localhost port; returns the MCP URL
        /// and a shutdown trigger for graceful axum termination.
        async fn spawn_server() -> (String, tokio::sync::oneshot::Sender<()>) {
            let service: StreamableHttpService<DummyServer, LocalSessionManager> =
                StreamableHttpService::new(
                    || Ok(DummyServer),
                    Default::default(),
                    StreamableHttpServerConfig::default().with_sse_keep_alive(None),
                );
            let router = axum::Router::new().nest_service("/mcp", service);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
            (format!("http://{addr}/mcp"), shutdown_tx)
        }

        #[tokio::test]
        async fn roundtrip_initialize_list_call_success_and_error_cases() {
            let (url, shutdown) = spawn_server().await;

            let mut client = McpClient::connect(&url).await.unwrap();

            // initialize: typed identity available, timing recorded.
            let peer = client.peer_info().expect("peer info after connect");
            assert_eq!(
                peer.server_info.as_ref().unwrap().name,
                "dummy-parity-server"
            );
            assert_eq!(client.stats().count("initialize"), 1);

            // tools/list: both dummy tools visible, p50/p95 collected.
            let tools = client.list_tools().await.unwrap();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            assert_eq!(names, vec!["echo", "boom"]);
            assert_eq!(client.stats().count("tools/list"), 1);
            assert!(client.stats().p50("tools/list").is_some());
            assert!(client.stats().p95("tools/list").is_some());

            // tools/call success: echo returns its arguments as text.
            let mut args = serde_json::Map::new();
            args.insert(
                "message".to_owned(),
                serde_json::Value::String("hi parity".into()),
            );
            let result = client.call_tool("echo", args).await.unwrap();
            assert_eq!(result.is_error, Some(false));
            let text = result
                .content
                .first()
                .unwrap()
                .as_text()
                .expect("text content");
            assert!(text.text.contains("hi parity"), "got: {}", text.text);

            // tools/call tool-level error: typed ToolFailed with the message.
            let err = client
                .call_tool("boom", serde_json::Map::new())
                .await
                .unwrap_err();
            match &err {
                HarnessError::ToolFailed { tool, detail } => {
                    assert_eq!(tool, "boom");
                    assert!(detail.contains("boom exploded"), "got: {detail}");
                }
                other => panic!("expected ToolFailed, got {other:?}"),
            }

            // tools/call unknown tool: typed UnknownTool (METHOD_NOT_FOUND).
            let err = client
                .call_tool("nope", serde_json::Map::new())
                .await
                .unwrap_err();
            match &err {
                HarnessError::UnknownTool { tool, message } => {
                    assert_eq!(tool, "nope");
                    assert!(message.contains("not found"), "got: {message}");
                }
                other => panic!("expected UnknownTool, got {other:?}"),
            }

            // p50/p95 collected per operation; failed calls are samples too.
            for op in ["tools/call:echo", "tools/call:boom", "tools/call:nope"] {
                assert_eq!(client.stats().count(op), 1, "{op}");
                assert!(client.stats().p50(op).is_some(), "{op} p50");
                assert!(client.stats().p95(op).is_some(), "{op} p95");
            }

            client.close().await.unwrap();
            shutdown.send(()).unwrap();
        }

        #[tokio::test]
        async fn connect_to_unreachable_endpoint_is_typed_error_not_panic() {
            // Nothing listens on this port: failure must surface as McpService.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener); // free the port; connect() gets connection refused

            let err = McpClient::connect(&format!("http://{addr}/mcp"))
                .await
                .expect_err("must fail typed");
            assert!(matches!(err, HarnessError::McpService { .. }));
        }
    }
}
