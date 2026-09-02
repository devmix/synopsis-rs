//! Unit tests for the legacy HTTP+SSE transport (extracted from
//! `src/transport/sse.rs` by test-hygiene-phase-1 task 1.10). The 18 tests
//! previously lived in the inline `#[cfg(test)] mod tests` module; they now
//! run against the public API (`mcp::transport::sse`) plus the docs-hidden
//! `mcp::test_support` seams (`endpoint_url`, `encode_sse_frame`,
//! `CHANNEL_CAPACITY`). The 5 `POST /message` tests that need the
//! `#[cfg(test)]` `test_server` fixture stay inline in `src/transport/sse.rs`.
//!
//! Oracle mapping: mcp-go v0.57.0 `server/sse.go` — the wire contract the
//! endpoint URL and the SSE frame bytes are pinned against.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Request, StatusCode, header};
use axum::routing::get;
use sse_stream::Sse;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tower::ServiceExt;
use uuid::Uuid;

use mcp::test_support::{CHANNEL_CAPACITY, encode_sse_frame, endpoint_url};
use mcp::transport::sse::{SseSessionMap, handle_sse};

/// The non-standard proxy headers `endpoint_url` reads (design D2 revision):
/// the production consts are private to `transport::sse`, so the moved tests
/// define the same values locally.
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

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

// --- idle reaper (design D9, task 1.5) ---
//
// `#[tokio::test(start_paused = true)]` + short thresholds/ticks keep
// every test deterministic: the paused clock auto-advances to each
// reaper tick as the test `sleep()`s (no real waiting). Each test
// sleeps PAST the reaping pass under test, so that pass is guaranteed
// complete before the assertion (a pass at the same instant the test
// wakes is not yet guaranteed to have run).

/// A session that is never touched is reaped once its `last_activity` is
/// strictly older than the threshold: the map empties, and sending to the
/// removed session fails (its sender is gone).
///
/// Paused-clock determinism: with `start_paused`, the test's own
/// `sleep()` auto-advances the virtual clock to each reaper tick (no real
/// waiting). The test sleeps PAST the reaping pass (t=3 s for a 2 s
/// threshold / 1 s tick: passes at t=1 and t=2 keep the session — not
/// strictly older — the t=3 pass reaps it) so that pass is guaranteed
/// complete before the assertion.
#[tokio::test(start_paused = true)]
async fn reaper_reaps_idle_session_after_threshold() {
    let map =
        SseSessionMap::with_idle_timeout(Duration::from_secs(2)).with_tick(Duration::from_secs(1));
    let (id, rx) = map.create();
    drop(rx); // the client side is gone; only the map's sender remains
    map.clone().spawn_reaper();
    assert_eq!(map.len(), 1);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(map.is_empty(), "the idle session must be reaped");
    // The session is gone: a send to its id fails.
    assert!(map.send(&id, "payload".to_owned()).await.is_err());
}

/// A session touched within the threshold at every reaper tick survives —
/// even well past the threshold in absolute terms.
#[tokio::test(start_paused = true)]
async fn reaper_keeps_touched_session() {
    let map =
        SseSessionMap::with_idle_timeout(Duration::from_secs(10)).with_tick(Duration::from_secs(1));
    let (id, _rx) = map.create();
    map.clone().spawn_reaper();
    // Five 2 s cycles: 10 s elapse, but the idle gap never exceeds 2 s
    // (the reaper passes on every 1 s tick in between).
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(map.touch(&id), "the touched session must still exist");
    }
    assert_eq!(
        map.len(),
        1,
        "a session touched within the threshold survives"
    );
}

/// A freshly created session is not reaped before its own threshold
/// elapses (the reaper measures per-session age, not process uptime).
/// The session is created at t=5 s and lives to t=14 s (age 9 s < 10 s
/// threshold); the reaper's cutoff is live for the passes at t=10..14, so
/// the keep is a real comparison, not a missing-cutoff short-circuit.
#[tokio::test(start_paused = true)]
async fn reaper_keeps_fresh_session() {
    let map =
        SseSessionMap::with_idle_timeout(Duration::from_secs(10)).with_tick(Duration::from_secs(1));
    map.clone().spawn_reaper();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (id, _rx) = map.create(); // created mid-run
    tokio::time::sleep(Duration::from_secs(9)).await; // age 9 s < 10 s
    assert_eq!(map.len(), 1, "a fresh session must survive");
    assert!(map.get(&id).is_some());
}

/// Spawning a second reaper is harmless: both passes run the same
/// idempotent removal (removing an absent id is a no-op), so a double
/// spawn neither double-removes nor panics.
#[tokio::test(start_paused = true)]
async fn spawn_reaper_twice_is_harmless() {
    let map =
        SseSessionMap::with_idle_timeout(Duration::from_secs(2)).with_tick(Duration::from_secs(1));
    let _first = map.clone().spawn_reaper();
    let _second = map.clone().spawn_reaper();
    let (id, _rx) = map.create();
    // Past the t=3 s reaping pass (2 s threshold / 1 s tick).
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(map.is_empty(), "one removal is enough; both reapers agree");
    assert!(!map.remove(&id), "the session was removed exactly once");
}

/// In-process axum (task 1.5 acceptance): with a short idle threshold, a
/// `GET /sse` stream with no activity ends — the client observes EOF —
/// once the reaper removes the idle session (the sender drop ends the
/// stream; the disconnect guard fires — one removal code path, design
/// D6/D9).
#[tokio::test(start_paused = true)]
async fn idle_sse_stream_ends_after_threshold() {
    let sessions =
        SseSessionMap::with_idle_timeout(Duration::from_secs(5)).with_tick(Duration::from_secs(1));
    sessions.clone().spawn_reaper();
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
    let mut data = response.into_body().into_data_stream();
    let first = data
        .next()
        .await
        .expect("the endpoint frame arrives")
        .expect("no frame error");
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(first.starts_with("event: endpoint\n"), "got: {first}");
    assert_eq!(sessions.len(), 1, "the connection registered a session");

    // No POSTs: the session idles past the 5 s threshold — the first
    // reaping pass lands at t=6 s; sleeping to t=7 s guarantees it is
    // complete.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(sessions.is_empty(), "the idle session must be reaped");

    // The reaper dropped the session's sender: the stream ends (EOF).
    assert!(
        data.next().await.is_none(),
        "the reaped session's stream must end"
    );
}
