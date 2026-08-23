//! Shared synchronous HTTP client for OpenAI-compatible LLM APIs.
//!
//! New base-tier crate per design.md D1 (change `llm`): the only internal
//! dependency is `config` (source of [`LlmConfig`](config::preset::LlmConfig));
//! `graph` and `embedding` depend on this crate, never the other way around.
//!
//! The public seam is [`LlmClient`] (built from a validated
//! [`LlmConfig`](config::preset::LlmConfig)) and [`LlmError`]. The client is a
//! blocking ureq wrapper (design D2): async consumers must dispatch calls onto
//! `spawn_blocking` workers (workspace convention). The `call` method arrives
//! with task 1.2; this scaffold provides the constructor with configuration
//! validation and the error taxonomy the retry policy (task 1.3) builds on.
//!
//! Deliberate deviation from the oracle (`../synopsis/internal/llm/client.go`):
//! silent defaults are replaced by fail-fast validation — a zero timeout,
//! retry count, or token budget is a configuration bug, not a value to paper
//! over (see [`LlmClient::new`]).

pub mod error;

pub use error::LlmError;

use config::preset::{LlmConfig, ResponseFormat};
use ureq::{Agent, http::Uri};

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
    #[expect(dead_code, reason = "read by the call method (task 1.2)")]
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
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are valid).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn valid_config() -> LlmConfig {
        LlmConfig {
            api_base_url: "http://127.0.0.1:9999".to_string(),
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

    fn config_with(mutate: impl FnOnce(&mut LlmConfig)) -> LlmConfig {
        let mut config = valid_config();
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

    #[test]
    fn new_accepts_valid_config() {
        let client = LlmClient::new(&valid_config()).unwrap();
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
        let config = config_with(|c| {
            c.api_base_url = "https://api.example.com/v1".to_string();
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
        let config = config_with(|c| c.api_base_url = "http://127.0.0.1:1/".to_string());
        let client = LlmClient::new(&config).unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:1/chat/completions");
    }

    #[test]
    fn new_rejects_empty_base_url() {
        for url in ["", "   "] {
            let config = config_with(|c| c.api_base_url = url.to_string());
            assert_configuration(new_err(&config), "api_base_url must not be empty");
        }
    }

    #[test]
    fn new_rejects_base_url_without_scheme() {
        // A schemeless "host:port" does not parse as an absolute URI.
        let config = config_with(|c| c.api_base_url = "127.0.0.1:9999".to_string());
        assert_configuration(new_err(&config), "is not a valid URL");
    }

    #[test]
    fn new_rejects_non_http_scheme() {
        let config = config_with(|c| c.api_base_url = "ftp://files.example.com".to_string());
        assert_configuration(new_err(&config), "must use the http or https scheme");
    }

    #[test]
    fn new_rejects_garbage_base_url() {
        let config = config_with(|c| c.api_base_url = "not a url".to_string());
        assert_configuration(new_err(&config), "is not a valid URL");
    }

    #[test]
    fn new_rejects_empty_model_name() {
        for name in ["", "  "] {
            let config = config_with(|c| c.model_name = name.to_string());
            assert_configuration(new_err(&config), "model_name must not be empty");
        }
    }

    #[test]
    fn new_rejects_non_positive_timeout() {
        for timeout_ms in [0, -1] {
            let config = config_with(|c| c.timeout_ms = timeout_ms);
            assert_configuration(new_err(&config), "timeout_ms must be > 0");
        }
    }

    #[test]
    fn new_rejects_negative_max_retries() {
        let config = config_with(|c| c.max_retries = -1);
        assert_configuration(new_err(&config), "max_retries must be >= 0");
    }

    #[test]
    fn new_accepts_zero_max_retries() {
        let config = config_with(|c| c.max_retries = 0);
        let client = LlmClient::new(&config).unwrap();
        assert_eq!(client.config.max_retries, 0);
    }

    #[test]
    fn new_rejects_non_positive_max_tokens() {
        for max_tokens in [0, -5] {
            let config = config_with(|c| c.max_tokens = max_tokens);
            assert_configuration(new_err(&config), "max_tokens must be > 0");
        }
    }

    #[test]
    fn new_rejects_bad_temperature() {
        for temperature in [-0.1, f64::NAN] {
            let config = config_with(|c| c.temperature = temperature);
            assert_configuration(new_err(&config), "temperature must be a finite value >= 0");
        }
    }

    #[test]
    fn client_is_send_sync_and_cheap_to_clone() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LlmClient>();
        let client = LlmClient::new(&valid_config()).unwrap();
        let clone = client.clone();
        assert_eq!(clone.model(), client.model());
    }
}
