//! The call core: build the chat-completions request, send it with Bearer
//! auth, retry transient failures with exponential backoff, and parse the
//! response into the model's text content.
//!
//! [`LlmClient`] is the public seam (design D1). It is built once from a
//! validated [`LlmConfig`](config::preset::LlmConfig) (see [`LlmClient::new`])
//! and shared across threads; [`LlmClient::call`] performs a blocking
//! `POST {api_base_url}/chat/completions` and returns the model's content
//! (design D2).
//!
//! Retry policy (task 1.3): [`LlmError::is_retryable`] is the single decision
//! point. Retryable failures (429/5xx, transport) are retried up to
//! `max_retries` times; before each retry the client sleeps an exponential
//! backoff — `500 ms · 2^(retry-1)` scaled by a ±20% jitter. Non-retryable
//! failures (other statuses, empty content, parse, configuration) are
//! returned immediately. Exhausting the budget returns
//! [`LlmError::RetriesExhausted`] carrying the last attempt's error. The
//! sleeper is injectable (the test-only `with_sleeper` seam) so no test ever
//! sleeps for real.
//!
//! Design decisions:
//! - message `content` is a plain string rather than a `[{type, text}]`
//!   parts array — for text-only prompts the wire form is equivalent, and a
//!   string is simpler and cheaper to serialize;
//! - `temperature` / `seed` / `max_tokens` are always serialized (zero values
//!   are not omitted); explicit values are deterministic and every
//!   OpenAI-compatible API accepts them;
//! - silent config defaults are replaced by fail-fast validation in
//!   [`LlmClient::new`] (a zero timeout / retry count / token budget is a
//!   configuration bug, not a value to paper over);
//! - backoff base is 500 ms with multiplicative ±20% jitter: a faster first
//!   retry suits the laptop-local use case, and multiplicative jitter scales
//!   with the delay instead of becoming negligible as the backoff grows;
//! - the request body is built once and re-sent on every retry; the body is a
//!   pure function of the config and the prompts, so the wire bytes are
//!   identical on each attempt;
//! - jitter draws come from a small hand-rolled splitmix64 stream instead of a
//!   `rand` dependency (not in the frozen stack): jitter only needs to
//!   desynchronize concurrent retry loops, not to be cryptographic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use config::preset::{LlmConfig, ResponseFormat};
use serde::{Deserialize, Serialize};
use ureq::{Agent, http::Uri};

use crate::error::LlmError;

/// The first retry's backoff: 500 ms (a faster first retry suits the laptop-local use case).
const BACKOFF_BASE_MS: u64 = 500;
/// The exponential factor: every retry doubles the previous delay.
const BACKOFF_FACTOR: u64 = 2;
/// The jitter spread: each delay is scaled by a random factor in [0.8, 1.2).
const JITTER_SPREAD: f64 = 0.2;
/// The splitmix64 increment (the 64-bit golden ratio).
const SPLITMIX_GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// Synchronous client for an OpenAI-compatible chat completions endpoint.
///
/// Built once from a validated [`LlmConfig`](config::preset::LlmConfig) and
/// shared across threads (`Send + Sync`; the ureq connection pool and the
/// backoff sleeper live behind `Arc`s, so cloning is cheap). Methods block —
/// call from sync contexts or `spawn_blocking` workers, never from inside an
/// async task (design D2).
pub struct LlmClient {
    /// Shared connection pool: global timeout, identity, and status handling
    /// are configured once in [`LlmClient::new`].
    agent: Agent,
    /// `{api_base_url}/chat/completions`, normalized (no trailing slash in the
    /// base).
    endpoint: String,
    /// The validated configuration: single source of all request parameters
    /// (model, sampling, budget, auth, retry budget).
    config: LlmConfig,
    /// The backoff sleeper between retries. The default is a real
    /// [`std::thread::sleep`]; tests replace it via the `with_sleeper` seam
    /// so no test sleeps.
    sleeper: Arc<dyn Fn(Duration) + Send + Sync>,
    /// The seed of this client's splitmix64 jitter stream (see
    /// [`LlmClient::next_jitter`]).
    rng_seed: u64,
    /// The draw counter of the jitter stream.
    rng_counter: AtomicU64,
}

impl Clone for LlmClient {
    fn clone(&self) -> Self {
        // The clone continues the jitter stream where the original left off,
        // so two clones never produce the same jitter sequence (identical
        // sequences would synchronize their retry timing).
        let next_state = self.rng_seed.wrapping_add(
            self.rng_counter
                .load(Ordering::Relaxed)
                .wrapping_mul(SPLITMIX_GOLDEN),
        );
        Self {
            agent: self.agent.clone(),
            endpoint: self.endpoint.clone(),
            config: self.config.clone(),
            sleeper: Arc::clone(&self.sleeper),
            rng_seed: splitmix64_mix(next_state),
            rng_counter: AtomicU64::new(0),
        }
    }
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
    /// Design: silent defaults are replaced by fail-fast validation — a
    /// missing or zero value is a configuration bug and fails fast instead.
    pub fn new(config: &LlmConfig) -> Result<Self, LlmError> {
        let base_url = config.api_base_url.trim();
        if base_url.is_empty() {
            return Err(LlmError::Configuration(
                "api_base_url must not be empty".to_string(),
            ));
        }
        // Normalize away a trailing slash so "http://x" and "http://x/" yield
        // the same endpoint (avoids a double slash in the URL).
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
        // not by ureq, so error messages can carry the response body.
        let agent_config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent("synopsis/0.1.0")
            .build();

        // Jitter stream seed: the wall clock, mixed so its low bits (which
        // carry the least entropy) do not bias the early draws.
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let rng_seed = splitmix64_mix(elapsed.as_nanos() as u64);

        Ok(Self {
            agent: Agent::new_with_config(agent_config),
            endpoint,
            config: config.clone(),
            sleeper: Arc::new(std::thread::sleep),
            rng_seed,
            rng_counter: AtomicU64::new(0),
        })
    }

    /// Replaces the backoff sleeper (test seam; reachable from integration
    /// tests through the docs-hidden `test_support` module).
    ///
    /// The default sleeper is a real [`std::thread::sleep`]; tests install a
    /// recorder to observe the backoff delays without sleeping. The sleeper
    /// runs on the calling thread, between retry attempts.
    pub(crate) fn with_sleeper(self, sleeper: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        Self {
            sleeper: Arc::new(sleeper),
            ..self
        }
    }

    /// Performs one chat completion and returns the model's text content.
    ///
    /// Builds the request body (model, `system` + `user` messages, sampling,
    /// and the structured-output mode) once, then `POST`s it to
    /// `{api_base_url}/chat/completions` with an `Authorization: Bearer`
    /// header only when `api_key` is non-empty, retrying transient failures
    /// (task 1.3) and parsing `choices[0].message.content` (trimmed) out of
    /// the final `200` response.
    ///
    /// Retry policy: up to `max_retries` retries after the initial attempt
    /// (total attempts = `max_retries + 1`), each preceded by an exponential
    /// backoff sleep ([`LlmClient::backoff_delay`]). Only retryable errors
    /// ([`LlmError::is_retryable`]: 429/5xx, transport) are retried;
    /// everything else is returned immediately.
    ///
    /// `schema` and `schema_name` are used only in
    /// [`ResponseFormat::JsonSchema`] mode: a non-empty `schema` is parsed as
    /// JSON and embedded under `response_format.json_schema.{name, schema}`,
    /// defaulting the name to `llm_output` when `schema_name` is empty; an
    /// empty or absent `schema` falls back to `json_object`. In
    /// [`ResponseFormat::JsonObject`] mode both arguments are ignored. The
    /// schema is embedded in every retried request (the payload is built once
    /// and re-sent verbatim).
    ///
    /// The per-request timeout is the config `timeout_ms`, applied globally to
    /// the connection pool in [`LlmClient::new`].
    ///
    /// # Errors
    ///
    /// - [`LlmError::Configuration`] when a prompt is empty;
    /// - [`LlmError::HttpStatus`] for a non-retryable non-`200` status (other
    ///   4xx, 3xx, 1xx) — returned immediately;
    /// - [`LlmError::RetriesExhausted`] when the retry budget is exhausted;
    ///   carries the last attempt's [`LlmError::RetryableHttp`] (429/5xx) or
    ///   [`LlmError::Transport`] (connection / DNS / timeout) error;
    /// - [`LlmError::EmptyContent`] for a `200` whose content is empty or
    ///   whitespace-only — non-retryable by design;
    /// - [`LlmError::Truncated`] for a `200` whose non-empty content stopped
    ///   at the token budget (`finish_reason == "length"`) — non-retryable by
    ///   design;
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

        // Built once and re-sent on every retry: the body is a pure function
        // of the config and the prompts, so the wire bytes are identical on
        // each attempt.
        let body = self.build_request_body(system, user, schema, schema_name)?;
        let payload = serde_json::to_string(&body)
            .map_err(|err| LlmError::Parse(format!("failed to serialize request: {err}")))?;

        self.call_with_retries(&payload)
    }

    /// Sends the pre-built payload, retrying retryable failures with backoff.
    ///
    /// Before every retry (never before the initial attempt) the sleeper is
    /// invoked with [`LlmClient::backoff_delay`]. A non-retryable error is
    /// returned immediately; exhausting the budget wraps the last attempt's
    /// error in [`LlmError::RetriesExhausted`] (here `attempts` = total
    /// attempts).
    fn call_with_retries(&self, payload: &str) -> Result<String, LlmError> {
        let max_attempts = self.config.max_retries as u32 + 1;
        let mut attempt = 0;
        loop {
            attempt += 1;
            if attempt > 1 {
                // `attempt - 1` is the 1-based retry index (first retry = 1).
                (self.sleeper)(self.backoff_delay(attempt - 1));
            }
            let result = self.send_once(payload);
            match result {
                Ok(content) => return Ok(content),
                // Non-retryable: retrying produces the same result (or the
                // configuration is broken).
                Err(err) if !err.is_retryable() => return Err(err),
                // The budget is used up: report exhaustion with the last
                // attempt's cause.
                Err(err) if attempt == max_attempts => {
                    return Err(LlmError::RetriesExhausted {
                        attempts: max_attempts,
                        last: Box::new(err),
                    });
                }
                // Retryable with budget left: back off and try again.
                Err(_) => {}
            }
        }
    }

    /// One attempt: POST the payload, classify the status, parse the content.
    fn send_once(&self, payload: &str) -> Result<String, LlmError> {
        let mut request = self.agent.post(&self.endpoint);
        if !self.config.api_key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.config.api_key));
        }
        let mut response = request
            .content_type("application/json")
            .send(payload)
            .map_err(|err| LlmError::from_transport(err, &self.endpoint))?;

        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|err| LlmError::from_transport(err, &self.endpoint))?;

        self.classify_and_parse(status.as_u16(), text)
    }

    /// The backoff delay before retry `retry` (1-based: 1 = the first retry).
    ///
    /// `BACKOFF_BASE_MS · BACKOFF_FACTOR^(retry-1)` scaled by a ±20% jitter
    /// drawn from this client's splitmix64 stream. The ±20% bands of
    /// consecutive retries are disjoint (…600 ms < 800 ms < 1600 ms…), so the
    /// delays strictly grow even in the worst case.
    pub(crate) fn backoff_delay(&self, retry: u32) -> Duration {
        let mut scaled_ms = BACKOFF_BASE_MS;
        // Bounded by the u64 bit width: past ~59 doublings the value has
        // saturated anyway, so the cap only guards a pathological config.
        for _ in 0..(retry.saturating_sub(1)).min(63) {
            scaled_ms = scaled_ms.saturating_mul(BACKOFF_FACTOR);
        }
        let u = self.next_jitter(); // [0, 1)
        let factor = 1.0 - JITTER_SPREAD + 2.0 * JITTER_SPREAD * u; // [0.8, 1.2)
        Duration::from_millis((scaled_ms as f64 * factor) as u64)
    }

    /// The next value of the jitter stream, normalized to [0, 1).
    ///
    /// A hand-rolled splitmix64 stream instead of a `rand` dependency (not in
    /// the frozen stack): jitter only needs to desynchronize concurrent retry
    /// loops, not to be cryptographic.
    fn next_jitter(&self) -> f64 {
        let draw = self.rng_counter.fetch_add(1, Ordering::Relaxed);
        let state = self
            .rng_seed
            .wrapping_add(draw.wrapping_mul(SPLITMIX_GOLDEN));
        splitmix64_mix(state) as f64 / u64::MAX as f64
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
    /// schema per call.
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
        // (falls through to `json_object` in that case).
        Ok(ResponseFormatBody {
            kind: "json_object".to_string(),
            json_schema: None,
        })
    }

    /// Maps an HTTP status plus body to a result or a classified error.
    fn classify_and_parse(&self, status: u16, body: String) -> Result<String, LlmError> {
        match status {
            // 429 (rate limited) and all 5xx are retryable:
            // `call_with_retries` retries them; everything else is returned
            // to the caller immediately.
            429 | 500..=599 => Err(LlmError::RetryableHttp { status, body }),
            200 => parse_chat_completion(&body, self.config.max_tokens),
            // Every other status (other 4xx, 3xx, 1xx) is non-retryable.
            _ => Err(LlmError::HttpStatus { status, body }),
        }
    }
}

/// The splitmix64 finalizer: a cheap, well-mixed 64-bit permutation.
fn splitmix64_mix(x: u64) -> u64 {
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Parses a `200` response body into the model's text content.
///
/// A non-empty content that stopped at the token budget
/// (`finish_reason == "length"`) is a truncated prefix, not a complete
/// answer: it becomes [`LlmError::Truncated`] (non-retryable) instead of an
/// opaque downstream parse failure. An empty content stays
/// [`LlmError::EmptyContent`] (that arm already reports `finish_reason`).
fn parse_chat_completion(body: &str, max_tokens: i32) -> Result<String, LlmError> {
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
    if choice.finish_reason.as_deref() == Some("length") {
        // Intentionally non-retryable: the same prompt with the same budget
        // truncates again.
        return Err(LlmError::Truncated { max_tokens });
    }
    Ok(content.to_string())
}

// ── Request types ────────────────────────────────────────────────────────────

/// The chat-completions request body (wire shape; see the module docs for the
/// design decisions).
#[derive(Debug, Serialize)]
struct RequestBody {
    /// The model identifier.
    model: String,
    /// The conversation: `system` then `user`.
    messages: Vec<RequestMessage>,
    /// Sampling temperature (always sent).
    temperature: f64,
    /// Sampling seed (always sent).
    seed: i64,
    /// Maximum tokens (always sent).
    max_tokens: i32,
    /// Structured-output mode.
    response_format: ResponseFormatBody,
}

/// One message in the conversation.
#[derive(Debug, Serialize)]
struct RequestMessage {
    /// The role: `system` or `user`.
    role: String,
    /// The message text (a plain string).
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
    fn new_accepts_zero_max_retries() {
        let config = config_with("http://127.0.0.1:1", |c| c.max_retries = 0);
        let client = LlmClient::new(&config).unwrap();
        assert_eq!(client.config.max_retries, 0);
    }

    // ── Truncation detection (fix-demo-ingestion-pipeline 1.1) ─────────────

    /// A `200` body with non-empty `content` and `finish_reason: "length"`
    /// (a truncated JSON prefix — the demo NER failure mode). The content is
    /// JSON-escaped so it may contain quotes of its own.
    fn truncated_body(content: &str) -> String {
        format!(
            "{{\"choices\":[{{\"message\":{{\"content\":{}}},\"finish_reason\":\"length\"}}]}}",
            serde_json::to_string(content).unwrap()
        )
    }

    #[test]
    fn parse_chat_completion_truncated_body_is_truncated_error() {
        let body = truncated_body("{\"entities\":[");
        let err = parse_chat_completion(&body, 4096).unwrap_err();
        match &err {
            LlmError::Truncated { max_tokens } => {
                assert_eq!(*max_tokens, 4096, "got: {err}");
            }
            other => panic!("expected Truncated, got: {other:?}"),
        }
        assert!(!err.is_retryable(), "truncation must not be retryable");
        assert!(
            err.to_string().contains("max_tokens=4096"),
            "Display must name max_tokens: {err}"
        );
        assert!(
            err.to_string().contains("finish_reason=length"),
            "Display must name the finish reason: {err}"
        );
    }

    #[test]
    fn parse_chat_completion_stop_finish_reason_returns_content() {
        let body = r#"{"choices":[{"message":{"content":"  ok "},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_chat_completion(body, 1024).unwrap(), "ok");
    }

    #[test]
    fn parse_chat_completion_absent_finish_reason_returns_content() {
        let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        assert_eq!(parse_chat_completion(body, 1024).unwrap(), "ok");
    }

    #[test]
    fn classify_and_parse_passes_configured_max_tokens() {
        // `valid_config` pins max_tokens = 1024: the error must carry the
        // configured budget, not a default.
        let client = LlmClient::new(&valid_config("http://127.0.0.1:9999")).unwrap();
        assert_eq!(client.config.max_tokens, 1024);
        let body = truncated_body("{\"entities\":[]");
        let err = client.classify_and_parse(200, body).unwrap_err();
        match &err {
            LlmError::Truncated { max_tokens } => {
                assert_eq!(*max_tokens, 1024, "got: {err}");
            }
            other => panic!("expected Truncated, got: {other:?}"),
        }
    }
}
