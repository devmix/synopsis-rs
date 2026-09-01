//! Legacy HTTP+SSE client for the parity harness (add-legacy-sse-transport
//! task 1.4).
//!
//! Wire reference: mcp-go v0.57.0 `server/sse.go` (pinned in
//! `../synopsis/go.mod`). `GET /sse` answers `200 text/event-stream`; the
//! first frame is an `event: endpoint` whose `data:` is the absolute message
//! URL (`…/message?sessionId=<id>`), and every subsequent `event: message`
//! frame carries one JSON-RPC 2.0 object. Requests go to that message URL via
//! `POST`, which answers `202 Accepted` with an empty body; the response (if
//! any) arrives back on the SSE stream.
//!
//! No new crate: the body is `reqwest`'s `bytes_stream()` (the `stream`
//! feature) and the `event:`/`data:` framing is parsed by hand — the format is
//! `event:`/`data:` lines terminated by a blank line, exactly as mcp-go emits
//! it.
//!
//! The client owns the session (id + body stream): [`SseClient::connect`]
//! performs the handshake and yields incoming `event: message` payloads as
//! [`serde_json::Value`]; [`SseClient::initialize`], [`SseClient::list_tools`]
//! and [`SseClient::call_tool`] build the JSON-RPC request, POST it, and wait
//! for the matching response by `id`. Failures are typed [`SseError`] values;
//! nothing panics.

use std::pin::Pin;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use tokio_stream::{Stream, StreamExt};

/// Per-frame read deadline: a live SSE session delivers each frame well within
/// this window; exceeding it means the peer is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One connected legacy-SSE session: the HTTP client, the session id, the
/// absolute message URL (from the `endpoint` frame), and the owned body stream.
///
/// `SseClient` is not `Debug`: it owns an opaque `reqwest` body stream. That
/// is fine for the harness — failures are reported as typed [`SseError`]
/// values, never by printing the client.
pub struct SseClient {
    http: Client,
    session_id: String,
    message_url: String,
    reader: SseReader,
    next_id: u64,
}

/// The owned `/sse` body stream plus the line buffer that reassembles frames
/// across chunk boundaries (hyper may split or coalesce frames arbitrarily).
struct SseReader {
    stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    buffer: String,
}

/// A typed failure of the legacy-SSE client.
#[derive(Debug)]
pub enum SseError {
    /// A reqwest HTTP failure (connect, send, or body read).
    Http(String),
    /// `GET /sse` did not answer `2xx`.
    BadSseStatus {
        /// The response status that was received.
        status: String,
    },
    /// The stream ended before the `endpoint` frame arrived.
    NoEndpointFrame,
    /// The first frame was not the `endpoint` event.
    BadEndpoint(String),
    /// The endpoint frame's message URL carried no `sessionId`.
    MissingSessionId,
    /// `POST /message` did not answer `202 Accepted`.
    UnexpectedStatus {
        /// The response status that was received instead.
        got: String,
    },
    /// A frame's `data:` payload was not valid JSON.
    Parse(String),
    /// The `/sse` stream ended before the expected response.
    StreamEnded,
    /// Reading a frame exceeded the deadline (the peer is dead).
    Timeout,
    /// The server answered with a JSON-RPC `error` object.
    JsonRpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
}

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(detail) => write!(f, "sse http error: {detail}"),
            Self::BadSseStatus { status } => write!(f, "GET /sse answered {status}, not 2xx"),
            Self::NoEndpointFrame => write!(f, "the /sse stream ended before the endpoint frame"),
            Self::BadEndpoint(event) => {
                write!(
                    f,
                    "the first /sse frame was `event: {event}`, not `endpoint`"
                )
            }
            Self::MissingSessionId => write!(f, "the endpoint frame carried no sessionId"),
            Self::UnexpectedStatus { got } => {
                write!(f, "POST /message answered {got}, not 202 Accepted")
            }
            Self::Parse(detail) => write!(f, "an SSE frame's data was not valid JSON: {detail}"),
            Self::StreamEnded => write!(f, "the /sse stream ended before the expected response"),
            Self::Timeout => write!(f, "reading an SSE frame timed out"),
            Self::JsonRpc { code, message } => {
                write!(
                    f,
                    "the server returned a JSON-RPC error (code {code}): {message}"
                )
            }
        }
    }
}

impl std::error::Error for SseError {}

impl SseClient {
    /// Connect to a legacy-SSE endpoint: `GET {base_url}/sse`, read the
    /// `endpoint` frame, extract the session id and the message URL, and keep
    /// the body stream for subsequent `event: message` frames (mcp-go v0.57.0
    /// `handleSSE`).
    pub async fn connect(base_url: &str) -> Result<Self, SseError> {
        let http = Client::new();
        let base = base_url.trim_end_matches('/');
        let url = format!("{base}/sse");
        let response = http
            .get(&url)
            .send()
            .await
            .map_err(|err| SseError::Http(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(SseError::BadSseStatus {
                status: status.to_string(),
            });
        }
        let mut reader = SseReader::new(response);

        // The first frame is the endpoint event; its data is the message URL.
        let first = reader
            .next_frame()
            .await?
            .ok_or(SseError::NoEndpointFrame)?;
        let (event, data) = parse_frame(&first)?;
        if event != "endpoint" {
            return Err(SseError::BadEndpoint(event));
        }
        let message_url = data;
        let session_id = extract_session_id(&message_url)?;

        Ok(Self {
            http,
            session_id,
            message_url,
            reader,
            next_id: 0,
        })
    }

    /// The session id from the `endpoint` frame (mcp-go v0.57.0's
    /// `sessionIDGenFunc` UUID).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// `initialize` (mcp-go v0.57.0 `handleInitialize`): returns the result
    /// object (`protocolVersion`, `capabilities`, `serverInfo`).
    pub async fn initialize(&mut self) -> Result<Value, SseError> {
        // A known protocol version so the server echoes it (the same value the
        // Streamable HTTP handshake negotiates).
        self.request(
            "initialize",
            Some(json!({ "protocolVersion": "2025-06-18" })),
        )
        .await
    }

    /// `tools/list`: returns the result object (`{"tools": [ … ]}`).
    pub async fn list_tools(&mut self) -> Result<Value, SseError> {
        self.request("tools/list", None).await
    }

    /// `tools/call` for `name` with JSON `arguments`: returns the result
    /// object (the MCP `CallToolResult`).
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, SseError> {
        let params = json!({ "name": name, "arguments": arguments });
        self.request("tools/call", Some(params)).await
    }

    /// POST one JSON-RPC request to the message URL and assert `202 Accepted`
    /// (mcp-go v0.57.0 `handleMessage`).
    async fn send(&self, request: &Value) -> Result<(), SseError> {
        let response = self
            .http
            .post(&self.message_url)
            .json(request)
            .send()
            .await
            .map_err(|err| SseError::Http(err.to_string()))?;
        let status = response.status();
        // Drain the (empty) body so the connection can be reused.
        let _ = response.bytes().await;
        if status.as_u16() != 202 {
            return Err(SseError::UnexpectedStatus {
                got: status.to_string(),
            });
        }
        Ok(())
    }

    /// Build a JSON-RPC request with a fresh id, send it, and wait for the
    /// response with the matching id. Returns the `result` object; a JSON-RPC
    /// `error` becomes [`SseError::JsonRpc`].
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, SseError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            request["params"] = params;
        }
        self.send(&request).await?;

        // Frames arrive in order on the single session stream; keep reading
        // until the response with our id (a keep-alive or stray frame is
        // skipped).
        loop {
            let response = self
                .reader
                .next_message()
                .await?
                .ok_or(SseError::StreamEnded)?;
            match response.get("id").and_then(Value::as_u64) {
                Some(matched) if matched == id => {
                    if let Some(error) = response.get("error") {
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        return Err(SseError::JsonRpc { code, message });
                    }
                    return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => continue,
            }
        }
    }
}

impl SseReader {
    fn new(response: reqwest::Response) -> Self {
        // `bytes_stream()` yields `bytes::Bytes`; convert to `Vec<u8>` so the
        // boxed stream does not name the `bytes` crate in this module's API.
        let stream = response
            .bytes_stream()
            .map(|result| result.map(|bytes| bytes.to_vec()));
        Self::from_stream(stream)
    }

    fn from_stream(
        stream: impl Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send + 'static,
    ) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: String::new(),
        }
    }

    /// Read the next complete SSE frame (the text up to the blank line), or
    /// `None` when the stream ends.
    async fn next_frame(&mut self) -> Result<Option<String>, SseError> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame = self.buffer[..end].to_owned();
                self.buffer = self.buffer[end + 2..].to_owned();
                return Ok(Some(frame));
            }
            let chunk = match tokio::time::timeout(READ_TIMEOUT, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(err))) => return Err(SseError::Http(err.to_string())),
                Ok(None) => return Ok(None),
                Err(_) => return Err(SseError::Timeout),
            };
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// Read the next `event: message` frame and parse its `data:` payload as
    /// JSON, skipping any non-`message` frame (e.g. a keep-alive ping).
    async fn next_message(&mut self) -> Result<Option<Value>, SseError> {
        loop {
            let Some(frame) = self.next_frame().await? else {
                return Ok(None);
            };
            let (event, data) = parse_frame(&frame)?;
            if event == "message" {
                let value =
                    serde_json::from_str(&data).map_err(|err| SseError::Parse(err.to_string()))?;
                return Ok(Some(value));
            }
        }
    }
}

/// Split one SSE frame into its `event:` name and `data:` payload. A missing
/// `event:` defaults to `"message"` (the SSE spec); multiple `data:` lines are
/// joined with `\n` (the spec). `id:`, `retry:` and comment lines are ignored.
fn parse_frame(frame: &str) -> Result<(String, String), SseError> {
    let mut event = String::from("message");
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue; // comment line
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
    }
    Ok((event, data_lines.join("\n")))
}

/// The `sessionId` query parameter of the message URL (mcp-go v0.57.0's
/// `/message?sessionId=<id>`).
fn extract_session_id(message_url: &str) -> Result<String, SseError> {
    let query = message_url.rsplit_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        let Some(value) = pair.strip_prefix("sessionId=") else {
            continue;
        };
        if !value.is_empty() {
            return Ok(value.to_owned());
        }
    }
    Err(SseError::MissingSessionId)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn frame(event: &str, data: &str) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    #[test]
    fn parse_frame_extracts_event_and_data() {
        let (event, data) = parse_frame(&frame("message", r#"{"a":1}"#)).unwrap();
        assert_eq!(event, "message");
        assert_eq!(data, r#"{"a":1}"#);
    }

    #[test]
    fn parse_frame_defaults_event_to_message_when_absent() {
        let (event, data) = parse_frame("data: hi\n\n").unwrap();
        assert_eq!(event, "message");
        assert_eq!(data, "hi");
    }

    #[test]
    fn parse_frame_joins_multiline_data_with_newline() {
        let (event, data) = parse_frame("event: message\ndata: a\ndata: b\n\n").unwrap();
        assert_eq!(event, "message");
        assert_eq!(data, "a\nb");
    }

    #[test]
    fn parse_frame_ignores_comments_and_id_lines() {
        let (event, data) =
            parse_frame(": keep-alive\nid: 7\nevent: message\ndata: x\nretry: 5\n\n").unwrap();
        assert_eq!(event, "message");
        assert_eq!(data, "x");
    }

    #[test]
    fn extract_session_id_reads_the_query_parameter() {
        assert_eq!(
            extract_session_id("http://localhost:8080/message?sessionId=abc-123").unwrap(),
            "abc-123"
        );
        // sessionId is not the first parameter.
        assert_eq!(
            extract_session_id("http://h/m?a=1&sessionId=xyz&b=2").unwrap(),
            "xyz"
        );
    }

    #[test]
    fn extract_session_id_is_missing_without_the_parameter() {
        assert!(matches!(
            extract_session_id("http://localhost:8080/message"),
            Err(SseError::MissingSessionId)
        ));
    }

    /// Frame reassembly across chunk boundaries: hyper may split a frame across
    /// arbitrary chunk sizes (or coalesce several frames in one chunk); the
    /// reader must reassemble by the blank-line delimiter either way.
    #[tokio::test]
    async fn next_frame_reassembles_across_chunk_boundaries() {
        let full = "event: endpoint\ndata: http://x/m?sessionId=abc\n\n\
                    event: message\ndata: {\"id\":1}\n\n";
        // Split at an arbitrary point inside the first frame.
        let (head, tail) = full.split_at(10);
        let chunks: Vec<Result<Vec<u8>, reqwest::Error>> =
            vec![Ok(head.as_bytes().to_vec()), Ok(tail.as_bytes().to_vec())];
        let mut reader = SseReader::from_stream(tokio_stream::iter(chunks));

        let frame1 = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(frame1, "event: endpoint\ndata: http://x/m?sessionId=abc");
        let frame2 = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(frame2, "event: message\ndata: {\"id\":1}");
        // The stream is drained.
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    /// Several frames coalesced into a single chunk are still split apart.
    #[tokio::test]
    async fn next_frame_splits_coalesced_frames() {
        let full = "event: message\ndata: {\"id\":1}\n\nevent: message\ndata: {\"id\":2}\n\n";
        let chunks: Vec<Result<Vec<u8>, reqwest::Error>> = vec![Ok(full.as_bytes().to_vec())];
        let mut reader = SseReader::from_stream(tokio_stream::iter(chunks));
        assert_eq!(
            reader.next_frame().await.unwrap().unwrap(),
            "event: message\ndata: {\"id\":1}"
        );
        assert_eq!(
            reader.next_frame().await.unwrap().unwrap(),
            "event: message\ndata: {\"id\":2}"
        );
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    /// `next_message` skips non-`message` frames (e.g. a keep-alive ping) and
    /// parses the `data:` payload of the next `event: message` frame as JSON.
    #[tokio::test]
    async fn next_message_skips_non_message_and_parses_json() {
        let full = "event: ping\ndata: \n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"ok\":true}}\n\n";
        let chunks: Vec<Result<Vec<u8>, reqwest::Error>> = vec![Ok(full.as_bytes().to_vec())];
        let mut reader = SseReader::from_stream(tokio_stream::iter(chunks));
        let message = reader.next_message().await.unwrap().unwrap();
        assert_eq!(message["id"], json!(0));
        assert_eq!(message["result"]["ok"], json!(true));
    }

    /// `next_message` returns `None` once the stream ends (no frames left).
    #[tokio::test]
    async fn next_message_is_none_when_the_stream_ends() {
        let chunks: Vec<Result<Vec<u8>, reqwest::Error>> = vec![];
        let mut reader = SseReader::from_stream(tokio_stream::iter(chunks));
        assert!(reader.next_message().await.unwrap().is_none());
    }
}
