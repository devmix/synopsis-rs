//! Legacy HTTP+SSE transport (mcp-go v0.57.0 `SSEServer` wire contract).
//!
//! Task 1.1 of `add-legacy-sse-transport`: the session model and the
//! `GET /sse` handler. The wire reference is mcp-go v0.57.0 `server/sse.go`
//! `handleSSE` (module cache, read-only): `200 text/event-stream`, the first
//! frame is an `event: endpoint` whose `data:` is the message URL, and the
//! session's outbound channel is then streamed as `event: message` frames.
//!
//! **Deployment model (general-service, user decision 2026-09-01):** the
//! server is a self-hosted service for general use — potentially behind a
//! TLS-terminating reverse proxy with multiple concurrent clients (the 16 GB
//! memory constraint is unchanged). Two deliberate, justified breaks from the
//! oracle's wire (design D2 revision):
//! - **No wildcard CORS.** mcp-go emits `Access-Control-Allow-Origin: *` by
//!   default — a security anti-pattern for a general service. The Rust server
//!   sends none; a deploying proxy may add explicit CORS.
//! - **Proxy-aware endpoint URL.** The oracle hardcodes `http://` (empty
//!   `baseURL`), which is a dead message URL behind TLS termination. The
//!   endpoint URL takes scheme from `X-Forwarded-Proto` (first value if a
//!   comma-list, lowercased) else `http`, and host from `X-Forwarded-Host`
//!   else the `Host` header.
//!
//! Design decisions (see `design.md`):
//! - D3 — in-memory session map, bounded per-session channel (capacity
//!   [`CHANNEL_CAPACITY`]) with a `send()` backpressure helper; `tx` is private
//!   (encapsulation). Zero new crates.
//! - D9 — `last_activity` is set on create and refreshed by `touch()`; task
//!   1.5's idle reaper reads it.
//! - D6 — no `CloseSessions` equivalent: the client disconnect drops the body,
//!   which removes the session (the [`SessionGuard`]).
//!
//! Task 1.2 adds `POST /message` ([`handle_message`]): session validation
//! (400 + pinned JSON-RPC error body), `202 Accepted`, background dispatch
//! through the JSON-RPC method table ([`crate::transport::jsonrpc`]) and the
//! response pushed back on the session's SSE channel. The router wiring
//! arrives in task 1.3. This module is tested in isolation.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{FromRef, Query, State};
use axum::http::header;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;
use sse_stream::Sse;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt, once};
use uuid::Uuid;

use crate::server::Server;
use crate::transport::jsonrpc::{self, INVALID_PARAMS, JsonRpcResponse, PARSE_ERROR};

/// Bounded per-session channel capacity (design D3 revision): a slow/dead
/// client must not accumulate unbounded frames in RAM under the 16 GB
/// constraint; 64 small JSON-RPC frames is far beyond any real client's burst.
const CHANNEL_CAPACITY: usize = 64;

/// Backpressure send deadline (design D3 revision): a client that cannot drain
/// the [`CHANNEL_CAPACITY`]-frame buffer within this window is considered dead
/// and is reaped.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Non-standard proxy headers (not in `http::header`); the service may sit
/// behind a TLS-terminating reverse proxy (design D2 revision).
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// One body frame: the SSE-encoded bytes, or an error. This stream never
/// produces an error, but the axum body type requires an error channel.
type Frame = std::result::Result<Bytes, axum::BoxError>;

/// One active SSE connection (design D3).
///
/// `tx` is the bounded outbound channel (capacity [`CHANNEL_CAPACITY`]); each
/// payload is one JSON-RPC 2.0 object string. `last_activity` is refreshed by
/// [`SseSessionMap::touch`] and read by the idle reaper (design D9, task 1.5).
/// Both fields are private: the [`SseSessionMap`] is the only writer (design D3
/// revision — encapsulation, no raw `tx` escape).
pub struct SseSession {
    tx: mpsc::Sender<String>,
    last_activity: Instant,
}

/// In-memory registry of active SSE sessions (design D3).
///
/// Backed by `Arc<Mutex<HashMap<id, SseSession>>>`: the lock is held only for
/// HashMap ops (microseconds), never across I/O or `.await` — fine at
/// service-scale session counts. DashMap would be a new dependency (rejected
/// per the frozen stack). A session lives until its SSE stream closes (client
/// disconnect or server shutdown, design D6) or is reaped by the idle timeout
/// (design D9, task 1.5).
#[derive(Clone)]
pub struct SseSessionMap(Arc<Mutex<HashMap<String, SseSession>>>);

impl SseSessionMap {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Create a session with a fresh server-generated UUIDv4 id (mcp-go
    /// `NewSSEServer`'s default `sessionIDGenFunc`), a bounded outbound channel
    /// (capacity [`CHANNEL_CAPACITY`], design D3 revision), and
    /// `last_activity = now`. Returns the id plus the receiver end of the
    /// channel (which the `/sse` body stream consumes).
    pub fn create(&self) -> (String, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let id = Uuid::new_v4().to_string();
        self.lock().insert(
            id.clone(),
            SseSession {
                tx,
                last_activity: Instant::now(),
            },
        );
        (id, rx)
    }

    /// Send one payload to a session's bounded channel (design D3 revision:
    /// backpressure + encapsulation). Clones the sender under a brief lock,
    /// then awaits `tx.send(payload)` under a [`SEND_TIMEOUT`] deadline — the
    /// lock is never held across the await. On timeout (a client that cannot
    /// drain the buffer is dead) or channel-closed (the receiver was dropped),
    /// the session is removed and `Err` is returned; task 1.2 maps that to an
    /// HTTP error per mcp-go v0.57.0.
    pub async fn send(
        &self,
        id: &str,
        payload: String,
    ) -> Result<(), mpsc::error::SendError<String>> {
        let Some(tx) = self.get(id) else {
            // Unknown session: nothing to remove, nothing to send.
            return Err(mpsc::error::SendError(payload));
        };
        match tokio::time::timeout(SEND_TIMEOUT, tx.send(payload)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(send_err)) => {
                // Channel closed: the receiver was dropped (client gone).
                self.remove(id);
                Err(send_err)
            }
            Err(_elapsed) => {
                // Backpressure timeout: the client cannot drain the buffer.
                self.remove(id);
                Err(mpsc::error::SendError(String::from(
                    "sse send timed out (client not draining)",
                )))
            }
        }
    }

    /// Refresh the session's `last_activity` (design D3/D9). Task 1.2's
    /// `POST /message` handler calls this on every request; task 1.5's reaper
    /// reads it to reap idle sessions. `true` if the session existed.
    pub fn touch(&self, id: &str) -> bool {
        let mut guard = self.lock();
        match guard.get_mut(id) {
            Some(session) => {
                session.last_activity = Instant::now();
                true
            }
            None => false,
        }
    }

    /// Clone a session's outbound sender by id (tests/internal use), or `None`
    /// if the id is unknown (task 1.2's `POST /message` maps that to a 400).
    pub fn get(&self, id: &str) -> Option<mpsc::Sender<String>> {
        self.lock().get(id).map(|session| session.tx.clone())
    }

    /// Remove a session by id; `true` if it was present.
    pub fn remove(&self, id: &str) -> bool {
        self.lock().remove(id).is_some()
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// `true` when no sessions are active.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SseSession>> {
        // A poisoned lock only occurs if a holder panicked mid-mutation; the
        // map is still intact, so recover it (the no-panic rule, design D7).
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for SseSessionMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared axum state for the legacy SSE routes (design D5): the session
/// registry plus the MCP server handle [`handle_message`] dispatches through.
/// `handle_sse` takes `State<SseSessionMap>` and resolves it from this state
/// via the [`FromRef`] impl below (axum's `State` extractor accepts any
/// router state with a `FromRef` into the target type), so the task 1.1
/// handler signature is unchanged.
#[derive(Clone)]
pub struct SseState {
    /// The active SSE sessions (task 1.1).
    pub sessions: SseSessionMap,
    /// The MCP server (the same handle the Streamable HTTP path serves from).
    pub server: Arc<Server>,
}

impl FromRef<SseState> for SseSessionMap {
    fn from_ref(state: &SseState) -> Self {
        state.sessions.clone()
    }
}

/// Build the absolute message-endpoint URL for the `endpoint` event (design D2
/// revision — proxy-aware). Scheme: `X-Forwarded-Proto` (first value if a
/// comma-list, lowercased) else `http`. Host: `X-Forwarded-Host` else the
/// `Host` header. A hardcoded `http://` would hand clients a dead message URL
/// behind a TLS-terminating proxy.
fn endpoint_url(headers: &HeaderMap, id: &str) -> String {
    let scheme = headers
        .get(X_FORWARDED_PROTO)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "http".to_owned());
    let host = headers
        .get(X_FORWARDED_HOST)
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("localhost");
    format!("{scheme}://{host}/message?sessionId={id}")
}

/// Encode one SSE event frame per the SSE spec (WHATWG §8.2).
///
/// Every line of the event's `data` is prefixed with its own `data:` field —
/// a bare newline inside a single `data:` field would terminate the field
/// early, so multi-line payloads must be split. The frame ends with a blank
/// line. sse-stream's built-in `From<Sse> for Bytes` writes the raw data
/// verbatim (no splitting), so the spec-correct splitting is done here while
/// the pipeline still uses sse-stream's [`Sse`] as the typed frame.
fn encode_sse_frame(sse: &Sse) -> String {
    // An SSE event with no `event:` field is a "message" event by default; the
    // pipeline always sets both fields, so the fallbacks are just safety nets.
    let event = sse.event.as_deref().unwrap_or("message");
    let data = sse.data.as_deref().unwrap_or("");
    let mut out = String::new();
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Removes the session from the registry when dropped — either the client
/// disconnected (axum/hyper drops the response body) or the stream ended (the
/// channel closed / the idle reaper dropped the last sender). The Rust form of
/// mcp-go's `defer s.sessions.Delete(sessionID)` (design D6).
struct SessionGuard {
    sessions: SseSessionMap,
    id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.id);
    }
}

/// The `/sse` response body: the endpoint frame followed by the session's
/// message frames. Carries a [`SessionGuard`] so the session is removed from
/// the registry exactly when this body is dropped (client disconnect).
struct SseBodyStream {
    inner: Pin<Box<dyn Stream<Item = Frame> + Send>>,
    _guard: SessionGuard,
}

impl Stream for SseBodyStream {
    type Item = Frame;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `SseBodyStream` is `Unpin` (both fields are), so projecting through
        // `get_mut` is sound; the inner stream is itself pinned.
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

/// `GET /sse` handler (mcp-go v0.57.0 `handleSSE`): mint a session, reply
/// `200 text/event-stream` + `Cache-Control: no-cache` with the `endpoint`
/// event first (proxy-aware absolute URL), then stream the session's bounded
/// channel as `event: message` frames. On client disconnect the body drops and
/// the session is removed (design D6). Deliberately NO `Access-Control-Allow-
/// Origin` header (design D2 revision — justified break from the oracle's
/// wildcard CORS).
pub async fn handle_sse(
    State(sessions): State<SseSessionMap>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (id, rx) = sessions.create();

    // Proxy-aware absolute URL (design D2 revision); the oracle hardcodes
    // http:// (empty baseURL), which is a dead URL behind TLS termination.
    let endpoint_url = endpoint_url(&headers, &id);

    let endpoint = Sse::default().event("endpoint").data(endpoint_url);
    let message_frames =
        ReceiverStream::new(rx).map(|payload| Sse::default().event("message").data(payload));
    let frames = once(endpoint).chain(message_frames);
    let body_stream =
        frames.map(|sse| Ok::<_, axum::BoxError>(Bytes::from(encode_sse_frame(&sse))));
    let inner: Pin<Box<dyn Stream<Item = Frame> + Send>> = Box::pin(body_stream);

    let body = Body::from_stream(SseBodyStream {
        inner,
        _guard: SessionGuard { sessions, id },
    });

    // mcp-go v0.57.0 handleSSE headers: text/event-stream, no-cache, keep-alive.
    // NO Access-Control-Allow-Origin (design D2 revision — the oracle's `*` is
    // a security anti-pattern for a general-use service; a deploying proxy may
    // add explicit CORS).
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

/// The `sessionId` query parameter of `POST /message` (mcp-go v0.57.0 sse.go
/// `r.URL.Query().Get("sessionId")`).
#[derive(Debug, Deserialize)]
pub struct MessageQuery {
    /// The session id from the `endpoint` event; missing/empty → 400.
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// The pinned mcp-go v0.57.0 400 response (sse.go handleMessage +
/// jsonrpc_error.go writeJSONRPCError): `400 Bad Request`,
/// `Content-Type: application/json`, body = the JSON-RPC error object with
/// id null.
fn http_jsonrpc_error(code: i32, message: &str) -> Response {
    let response = JsonRpcResponse::error(Value::Null, code, message);
    let body = serde_json::to_string(&response).unwrap_or_else(|err| err.to_string());
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// `POST /message?sessionId=<id>` handler (mcp-go v0.57.0 `handleMessage`):
/// validate the session (missing/unknown → `400` + the pinned JSON-RPC error
/// body), answer `202 Accepted` with an empty body, then dispatch the
/// JSON-RPC message in the background and push the response (if any) onto
/// the session's SSE channel as an `event: message` frame.
///
/// mcp-go v0.57.0 sse.go: "quick return request, send 202 Accepted with no
/// body, then deal the message and sent response via SSE". A failed channel
/// send after the 202 is dropped (mcp-go logs "Event queue full" for a full
/// queue and silently skips a closed session; the library stays logger-less)
/// — there is no HTTP error left to report. A non-POST method is rejected by
/// the route (axum 405) rather than mcp-go's 400 "Method not allowed" body;
/// the pinned contract covers the POST leg.
pub async fn handle_message(
    State(state): State<SseState>,
    Query(query): Query<MessageQuery>,
    body: Bytes,
) -> Response {
    // mcp-go v0.57.0 sse.go handleMessage: a missing sessionId → 400 with the
    // JSON-RPC INVALID_PARAMS error body (id null, "Missing sessionId").
    let Some(session_id) = query.session_id.filter(|id| !id.is_empty()) else {
        return http_jsonrpc_error(INVALID_PARAMS, "Missing sessionId");
    };
    // mcp-go v0.57.0 sse.go handleMessage: an unknown sessionId → 400 with
    // the JSON-RPC INVALID_PARAMS error body (id null, "Invalid session
    // ID"). `touch` doubles as the existence check and refreshes
    // last_activity for the idle reaper (design D9, task 1.5).
    if !state.sessions.touch(&session_id) {
        return http_jsonrpc_error(INVALID_PARAMS, "Invalid session ID");
    }
    // mcp-go v0.57.0 sse.go handleMessage: the body must decode as raw JSON
    // (json.RawMessage) → 400 + the JSON-RPC PARSE_ERROR body ("Parse
    // error", id null). A structurally invalid JSON-RPC message (valid JSON)
    // is NOT a 400 here: mcp-go answers 202 and pushes the -32700 response
    // over the SSE stream (jsonrpc::dispatch).
    let Ok(raw) = serde_json::from_slice::<Value>(&body) else {
        return http_jsonrpc_error(PARSE_ERROR, "Parse error");
    };
    let sessions = state.sessions.clone();
    let server = state.server.clone();
    tokio::spawn(async move {
        if let Some(response) = jsonrpc::dispatch(&server, &raw).await {
            let payload = serde_json::to_string(&response).unwrap_or_else(|err| err.to_string());
            // A send failure after the 202 is a dead/reaped session: mcp-go
            // logs and drops the frame (pinned); SseSessionMap::send already
            // removed the dead session (design D3).
            let _ = sessions.send(&session_id, payload).await;
        }
    });
    // mcp-go v0.57.0 sse.go handleMessage: 202 Accepted, empty body.
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use std::time::Duration;
    use tower::ServiceExt;

    // --- endpoint_url (proxy-aware, design D2 revision) ---

    #[test]
    fn endpoint_url_no_proxy_headers_uses_http_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:3000".parse().unwrap());
        assert_eq!(
            endpoint_url(&headers, "abc-123"),
            "http://localhost:3000/message?sessionId=abc-123"
        );
    }

    #[test]
    fn endpoint_url_uses_forwarded_proto_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_PROTO, "https".parse().unwrap());
        headers.insert(X_FORWARDED_HOST, "svc.example.com".parse().unwrap());
        assert_eq!(
            endpoint_url(&headers, "abc-123"),
            "https://svc.example.com/message?sessionId=abc-123"
        );
    }

    #[test]
    fn endpoint_url_comma_list_proto_first_value_wins() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_PROTO, "https, http".parse().unwrap());
        headers.insert(header::HOST, "localhost:3000".parse().unwrap());
        assert_eq!(
            endpoint_url(&headers, "abc-123"),
            "https://localhost:3000/message?sessionId=abc-123"
        );
    }

    // --- SSE frame bytes (wire contract) ---

    #[test]
    fn endpoint_frame_bytes_match_wire_contract() {
        let sse = Sse::default()
            .event("endpoint")
            .data("http://localhost:3000/message?sessionId=abc-123");
        let frame = encode_sse_frame(&sse);
        assert_eq!(
            frame,
            "event: endpoint\ndata: http://localhost:3000/message?sessionId=abc-123\n\n"
        );
    }

    #[test]
    fn message_frame_bytes_match_wire_contract() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let sse = Sse::default().event("message").data(json);
        let frame = encode_sse_frame(&sse);
        assert_eq!(frame, format!("event: message\ndata: {json}\n\n"));
    }

    #[test]
    fn multiline_data_is_split_into_data_fields() {
        // A bare newline inside one `data:` field would end the field early;
        // the SSE spec requires a `data:` prefix on every line.
        let sse = Sse::default().event("message").data("line1\nline2");
        let frame = encode_sse_frame(&sse);
        assert_eq!(frame, "event: message\ndata: line1\ndata: line2\n\n");
    }

    // --- session map ops ---

    #[test]
    fn session_map_create_get_remove_touch_round_trip() {
        let map = SseSessionMap::new();
        assert!(map.is_empty());

        let (id, _rx) = map.create();
        assert_eq!(map.len(), 1);
        assert!(map.get(&id).is_some());
        assert!(map.touch(&id)); // known session refreshes activity
        assert!(map.remove(&id));
        assert!(map.get(&id).is_none());
        assert!(map.is_empty());
    }

    #[test]
    fn session_map_unknown_id_get_none_touch_false() {
        let map = SseSessionMap::new();
        assert!(map.get("no-such-id").is_none());
        assert!(!map.touch("no-such-id"));
        assert!(!map.remove("no-such-id"));
    }

    #[test]
    fn session_ids_are_unique_uuid_v4() {
        let map = SseSessionMap::new();
        let (a, _) = map.create();
        let (b, _) = map.create();
        assert_ne!(a, b);
        // UUIDv4: 36 chars, version nibble (index 14) is '4'.
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
    }

    /// Backpressure (design D3 revision): the per-session channel is bounded to
    /// [`CHANNEL_CAPACITY`], so the 65th `try_send` fails with `Full`.
    #[test]
    fn backpressure_channel_is_bounded() {
        let map = SseSessionMap::new();
        let (id, _rx) = map.create();
        let tx = map.get(&id).unwrap();
        for i in 0..CHANNEL_CAPACITY {
            tx.try_send(format!("frame-{i}")).unwrap();
        }
        assert!(matches!(
            tx.try_send(String::from("frame-65")),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    /// `send()` to a session whose receiver was dropped removes the session and
    /// returns `Err` (design D3 revision).
    #[tokio::test]
    async fn send_to_dropped_receiver_removes_session_and_errors() {
        let map = SseSessionMap::new();
        let (id, rx) = map.create();
        drop(rx); // client gone: channel closed
        let result = map.send(&id, "payload".to_owned()).await;
        assert!(result.is_err());
        assert!(map.get(&id).is_none()); // session removed
        assert!(map.is_empty());
    }

    // --- in-process axum (handler) ---

    /// In-process axum test (task 1.1 acceptance criterion): a Router with ONLY
    /// `GET /sse` returns 200 `text/event-stream` + `no-cache` and NO
    /// `access-control-allow-origin`, the first frame is the proxy-aware
    /// endpoint event, and the session is removed from the registry when the
    /// client (the response body) is dropped.
    #[tokio::test]
    async fn sse_endpoint_serves_endpoint_event_and_cleans_up_on_drop() {
        let sessions = SseSessionMap::new();
        let app = Router::new()
            .route("/sse", get(handle_sse))
            .with_state(sessions.clone());

        let response = app
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
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        // Deliberate break from the oracle's wildcard CORS (design D2 revision).
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "must not emit Access-Control-Allow-Origin"
        );

        // The connection registered a session.
        assert_eq!(sessions.len(), 1);

        // Read the first frame: the endpoint event (http:// + Host, no proxy).
        let mut data_stream = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), data_stream.next())
            .await
            .expect("endpoint frame must arrive")
            .expect("stream must not end before the endpoint frame")
            .expect("frame must not be an error");
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(
            first.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
            "got: {first}"
        );
        assert!(first.ends_with("\n\n"), "got: {first}");

        // Drop the body: the client disconnected, so the session is removed.
        drop(data_stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !sessions.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session must be removed on disconnect");
        assert!(sessions.is_empty());
    }

    /// A payload pushed to the handler's session (reached via the session id in
    /// the endpoint frame) arrives as an `event: message` frame on the same
    /// stream — the bounded channel → stream → frame pipeline, end-to-end.
    #[tokio::test]
    async fn channel_payload_streams_as_message_frame() {
        use std::str::FromStr;

        let sessions = SseSessionMap::new();
        let app = Router::new()
            .route("/sse", get(handle_sse))
            .with_state(sessions.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sse")
                    .header(header::HOST, "localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut data_stream = response.into_body().into_data_stream();

        // The endpoint frame is first; it carries the handler's session id.
        let first = data_stream.next().await.unwrap().unwrap();
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(
            first.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
            "got: {first}"
        );
        let data_line = first
            .lines()
            .find(|line| line.starts_with("data: "))
            .unwrap();
        let session_id = data_line
            .strip_prefix("data: ")
            .and_then(|url| url.split("sessionId=").nth(1))
            .expect("endpoint URL carries a sessionId");
        assert!(Uuid::from_str(session_id).is_ok(), "got: {session_id}");

        // Push a payload via the map's send() (task 1.2's API); it must arrive
        // as an event: message frame.
        let payload = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        sessions.send(session_id, payload.to_owned()).await.unwrap();

        let second = data_stream.next().await.unwrap().unwrap();
        let second = String::from_utf8(second.to_vec()).unwrap();
        assert_eq!(second, format!("event: message\ndata: {payload}\n\n"));

        drop(data_stream);
    }

    // --- POST /message (task 1.2) ---

    use crate::transport::test_util::test_server;
    use axum::routing::post;
    use serde_json::json;

    /// Accumulates body chunks and returns complete SSE frames (terminated by
    /// the blank line), keeping the leftover bytes for the next call —
    /// hyper may split or coalesce frames across chunk boundaries.
    struct FrameReader {
        buffer: String,
    }

    impl FrameReader {
        fn new() -> Self {
            Self {
                buffer: String::new(),
            }
        }

        async fn next_frame<S>(&mut self, stream: &mut S) -> Option<String>
        where
            S: StreamExt<Item = std::result::Result<Bytes, axum::Error>> + Unpin,
        {
            loop {
                if let Some(end) = self.buffer.find("\n\n") {
                    let frame = self.buffer[..end].to_owned();
                    self.buffer = self.buffer[end + 2..].to_owned();
                    return Some(frame);
                }
                match stream.next().await {
                    Some(Ok(chunk)) => self.buffer.push_str(&String::from_utf8_lossy(&chunk)),
                    Some(Err(_)) | None => return None,
                }
            }
        }
    }

    /// Read the next `event: message` frame and parse its (single-line) data
    /// payload as a JSON-RPC object.
    async fn next_json_frame<S>(reader: &mut FrameReader, stream: &mut S) -> Value
    where
        S: StreamExt<Item = std::result::Result<Bytes, axum::Error>> + Unpin,
    {
        let frame = tokio::time::timeout(Duration::from_secs(5), reader.next_frame(stream))
            .await
            .expect("frame must arrive")
            .expect("stream must not end");
        assert!(
            frame.starts_with("event: message\n"),
            "message frame expected, got: {frame}"
        );
        let data_line = frame
            .lines()
            .find(|line| line.starts_with("data: "))
            .expect("message frame carries data");
        serde_json::from_str(data_line.strip_prefix("data: ").unwrap())
            .expect("valid JSON-RPC payload")
    }

    /// A `POST /message` request (optionally with a sessionId).
    fn post_message(session_id: Option<&str>, body: Value) -> Request<Body> {
        let uri = match session_id {
            Some(id) => format!("/message?sessionId={id}"),
            None => "/message".to_owned(),
        };
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap()
    }

    /// A `/message`-only router with a fresh session map and fixture server.
    /// Returns the finalized `Router<()>` (axum only implements `Service`
    /// for state-consumed routers).
    fn message_router() -> Router {
        Router::new()
            .route("/message", post(handle_message))
            .with_state(SseState {
                sessions: SseSessionMap::new(),
                server: Arc::new(test_server()),
            })
    }

    /// POST one JSON-RPC message to `/message`; returns the response status.
    async fn send_message(app: &Router, session_id: &str, body: Value) -> StatusCode {
        app.clone()
            .oneshot(post_message(Some(session_id), body))
            .await
            .unwrap()
            .status()
    }

    /// POST /message without a sessionId → 400 + the pinned JSON-RPC error
    /// body (mcp-go v0.57.0 sse.go: INVALID_PARAMS, "Missing sessionId", id
    /// null, Content-Type application/json).
    #[tokio::test]
    async fn message_without_session_id_is_400() {
        let app = message_router();
        let response = app
            .clone()
            .oneshot(post_message(
                None,
                json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed,
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32602,"message":"Missing sessionId"}})
        );
    }

    /// POST /message with an unknown sessionId → 400 + the pinned JSON-RPC
    /// error body (mcp-go v0.57.0 sse.go: INVALID_PARAMS, "Invalid session
    /// ID", id null).
    #[tokio::test]
    async fn message_with_unknown_session_id_is_400() {
        let app = message_router();
        let response = app
            .clone()
            .oneshot(post_message(
                Some("no-such-session"),
                json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["error"]["code"], json!(-32602));
        assert_eq!(parsed["error"]["message"], "Invalid session ID");
        assert!(parsed["id"].is_null());
    }

    /// POST /message with a valid session but a non-JSON body → 400 + the
    /// pinned PARSE_ERROR body (mcp-go v0.57.0 sse.go: "Parse error", id
    /// null). A structurally invalid JSON-RPC message (valid JSON) instead
    /// gets 202 + a -32700 frame on the stream (jsonrpc::dispatch).
    #[tokio::test]
    async fn message_with_malformed_body_is_400_parse_error() {
        let sessions = SseSessionMap::new();
        let (id, _rx) = sessions.create();
        let app = Router::new()
            .route("/message", post(handle_message))
            .with_state(SseState {
                sessions: sessions.clone(),
                server: Arc::new(test_server()),
            });
        let request = Request::builder()
            .method("POST")
            .uri(format!("/message?sessionId={id}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["error"]["code"], json!(-32700));
        assert_eq!(parsed["error"]["message"], "Parse error");
        assert!(parsed["id"].is_null());
    }

    /// In-process axum round-trip (task 1.2 acceptance criterion): GET /sse
    /// → read the endpoint event → POST initialize + tools/list + tools/call
    /// against a Server built on a temp fixture DB → the responses arrive
    /// over the SSE stream in order, each after its `202 Accepted`.
    #[tokio::test]
    async fn message_round_trip_over_sse() {
        let sessions = SseSessionMap::new();
        let app = Router::new()
            .route("/sse", get(handle_sse))
            .route("/message", post(handle_message))
            .with_state(SseState {
                sessions: sessions.clone(),
                server: Arc::new(test_server()),
            });

        // 1. GET /sse → the endpoint event carries the session id.
        let response = app
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
        let mut reader = FrameReader::new();
        let endpoint = reader
            .next_frame(&mut data)
            .await
            .expect("endpoint frame arrives");
        let session_id = endpoint
            .lines()
            .find(|line| line.starts_with("data: "))
            .expect("endpoint frame carries data")
            .strip_prefix("data: ")
            .and_then(|url| url.split("sessionId=").nth(1))
            .expect("endpoint URL carries a sessionId")
            .to_owned();
        assert!(!sessions.is_empty(), "the connection registered a session");

        // 2. POST initialize → 202 → the response arrives on the stream.
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}});
        assert_eq!(
            send_message(&app, &session_id, initialize).await,
            StatusCode::ACCEPTED
        );
        let frame = next_json_frame(&mut reader, &mut data).await;
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], json!(1));
        assert_eq!(frame["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            frame["result"]["capabilities"],
            json!({"tools":{"listChanged":true}})
        );
        assert_eq!(
            frame["result"]["serverInfo"],
            json!({"name":"synopsis-sse-test","version":"0.2.0"})
        );

        // 3. POST tools/list → 202 → all 12 tools on the stream.
        let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        assert_eq!(
            send_message(&app, &session_id, list).await,
            StatusCode::ACCEPTED
        );
        let frame = next_json_frame(&mut reader, &mut data).await;
        assert_eq!(frame["id"], json!(2));
        assert_eq!(frame["result"]["tools"].as_array().unwrap().len(), 12);

        // 4. POST tools/call (catalog_overview on the fixture DB) → 202 →
        //    the result's content[0].text is the dispatch payload.
        let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"catalog_overview"}});
        assert_eq!(
            send_message(&app, &session_id, call).await,
            StatusCode::ACCEPTED
        );
        let frame = next_json_frame(&mut reader, &mut data).await;
        assert_eq!(frame["id"], json!(3));
        assert_eq!(frame["result"]["isError"], json!(false));
        let payload: Value = serde_json::from_str(
            frame["result"]["content"][0]["text"]
                .as_str()
                .expect("content[0] is text"),
        )
        .unwrap();
        // The empty fixture DB: zero documents.
        assert_eq!(payload["document_count"], json!(0));

        drop(data);
    }
}
