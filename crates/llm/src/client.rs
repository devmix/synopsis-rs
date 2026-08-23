//! The call core: build the chat-completions request, send it with Bearer
//! auth, and parse the response into the model's text content.
//!
//! [`LlmClient`] is the public seam (design D1). It is built once from a
//! validated [`LlmConfig`](config::preset::LlmConfig) (see [`LlmClient::new`])
//! and shared across threads; [`LlmClient::call`] performs a single blocking
//! `POST {api_base_url}/chat/completions` and returns the model's content
//! (design D2).
//!
//! Deliberate deviations from the oracle (`../synopsis/internal/llm/client.go`),
//! recorded per the migration principles:
//! - message `content` is a plain string, not the oracle's `[{type, text}]`
//!   parts array — for text-only prompts the wire form is equivalent, and a
//!   string is simpler and cheaper to serialize;
//! - `temperature` / `seed` / `max_tokens` are always serialized (the oracle
//!   omits zero values via `omitempty`); explicit values are deterministic and
//!   every OpenAI-compatible API accepts them;
//! - silent config defaults are replaced by fail-fast validation in
//!   [`LlmClient::new`] (a zero timeout / retry count / token budget is a
//!   configuration bug, not a value to paper over).
//!
//! The retry policy (429/5xx + transport, exponential backoff with injected
//! delays) lands in task 1.3; this module already classifies every error via
//! [`LlmError::is_retryable`] so that policy has a single decision point.

use config::preset::{LlmConfig, ResponseFormat};
use serde::{Deserialize, Serialize};
use ureq::{Agent, http::Uri};

use crate::error::LlmError;

/// Synchronous client for an OpenAI-compatible chat completions endpoint.
///
/// Built once from a validated [`LlmConfig`](config::preset::LlmConfig) and
/// shared across threads (`Send + Sync`; the ureq connection pool lives behind
/// an `Arc`, so cloning is cheap). Methods block — call from sync contexts or
/// `spawn_blocking` workers, never from inside an async task (design D2).
#[derive(Clone)]
pub struct LlmClient {
    /// Shared connection pool: global timeout, identity, and status handling
    /// are configured once in [`LlmClient::new`].
    agent: Agent,
    /// `{api_base_url}/chat/completions`, normalized (no trailing slash in the
    /// base).
    endpoint: String,
    /// The validated configuration: single source of all request parameters
    /// (model, sampling, budget, auth).
    config: LlmConfig,
}

impl LlmClient {
    /// Builds a client from `config`, validating every field invariant:
    ///
    /// - `api_base_url` is non-empty, uses the `http`/`https` scheme with a
    ///   host, and `{base}/chat/completions` parses as an absolute URI;
    /// - `model_name` is non-empty;
    /// - `timeout_ms > 0` (per-request global timeout);
    /// - `max_retries >= 0` (0 = no retries after the initial attempt);
    /// - `max_tokens > 0`;
    /// - `temperature` is a finite value >= 0.
    ///
    /// # Errors
    ///
    /// [`LlmError::Configuration`] naming the first violated invariant.
    ///
    /// Deliberate deviation from the oracle: the Go client silently
    /// substitutes defaults (60 s timeout, 3 retries, 2048 max tokens,
    /// negative temperature clamped to 0); here a missing or zero value is a
    /// configuration bug and fails fast instead.
    pub fn new(config: &LlmConfig) -> Result<Self, LlmError> {
        let base_url = config.api_base_url.trim();
        if base_url.is_empty() {
            return Err(LlmError::Configuration(
                "api_base_url must not be empty".to_string(),
            ));
        }
        // Normalize away a trailing slash so "http://x" and "http://x/" yield
        // the same endpoint. The oracle concatenates blindly, producing
        // "http://x//chat/completions".
        let base_url = base_url.trim_end_matches('/');
        let endpoint = format!("{base_url}/chat/completions");
        let uri: Uri = endpoint.parse().map_err(|err| {
            LlmError::Configuration(format!(
                "api_base_url {base_url:?} is not a valid URL: {err}"
            ))
        })?;
        if uri.scheme_str() != Some("http") && uri.scheme_str() != Some("https") {
            return Err(LlmError::Configuration(format!(
                "api_base_url must use the http or https scheme, got {base_url:?}"
            )));
        }
        if uri.host().is_none() {
            return Err(LlmError::Configuration(format!(
                "api_base_url {base_url:?} has no host"
            )));
        }

        if config.model_name.trim().is_empty() {
            return Err(LlmError::Configuration(
                "model_name must not be empty".to_string(),
            ));
        }
        if config.timeout_ms <= 0 {
            return Err(LlmError::Configuration(format!(
                "timeout_ms must be > 0, got {}",
                config.timeout_ms
            )));
        }
        if config.max_retries < 0 {
            return Err(LlmError::Configuration(format!(
                "max_retries must be >= 0, got {}",
                config.max_retries
            )));
        }
        if config.max_tokens <= 0 {
            return Err(LlmError::Configuration(format!(
                "max_tokens must be > 0, got {}",
                config.max_tokens
            )));
        }
        if config.temperature.is_nan() || config.temperature < 0.0 {
            return Err(LlmError::Configuration(format!(
                "temperature must be a finite value >= 0, got {}",
                config.temperature
            )));
        }

        let timeout = std::time::Duration::from_millis(config.timeout_ms as u64);
        // Design D2: status codes are classified by the client (task 1.2),
        // not by ureq, so error messages can carry the response body — the
        // oracle includes the body in its HTTP error messages.
        let agent_config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent("synopsis/0.1.0")
            .build();

        Ok(Self {
            agent: Agent::new_with_config(agent_config),
            endpoint,
            config: config.clone(),
        })
    }

    /// Performs one chat completion and returns the model's text content.
    ///
    /// Builds the request body (model, `system` + `user` messages, sampling,
    /// and the structured-output mode), `POST`s it to
    /// `{api_base_url}/chat/completions` with an `Authorization: Bearer`
    /// header only when `api_key` is non-empty, and parses
    /// `choices[0].message.content` (trimmed) out of the `200` response.
    ///
    /// `schema` and `schema_name` are used only in
    /// [`ResponseFormat::JsonSchema`] mode: a non-empty `schema` is parsed as
    /// JSON and embedded under `response_format.json_schema.{name, schema}`,
    /// defaulting the name to `llm_output` when `schema_name` is empty; an
    /// empty or absent `schema` falls back to `json_object` (oracle
    /// parity). In [`ResponseFormat::JsonObject`] mode both arguments are
    /// ignored.
    ///
    /// The per-request timeout is the config `timeout_ms`, applied globally to
    /// the connection pool in [`LlmClient::new`].
    ///
    /// # Errors
    ///
    /// - [`LlmError::Configuration`] when a prompt is empty;
    /// - [`LlmError::RetryableHttp`] for `429` / `5xx`;
    /// - [`LlmError::HttpStatus`] for any other non-`200` status;
    /// - [`LlmError::Transport`] for connection / DNS / timeout failures;
    /// - [`LlmError::EmptyContent`] for a `200` whose content is empty or
    ///   whitespace-only — non-retryable by design;
    /// - [`LlmError::Parse`] for a `200` whose body is not a chat completion
    ///   (or a malformed JSON schema in `json_schema` mode).
    pub fn call(
        &self,
        system: &str,
        user: &str,
        schema: Option<&str>,
        schema_name: Option<&str>,
    ) -> Result<String, LlmError> {
        if system.trim().is_empty() {
            return Err(LlmError::Configuration(
                "system prompt must not be empty".to_string(),
            ));
        }
        if user.trim().is_empty() {
            return Err(LlmError::Configuration(
                "user prompt must not be empty".to_string(),
            ));
        }

        let body = self.build_request_body(system, user, schema, schema_name)?;
        let payload = serde_json::to_string(&body)
            .map_err(|err| LlmError::Parse(format!("failed to serialize request: {err}")))?;

        let mut request = self.agent.post(&self.endpoint);
        if !self.config.api_key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.config.api_key));
        }
        let mut response = request
            .content_type("application/json")
            .send(&payload)
            .map_err(|err| LlmError::from_transport(err, &self.endpoint))?;

        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|err| LlmError::from_transport(err, &self.endpoint))?;

        Self::classify_and_parse(status.as_u16(), text)
    }

    /// The model identifier this client calls (part of the linker cache key,
    /// design D4).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.config.model_name
    }

    /// The full chat completions endpoint this client posts to
    /// (`{api_base_url}/chat/completions`, trailing slash normalized away).
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The structured-output mode. Callers in `JsonSchema` mode must supply a
    /// schema per call (the oracle's `IsRequiresSchema`).
    #[must_use]
    pub fn response_format(&self) -> &ResponseFormat {
        &self.config.response_format
    }

    /// Builds the chat-completions request body.
    fn build_request_body(
        &self,
        system: &str,
        user: &str,
        schema: Option<&str>,
        schema_name: Option<&str>,
    ) -> Result<RequestBody, LlmError> {
        Ok(RequestBody {
            model: self.config.model_name.clone(),
            messages: vec![
                RequestMessage {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                RequestMessage {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            temperature: self.config.temperature,
            seed: self.config.seed,
            max_tokens: self.config.max_tokens,
            response_format: self.build_response_format(schema, schema_name)?,
        })
    }

    /// Builds the `response_format` field (`json_object` or `json_schema`).
    fn build_response_format(
        &self,
        schema: Option<&str>,
        schema_name: Option<&str>,
    ) -> Result<ResponseFormatBody, LlmError> {
        if self.config.response_format == ResponseFormat::JsonSchema
            && let Some(schema) = schema
            && !schema.trim().is_empty()
        {
            let parsed: serde_json::Value = serde_json::from_str(schema)
                .map_err(|err| LlmError::Parse(format!("invalid JSON schema: {err}")))?;
            let name = schema_name
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| "llm_output".to_string());
            return Ok(ResponseFormatBody {
                kind: "json_schema".to_string(),
                json_schema: Some(JsonSchemaConfig {
                    name,
                    schema: parsed,
                }),
            });
        }
        // `json_object` mode, or `json_schema` mode without a usable schema
        // (the oracle falls through to `json_object` in that case).
        Ok(ResponseFormatBody {
            kind: "json_object".to_string(),
            json_schema: None,
        })
    }

    /// Maps an HTTP status plus body to a result or a classified error.
    fn classify_and_parse(status: u16, body: String) -> Result<String, LlmError> {
        match status {
            // 429 (rate limited) and all 5xx are retryable (task 1.3).
            429 | 500..=599 => Err(LlmError::RetryableHttp { status, body }),
            200 => parse_chat_completion(&body),
            // Every other status (other 4xx, 3xx, 1xx) is non-retryable.
            _ => Err(LlmError::HttpStatus { status, body }),
        }
    }
}

/// Parses a `200` response body into the model's text content.
fn parse_chat_completion(body: &str) -> Result<String, LlmError> {
    let response: ChatResponse = serde_json::from_str(body)
        .map_err(|err| LlmError::Parse(format!("invalid chat completion JSON: {err}")))?;
    let Some(choice) = response.choices.first() else {
        return Err(LlmError::Parse("no choices in response".to_string()));
    };
    let content = choice.message.content.trim();
    if content.is_empty() {
        // Intentionally non-retryable: retrying produces the same result.
        let finish_reason = choice
            .finish_reason
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "(none)".to_string());
        let reasoning_content_len = choice
            .message
            .reasoning_content
            .as_ref()
            .map_or(0, std::string::String::len);
        return Err(LlmError::EmptyContent {
            finish_reason,
            reasoning_content_len,
        });
    }
    Ok(content.to_string())
}

// ── Request types ────────────────────────────────────────────────────────────

/// The chat-completions request body (wire shape; see the module docs for the
/// deliberate deviations from the oracle).
#[derive(Debug, Serialize)]
struct RequestBody {
    /// The model identifier.
    model: String,
    /// The conversation: `system` then `user`.
    messages: Vec<RequestMessage>,
    /// Sampling temperature (always sent; the oracle omits zero).
    temperature: f64,
    /// Sampling seed (always sent; the oracle omits zero).
    seed: i64,
    /// Maximum tokens (always sent; the oracle omits zero).
    max_tokens: i32,
    /// Structured-output mode.
    response_format: ResponseFormatBody,
}

/// One message in the conversation.
#[derive(Debug, Serialize)]
struct RequestMessage {
    /// The role: `system` or `user`.
    role: String,
    /// The message text (a plain string, not the oracle's parts array).
    content: String,
}

/// The `response_format` field.
#[derive(Debug, Serialize)]
struct ResponseFormatBody {
    /// `json_object` or `json_schema`.
    #[serde(rename = "type")]
    kind: String,
    /// Present only for `json_schema`.
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<JsonSchemaConfig>,
}

/// The `json_schema` payload.
#[derive(Debug, Serialize)]
struct JsonSchemaConfig {
    /// The schema name (defaults to `llm_output`).
    name: String,
    /// The parsed JSON schema (embedded verbatim).
    schema: serde_json::Value,
}

// ── Response types ───────────────────────────────────────────────────────────

/// A chat-completions response (only the fields the client reads).
#[derive(Deserialize)]
struct ChatResponse {
    /// The candidate completions; the first is used.
    #[serde(default)]
    choices: Vec<Choice>,
}

/// One candidate completion.
#[derive(Deserialize)]
struct Choice {
    /// The model's message.
    #[serde(default)]
    message: Message,
    /// Why the model stopped (diagnostic for empty content).
    #[serde(default)]
    finish_reason: Option<String>,
}

/// The model's message within a choice.
#[derive(Debug, Default, Deserialize)]
struct Message {
    /// The visible content (trimmed before use).
    #[serde(default)]
    content: String,
    /// A thinking model's reasoning (diagnostic for empty content).
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are valid).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

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

    // ── Constructor tests (relocated from lib.rs with the struct) ──────────

    #[test]
    fn new_accepts_valid_config() {
        let client = LlmClient::new(&valid_config("http://127.0.0.1:9999")).unwrap();
        assert_eq!(client.model(), "test-model");
        assert_eq!(client.response_format(), &ResponseFormat::JsonObject);
        assert_eq!(client.endpoint(), "http://127.0.0.1:9999/chat/completions");
        // The validated configuration is stored verbatim (task 1.2 reads it).
        assert_eq!(client.config.api_key, "test-key");
        assert_eq!(client.config.temperature, 0.0);
        assert_eq!(client.config.max_tokens, 1024);
        assert_eq!(client.config.max_retries, 2);
    }

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
    fn new_accepts_zero_max_retries() {
        let config = config_with("http://127.0.0.1:1", |c| c.max_retries = 0);
        let client = LlmClient::new(&config).unwrap();
        assert_eq!(client.config.max_retries, 0);
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

    // ── Mock server ──────────────────────────────────────────────────────────

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

    // ── Call tests ───────────────────────────────────────────────────────────

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

    #[test]
    fn call_429_is_retryable() {
        let server = MockServer::start(move |_| (429, b"rate limited".to_vec()));
        let client = LlmClient::new(&valid_config(&server.url)).unwrap();

        let err = client.call("s", "u", None, None).err().unwrap();

        assert!(
            matches!(err, LlmError::RetryableHttp { status: 429, .. }),
            "got: {err}"
        );
        assert!(err.is_retryable());
    }

    #[test]
    fn call_5xx_is_retryable() {
        let server = MockServer::start(move |_| (500, b"boom".to_vec()));
        let client = LlmClient::new(&valid_config(&server.url)).unwrap();

        let err = client.call("s", "u", None, None).err().unwrap();

        assert!(
            matches!(err, LlmError::RetryableHttp { status: 500, .. }),
            "got: {err}"
        );
        assert!(err.is_retryable());
        assert!(
            err.to_string().contains("boom"),
            "body must be carried: {err}"
        );
    }

    #[test]
    fn call_other_4xx_is_non_retryable() {
        let server = MockServer::start(move |_| (400, b"bad request".to_vec()));
        let client = LlmClient::new(&valid_config(&server.url)).unwrap();

        let err = client.call("s", "u", None, None).err().unwrap();

        assert!(
            matches!(err, LlmError::HttpStatus { status: 400, .. }),
            "got: {err}"
        );
        assert!(!err.is_retryable());
    }

    #[test]
    fn call_malformed_response_is_parse_error() {
        let server = MockServer::start(move |_| (200, b"not json".to_vec()));
        let client = LlmClient::new(&valid_config(&server.url)).unwrap();

        let err = client.call("s", "u", None, None).err().unwrap();

        assert!(matches!(err, LlmError::Parse(_)), "got: {err}");
        assert!(!err.is_retryable());
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
}
