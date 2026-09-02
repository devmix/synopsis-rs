//! Integration tests for the [`llm::LlmClient`] call core (oracle: Go
//! `internal/llm/client.go`).
//!
//! Relocated from the inline test module in `src/client.rs` (change
//! test-hygiene-phase-1, task 1.9): the 32 tests that use only the public
//! API plus the docs-hidden `llm::test_support` seams. The two tests that
//! read the private `LlmClient.config` field stay inline in `src/client.rs`.
//! `valid_config` and `config_with` are local copies of the inline helpers,
//! which are shared with the stay-inline tests and therefore remain in
//! `src/client.rs` (change header convention for shared helpers).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use config::preset::{LlmConfig, ResponseFormat};
use llm::{LlmClient, LlmError};

/// A valid config pointing at `base_url`; every other field satisfies the
/// constructor invariants.
fn valid_config(base_url: &str) -> LlmConfig {
    LlmConfig {
        api_base_url: base_url.to_string(),
        api_key: "test-key".to_string(),
        model_name: "test-model".to_string(),
        temperature: 0.0,
        max_tokens: 1024,
        seed: 42,
        response_format: ResponseFormat::JsonObject,
        timeout_ms: 5000,
        max_retries: 2,
    }
}

fn config_with(base_url: &str, mutate: impl FnOnce(&mut LlmConfig)) -> LlmConfig {
    let mut config = valid_config(base_url);
    mutate(&mut config);
    config
}

/// Builds a client from a config expected to be invalid and returns the
/// rejection error. (`LlmClient` has no `Debug` impl on purpose — it holds
/// the api_key, which must not leak into debug output, design D7 — so
/// `Result::err` is used instead of `unwrap_err`.)
fn new_err(config: &LlmConfig) -> LlmError {
    LlmClient::new(config)
        .err()
        .expect("config must be rejected")
}

fn assert_configuration(err: LlmError, needle: &str) {
    match err {
        LlmError::Configuration(msg) => {
            assert!(msg.contains(needle), "expected {needle:?} in {msg:?}");
        }
        other => panic!("expected Configuration, got {other:?}"),
    }
}

// ── Constructor tests ────────────────────────────────────────────────────────

#[test]
fn new_accepts_https_and_json_schema_mode() {
    let config = config_with("https://api.example.com/v1", |c| {
        c.response_format = ResponseFormat::JsonSchema;
    });
    let client = LlmClient::new(&config).unwrap();
    assert_eq!(
        client.endpoint(),
        "https://api.example.com/v1/chat/completions"
    );
    assert_eq!(client.response_format(), &ResponseFormat::JsonSchema);
}

#[test]
fn new_normalizes_trailing_slash_in_base_url() {
    let config = config_with("http://127.0.0.1:1/", |_| {});
    let client = LlmClient::new(&config).unwrap();
    assert_eq!(client.endpoint(), "http://127.0.0.1:1/chat/completions");
}

#[test]
fn new_rejects_empty_base_url() {
    for url in ["", "   "] {
        let config = config_with("http://127.0.0.1:1", |c| c.api_base_url = url.to_string());
        assert_configuration(new_err(&config), "api_base_url must not be empty");
    }
}

#[test]
fn new_rejects_base_url_without_scheme() {
    // A schemeless "host:port" does not parse as an absolute URI.
    let config = config_with("http://127.0.0.1:1", |c| {
        c.api_base_url = "127.0.0.1:9999".into()
    });
    assert_configuration(new_err(&config), "is not a valid URL");
}

#[test]
fn new_rejects_non_http_scheme() {
    let config = config_with("http://127.0.0.1:1", |c| {
        c.api_base_url = "ftp://files.example.com".to_string();
    });
    assert_configuration(new_err(&config), "must use the http or https scheme");
}

#[test]
fn new_rejects_garbage_base_url() {
    let config = config_with("http://127.0.0.1:1", |c| {
        c.api_base_url = "not a url".into()
    });
    assert_configuration(new_err(&config), "is not a valid URL");
}

#[test]
fn new_rejects_empty_model_name() {
    for name in ["", "  "] {
        let config = config_with("http://127.0.0.1:1", |c| c.model_name = name.to_string());
        assert_configuration(new_err(&config), "model_name must not be empty");
    }
}

#[test]
fn new_rejects_non_positive_timeout() {
    for timeout_ms in [0, -1] {
        let config = config_with("http://127.0.0.1:1", |c| c.timeout_ms = timeout_ms);
        assert_configuration(new_err(&config), "timeout_ms must be > 0");
    }
}

#[test]
fn new_rejects_negative_max_retries() {
    let config = config_with("http://127.0.0.1:1", |c| c.max_retries = -1);
    assert_configuration(new_err(&config), "max_retries must be >= 0");
}

#[test]
fn new_rejects_non_positive_max_tokens() {
    for max_tokens in [0, -5] {
        let config = config_with("http://127.0.0.1:1", |c| c.max_tokens = max_tokens);
        assert_configuration(new_err(&config), "max_tokens must be > 0");
    }
}

#[test]
fn new_rejects_bad_temperature() {
    for temperature in [-0.1, f64::NAN] {
        let config = config_with("http://127.0.0.1:1", |c| c.temperature = temperature);
        assert_configuration(new_err(&config), "temperature must be a finite value >= 0");
    }
}

#[test]
fn client_is_send_sync_and_cheap_to_clone() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LlmClient>();
    let client = LlmClient::new(&valid_config("http://127.0.0.1:1")).unwrap();
    let clone = client.clone();
    assert_eq!(clone.model(), client.model());
}

// ── Mock server ──────────────────────────────────────────────────────────────

/// Minimal HTTP/1.1 server on 127.0.0.1 with an ephemeral port: reads each
/// POST in full (headers + body), records the raw request, and serves the
/// handler's `(status, body)` with `Connection: close`. Keeps CI
/// network-free (pattern: `crates/embedding`).
struct MockServer {
    url: String,
    requests: Arc<AtomicUsize>,
    captured: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(handler: impl Fn(usize) -> (u16, Vec<u8>) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
        let server_captured = Arc::clone(&captured);
        let server_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            loop {
                if server_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let index = server_requests.fetch_add(1, Ordering::SeqCst);
                        handle_connection(stream, index, &server_captured, &handler);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            requests,
            captured,
            shutdown,
            thread: Some(thread),
        }
    }

    /// The raw request (headers + body) captured at index `i`.
    fn raw_request(&self, i: usize) -> String {
        self.captured.lock().unwrap()[i].clone()
    }

    /// The JSON body of the captured request at index `i`.
    fn request_body_json(&self, i: usize) -> serde_json::Value {
        let body = self
            .raw_request(i)
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        serde_json::from_str(&body).unwrap()
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop polls the shutdown flag, so the join returns
        // promptly.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads a full request (headers + `Content-Length` body bytes).
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        if let Some(total) = expected_request_len(&received)
            && received.len() >= total
        {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Err(_) => break,
            Ok(n) => received.extend_from_slice(&buffer[..n]),
        }
    }
    received
}

/// Total expected request length (headers + body) once the header block is
/// complete; `None` while more header bytes are still needed.
fn expected_request_len(received: &[u8]) -> Option<usize> {
    let header_end = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)?;
    if received.len() < header_end {
        return None;
    }
    // No Content-Length means no body (a `Content-Length: 0` is the same).
    let content_length = parse_content_length(&received[..header_end]).unwrap_or(0);
    Some(header_end + content_length)
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.lines() {
        let mut parts = line.splitn(2, ':');
        let name = parts.next()?.trim().to_ascii_lowercase();
        if name == "content-length" {
            return parts.next()?.trim().parse::<usize>().ok();
        }
    }
    None
}

fn handle_connection(
    mut stream: TcpStream,
    index: usize,
    captured: &Arc<Mutex<Vec<String>>>,
    handler: &impl Fn(usize) -> (u16, Vec<u8>),
) {
    let _ = stream.set_nonblocking(false);
    let raw = read_request(&mut stream);
    if let Ok(text) = std::str::from_utf8(&raw) {
        captured.lock().unwrap().push(text.to_string());
    }
    let (status, body) = handler(index);
    let reason = if (200..=299).contains(&status) {
        "OK"
    } else {
        "Error"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    // Let the client drain the response before the socket is closed.
    std::thread::sleep(Duration::from_millis(25));
}

fn success_body(content: &str) -> Vec<u8> {
    format!(
        "{{\"choices\":[{{\"message\":{{\"content\":\"{content}\"}},\"finish_reason\":\"stop\"}}]}}"
    )
    .into_bytes()
}

// ── Call tests ───────────────────────────────────────────────────────────────

#[test]
fn call_returns_trimmed_content_on_success() {
    let server = MockServer::start(move |_| (200, success_body("  hello world  ")));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    let content = client
        .call("system prompt", "user prompt", None, None)
        .unwrap();

    assert_eq!(content, "hello world");
    assert_eq!(server.request_count(), 1);
}

#[test]
fn call_request_body_matches_golden_json() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    client
        .call("system prompt", "user prompt", None, None)
        .unwrap();

    let expected = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "system", "content": "system prompt"},
            {"role": "user", "content": "user prompt"},
        ],
        "temperature": 0.0,
        "seed": 42,
        "max_tokens": 1024,
        "response_format": {"type": "json_object"},
    });
    assert_eq!(server.request_body_json(0), expected);
}

#[test]
fn call_sends_bearer_when_api_key_set() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    client.call("s", "u", None, None).unwrap();

    let raw = server.raw_request(0).to_ascii_lowercase();
    assert!(
        raw.contains("authorization: bearer test-key"),
        "expected Bearer header in:\n{raw}"
    );
}

#[test]
fn call_omits_authorization_when_api_key_empty() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let config = config_with(&server.url, |c| c.api_key.clear());
    let client = LlmClient::new(&config).unwrap();

    client.call("s", "u", None, None).unwrap();

    let raw = server.raw_request(0);
    assert!(
        !raw.to_ascii_lowercase().contains("authorization"),
        "expected no Authorization header in:\n{raw}"
    );
}

#[test]
fn call_empty_content_is_non_retryable_error() {
    let server = MockServer::start(move |_| {
        (
            200,
            br#"{"choices":[{"message":{"content":"","reasoning_content":"thinking"},"finish_reason":"length"}]}"#
                .to_vec(),
        )
    });
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    let err = client
        .call("s", "u", None, None)
        .expect_err("empty content must be an error");

    match &err {
        LlmError::EmptyContent {
            finish_reason,
            reasoning_content_len,
        } => {
            assert_eq!(finish_reason.as_str(), "length", "got: {err}");
            assert_eq!(*reasoning_content_len, 8, "got: {err}");
        }
        other => panic!("expected EmptyContent, got: {other:?}"),
    }
    assert!(!err.is_retryable(), "empty content must not be retryable");
    assert_eq!(server.request_count(), 1, "no retry for empty content");
}

/// Asserts `err` is [`LlmError::RetriesExhausted`] with exactly
/// `attempts` attempts and runs `check` on the carried last cause.
fn assert_exhausted(err: &LlmError, attempts: u32, check: impl Fn(&LlmError)) {
    match err {
        LlmError::RetriesExhausted {
            attempts: got,
            last,
        } => {
            assert_eq!(*got, attempts, "got: {err}");
            check(last);
        }
        other => panic!("expected RetriesExhausted, got: {other:?}"),
    }
}

/// Asserts a recorded backoff delay for retry `retry` (1-based) lies
/// within the documented ±20% jitter band of `500 ms · 2^(retry-1)`.
fn assert_delay_within(delay: &Duration, retry: u32) {
    let base_ms = 500u64 * 2u64.pow(retry - 1);
    let got_ms = delay.as_millis() as u64;
    let (lo, hi) = (base_ms * 8 / 10, base_ms * 12 / 10);
    assert!(
        got_ms >= lo && got_ms < hi,
        "retry {retry}: expected [{lo}, {hi}) ms, got {got_ms} ms"
    );
}

#[test]
fn call_429_is_retryable_and_exhausts_with_cause() {
    let server = MockServer::start(move |_| (429, b"rate limited".to_vec()));
    let config = config_with(&server.url, |c| c.max_retries = 0);
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    let client = llm::test_support::with_sleeper(LlmClient::new(&config).unwrap(), move |d| {
        recorded.lock().unwrap().push(d)
    });

    let err = client.call("s", "u", None, None).err().unwrap();

    assert_exhausted(&err, 1, |last| {
        assert!(
            matches!(last, LlmError::RetryableHttp { status: 429, .. }),
            "got: {last:?}"
        );
        assert!(last.is_retryable(), "the cause must stay retryable");
    });
    assert!(!err.is_retryable(), "exhaustion is terminal");
    assert_eq!(
        server.request_count(),
        1,
        "max_retries = 0: a single attempt"
    );
    assert!(
        delays.lock().unwrap().is_empty(),
        "no backoff sleep without retries"
    );
}

#[test]
fn call_5xx_is_retryable_and_exhausts_with_cause() {
    let server = MockServer::start(move |_| (500, b"boom".to_vec()));
    let config = config_with(&server.url, |c| c.max_retries = 0);
    let client = LlmClient::new(&config).unwrap();

    let err = client.call("s", "u", None, None).err().unwrap();

    assert_exhausted(&err, 1, |last| {
        assert!(
            matches!(last, LlmError::RetryableHttp { status: 500, .. }),
            "got: {last:?}"
        );
        assert!(
            last.to_string().contains("boom"),
            "body must be carried: {last}"
        );
    });
    assert!(!err.is_retryable());
    assert_eq!(
        server.request_count(),
        1,
        "max_retries = 0: a single attempt"
    );
}

#[test]
fn call_other_4xx_is_non_retryable() {
    let server = MockServer::start(move |_| (400, b"bad request".to_vec()));
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    let client = llm::test_support::with_sleeper(
        LlmClient::new(&valid_config(&server.url)).unwrap(),
        move |d| recorded.lock().unwrap().push(d),
    );

    let err = client.call("s", "u", None, None).err().unwrap();

    assert!(
        matches!(err, LlmError::HttpStatus { status: 400, .. }),
        "got: {err}"
    );
    assert!(!err.is_retryable());
    assert_eq!(server.request_count(), 1, "no retry for non-retryable 4xx");
    assert!(
        delays.lock().unwrap().is_empty(),
        "no backoff sleep for 4xx"
    );
}

#[test]
fn call_malformed_response_is_parse_error() {
    let server = MockServer::start(move |_| (200, b"not json".to_vec()));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    let err = client.call("s", "u", None, None).err().unwrap();

    assert!(matches!(err, LlmError::Parse(_)), "got: {err}");
    assert!(!err.is_retryable());
    assert_eq!(server.request_count(), 1, "no retry for parse errors");
}

#[test]
fn call_no_choices_is_parse_error() {
    let server = MockServer::start(move |_| (200, b"{}".to_vec()));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    let err = client.call("s", "u", None, None).err().unwrap();

    assert!(matches!(err, LlmError::Parse(_)), "got: {err}");
    assert!(err.to_string().contains("no choices"), "got: {err}");
}

#[test]
fn call_json_schema_mode_embeds_schema_and_name() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let config = config_with(&server.url, |c| {
        c.response_format = ResponseFormat::JsonSchema
    });
    let client = LlmClient::new(&config).unwrap();

    client
        .call(
            "s",
            "u",
            Some(r#"{"type":"object","properties":{"same_entity":{"type":"boolean"}}}"#),
            Some("link_decision"),
        )
        .unwrap();

    let body = server.request_body_json(0);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(
        body["response_format"]["json_schema"]["name"],
        "link_decision"
    );
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["type"],
        "object"
    );
}

#[test]
fn call_json_schema_mode_defaults_name_to_llm_output() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let config = config_with(&server.url, |c| {
        c.response_format = ResponseFormat::JsonSchema
    });
    let client = LlmClient::new(&config).unwrap();

    client
        .call("s", "u", Some(r#"{"type":"object"}"#), None)
        .unwrap();

    let body = server.request_body_json(0);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "llm_output");
}

#[test]
fn call_json_schema_mode_without_schema_falls_back_to_json_object() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let config = config_with(&server.url, |c| {
        c.response_format = ResponseFormat::JsonSchema
    });
    let client = LlmClient::new(&config).unwrap();

    client.call("s", "u", None, None).unwrap();

    let body = server.request_body_json(0);
    assert_eq!(body["response_format"]["type"], "json_object");
    assert!(body["response_format"].get("json_schema").is_none());
}

#[test]
fn call_rejects_empty_prompts_before_any_request() {
    let server = MockServer::start(move |_| (200, success_body("ok")));
    let client = LlmClient::new(&valid_config(&server.url)).unwrap();

    let sys_err = client.call("", "u", None, None).err().unwrap();
    assert_configuration(sys_err, "system prompt");

    let user_err = client.call("s", "   ", None, None).err().unwrap();
    assert_configuration(user_err, "user prompt");

    assert_eq!(server.request_count(), 0, "no request for empty prompts");
}

// ── Retry policy tests (task 1.3) ───────────────────────────────────────────

#[test]
fn call_retries_429_then_succeeds() {
    let server = MockServer::start(|i| {
        if i == 0 {
            (429, b"rate limited".to_vec())
        } else {
            (200, success_body("recovered"))
        }
    });
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    let client = llm::test_support::with_sleeper(
        LlmClient::new(&valid_config(&server.url)).unwrap(),
        move |d| recorded.lock().unwrap().push(d),
    );

    let content = client.call("s", "u", None, None).unwrap();

    assert_eq!(content, "recovered");
    assert_eq!(server.request_count(), 2, "one retry after the 429");
    let delays = delays.lock().unwrap();
    assert_eq!(delays.len(), 1, "exactly one backoff sleep");
    assert_delay_within(&delays[0], 1);
}

#[test]
fn call_5xx_exhausts_retries_with_last_cause() {
    let server = MockServer::start(move |_| (500, b"boom".to_vec()));
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    // max_retries = 2
    let client = llm::test_support::with_sleeper(
        LlmClient::new(&valid_config(&server.url)).unwrap(),
        move |d| recorded.lock().unwrap().push(d),
    );

    let err = client.call("s", "u", None, None).err().unwrap();

    assert_exhausted(&err, 3, |last| {
        assert!(
            matches!(last, LlmError::RetryableHttp { status: 500, body } if body == "boom"),
            "got: {last:?}"
        );
    });
    assert!(!err.is_retryable(), "exhaustion is terminal");
    assert_eq!(server.request_count(), 3, "initial attempt + 2 retries");
    assert_eq!(
        delays.lock().unwrap().len(),
        2,
        "a backoff sleep before each retry"
    );
}

#[test]
fn call_backoff_delays_grow_exponentially() {
    let server = MockServer::start(move |_| (500, b"boom".to_vec()));
    let config = config_with(&server.url, |c| c.max_retries = 3);
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    let client = llm::test_support::with_sleeper(LlmClient::new(&config).unwrap(), move |d| {
        recorded.lock().unwrap().push(d)
    });

    let err = client.call("s", "u", None, None).err().unwrap();
    assert_exhausted(&err, 4, |_| {});

    let delays = delays.lock().unwrap();
    assert_eq!(
        delays.len(),
        3,
        "a backoff sleep before each of the 3 retries"
    );
    for (i, delay) in delays.iter().enumerate() {
        assert_delay_within(delay, i as u32 + 1);
    }
    // The ±20% bands of consecutive retries are disjoint (600 < 800,
    // 1200 < 1600), so in-band delays are strictly increasing.
    assert!(delays[0] < delays[1], "delays must grow: {delays:?}");
    assert!(delays[1] < delays[2], "delays must grow: {delays:?}");
}

#[test]
fn call_json_schema_present_in_every_retried_request() {
    let server = MockServer::start(|i| {
        if i == 0 {
            (429, b"rate limited".to_vec())
        } else {
            (200, success_body("ok"))
        }
    });
    let config = config_with(&server.url, |c| {
        c.response_format = ResponseFormat::JsonSchema
    });
    let client = llm::test_support::with_sleeper(LlmClient::new(&config).unwrap(), |_d| {});

    client
        .call(
            "s",
            "u",
            Some(r#"{"type":"object","properties":{"same_entity":{"type":"boolean"}}}"#),
            Some("link_decision"),
        )
        .unwrap();

    assert_eq!(server.request_count(), 2, "one retry after the 429");
    for i in [0, 1] {
        let body = server.request_body_json(i);
        assert_eq!(
            body["response_format"]["type"], "json_schema",
            "request {i} must keep the json_schema mode"
        );
        assert_eq!(
            body["response_format"]["json_schema"]["name"], "link_decision",
            "request {i}"
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["type"], "object",
            "request {i}"
        );
    }
}

#[test]
fn call_transport_error_is_retried_until_exhausted() {
    // A closed loopback port fails the connect: a retryable transport
    // error (no mock server involved).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = config_with(&format!("http://127.0.0.1:{port}"), |c| c.max_retries = 1);
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&delays);
    let client = llm::test_support::with_sleeper(LlmClient::new(&config).unwrap(), move |d| {
        recorded.lock().unwrap().push(d)
    });

    let err = client.call("s", "u", None, None).err().unwrap();

    assert_exhausted(&err, 2, |last| {
        assert!(matches!(last, LlmError::Transport(_)), "got: {last:?}");
    });
    let delays = delays.lock().unwrap();
    assert_eq!(delays.len(), 1);
    assert_delay_within(&delays[0], 1);
}

#[test]
fn backoff_delay_stays_within_jitter_band_and_varies() {
    let client = LlmClient::new(&valid_config("http://127.0.0.1:1")).unwrap();
    let mut distinct = std::collections::HashSet::new();
    for retry in 1u32..=5 {
        let base_ms = 500u64 * 2u64.pow(retry - 1);
        for _ in 0..100 {
            let got_ms = llm::test_support::backoff_delay(&client, retry).as_millis() as u64;
            assert!(
                got_ms >= base_ms * 8 / 10 && got_ms < base_ms * 12 / 10,
                "retry {retry}: {got_ms} ms outside [{}, {})",
                base_ms * 8 / 10,
                base_ms * 12 / 10
            );
            distinct.insert(got_ms);
        }
    }
    // A working jitter stream produces many distinct delays over 400
    // draws (a constant draw would collapse each band to one value).
    assert!(distinct.len() > 100, "jitter never varied: {distinct:?}");
}
