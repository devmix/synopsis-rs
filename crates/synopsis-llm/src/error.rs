//! Crate error type.
//!
//! [`LlmError`] covers the failure classes of the llm crate: configuration
//! validation (constructor), HTTP status responses (retryable 429/5xx vs
//! non-retryable other statuses), transport failures (connection, DNS,
//! timeout, protocol), empty or truncated model content, response parsing,
//! and retry exhaustion.
//!
//! Library error per workspace convention: `thiserror` with one variant per
//! failure class. [`LlmError::is_retryable`] is the single retry decision
//! point the retry policy (task 1.3) builds on.

use thiserror::Error;

/// All error conditions surfaced by this crate.
#[derive(Debug, Error)]
pub enum LlmError {
    /// The client configuration violated a documented invariant (checked once
    /// in [`crate::LlmClient::new`]).
    #[error("invalid llm configuration: {0}")]
    Configuration(String),
    /// The API answered with a retryable status (429 or 5xx).
    #[error("retryable HTTP {status}: {body}")]
    RetryableHttp {
        /// The response status code.
        status: u16,
        /// The response body (diagnostic; may be empty).
        body: String,
    },
    /// The API answered with a non-retryable status (other 4xx).
    #[error("HTTP {status}: {body}")]
    HttpStatus {
        /// The response status code.
        status: u16,
        /// The response body (diagnostic; may be empty).
        body: String,
    },
    /// A transport-level failure: connection refused, DNS, protocol error,
    /// TLS, or the request hit the configured timeout.
    #[error("transport error: {0}")]
    Transport(String),
    /// The model answered 2xx but with empty content. Intentionally
    /// non-retryable: retrying produces the same result.
    #[error(
        "empty response from model (finish_reason={finish_reason:?}, \
         reasoning_content_length={reasoning_content_len})"
    )]
    EmptyContent {
        /// The response `finish_reason` (or "(none)").
        finish_reason: String,
        /// Length of `reasoning_content`: thinking models can spend the whole
        /// token budget on reasoning and return no visible content.
        reasoning_content_len: usize,
    },
    /// The model stopped at the token budget (`finish_reason == "length"`)
    /// with non-empty content: the content is a truncated prefix and any
    /// downstream parsing of it is meaningless. Intentionally non-retryable:
    /// the same prompt with the same budget truncates again.
    #[error(
        "LLM response truncated at max_tokens={max_tokens} \
             (finish_reason=length); raise max_tokens in the config"
    )]
    Truncated {
        /// The configured `max_tokens` the response was truncated at.
        max_tokens: i32,
    },
    /// A 2xx response could not be parsed as a chat completion (malformed
    /// JSON, missing `choices`).
    #[error("response parse error: {0}")]
    Parse(String),
    /// The retry budget was exhausted; carries the last attempt's error.
    #[error("exhausted {attempts} attempts: {last}")]
    RetriesExhausted {
        /// Total attempts made (initial + retries).
        attempts: u32,
        /// The error of the last attempt.
        #[source]
        last: Box<LlmError>,
    },
}

impl LlmError {
    /// Whether a retry is worth attempting.
    ///
    /// Retryable: 429/5xx (server-side, may clear) and transport failures
    /// (transient by nature). Non-retryable: configuration, other HTTP
    /// statuses, empty content (deterministic), truncated content (the same
    /// budget truncates again), parse failures (the same body will fail
    /// again), and exhaustion (already terminal).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RetryableHttp { .. } | Self::Transport { .. })
    }

    /// Maps a ureq transport error to an [`LlmError`], tagging it with the
    /// request URL for diagnostics.
    ///
    /// Classification: timeouts and transient network failures (connection,
    /// DNS, protocol, TLS, redirects) become [`LlmError::Transport`]
    /// (retryable); a malformed URI or a malformed request becomes
    /// [`LlmError::Configuration`] (permanent — the constructor already
    /// validates the endpoint, so these arms are defensive).
    #[must_use]
    pub fn from_transport(err: ureq::Error, url: &str) -> Self {
        match err {
            ureq::Error::Timeout(_) => Self::Transport(format!("request to {url} timed out")),
            ureq::Error::BadUri(detail) => {
                Self::Configuration(format!("invalid endpoint {url}: {detail}"))
            }
            ureq::Error::Http(detail) => {
                Self::Configuration(format!("malformed HTTP request for {url}: {detail}"))
            }
            ureq::Error::InvalidProxyUrl => {
                Self::Configuration(format!("invalid proxy configuration for {url}"))
            }
            // Unreachable with the constructor's `http_status_as_error(false)`
            // (statuses come back as responses); mapped defensively so the
            // classification stays correct even if the agent config changes.
            ureq::Error::StatusCode(status) => {
                if status == 429 || (500..=599).contains(&status) {
                    Self::RetryableHttp {
                        status,
                        body: String::new(),
                    }
                } else {
                    Self::HttpStatus {
                        status,
                        body: String::new(),
                    }
                }
            }
            // Transient: connection refused, DNS, protocol, TLS, redirects,
            // body limits. `ureq::Error` is `#[non_exhaustive]`, so the
            // wildcard is required and stays the transient bucket.
            other => Self::Transport(format!("request to {url} failed: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (constructing error variants
    // cannot fail).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::error::Error as _;

    use super::*;

    #[test]
    fn from_transport_maps_timeout_to_retryable_transport() {
        let err = LlmError::from_transport(
            ureq::Error::Timeout(ureq::Timeout::Global),
            "http://127.0.0.1:1/chat/completions",
        );
        assert!(matches!(err, LlmError::Transport(_)), "got: {err}");
        assert!(err.is_retryable());
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[test]
    fn from_transport_maps_io_errors_to_retryable_transport() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = LlmError::from_transport(ureq::Error::Io(io_err), "http://127.0.0.1:1");
        assert!(matches!(err, LlmError::Transport(_)), "got: {err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn from_transport_maps_host_not_found_to_retryable_transport() {
        let err = LlmError::from_transport(ureq::Error::HostNotFound, "http://nope.invalid");
        assert!(matches!(err, LlmError::Transport(_)), "got: {err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn from_transport_maps_status_codes_by_retryability() {
        // Defensive arm: the constructor disables ureq status errors, but the
        // mapping must stay correct if that ever changes.
        let limited = LlmError::from_transport(ureq::Error::StatusCode(429), "http://x");
        assert!(matches!(
            limited,
            LlmError::RetryableHttp { status: 429, .. }
        ));
        let server = LlmError::from_transport(ureq::Error::StatusCode(503), "http://x");
        assert!(matches!(
            server,
            LlmError::RetryableHttp { status: 503, .. }
        ));
        let not_found = LlmError::from_transport(ureq::Error::StatusCode(404), "http://x");
        assert!(matches!(
            not_found,
            LlmError::HttpStatus { status: 404, .. }
        ));
        assert!(!not_found.is_retryable());
    }

    #[test]
    fn from_transport_maps_bad_uri_to_configuration() {
        let err = LlmError::from_transport(
            ureq::Error::BadUri("missing scheme".to_string()),
            "http://127.0.0.1:1",
        );
        assert!(matches!(err, LlmError::Configuration(_)), "got: {err}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn is_retryable_classifies_every_variant() {
        assert!(
            LlmError::RetryableHttp {
                status: 429,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            LlmError::RetryableHttp {
                status: 503,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            !LlmError::HttpStatus {
                status: 404,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            !LlmError::HttpStatus {
                status: 401,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(LlmError::Transport("refused".to_string()).is_retryable());
        assert!(
            !LlmError::EmptyContent {
                finish_reason: "stop".to_string(),
                reasoning_content_len: 0,
            }
            .is_retryable()
        );
        assert!(!LlmError::Truncated { max_tokens: 4096 }.is_retryable());
        assert!(!LlmError::Parse("bad json".to_string()).is_retryable());
        assert!(!LlmError::Configuration("empty url".to_string()).is_retryable());
        let exhausted = LlmError::RetriesExhausted {
            attempts: 3,
            last: Box::new(LlmError::Transport("refused".to_string())),
        };
        assert!(!exhausted.is_retryable());
    }

    #[test]
    fn retry_exhausted_reports_attempts_and_last_error() {
        let err = LlmError::RetriesExhausted {
            attempts: 4,
            last: Box::new(LlmError::RetryableHttp {
                status: 500,
                body: "boom".to_string(),
            }),
        };
        assert_eq!(
            err.to_string(),
            "exhausted 4 attempts: retryable HTTP 500: boom"
        );
        // The last error is reachable as the source (error-chain convention).
        let source = err
            .source()
            .expect("RetriesExhausted carries a source")
            .to_string();
        assert_eq!(source, "retryable HTTP 500: boom");
    }

    #[test]
    fn display_messages_carry_diagnostics() {
        assert_eq!(
            LlmError::RetryableHttp {
                status: 429,
                body: "slow down".to_string()
            }
            .to_string(),
            "retryable HTTP 429: slow down"
        );
        assert_eq!(
            LlmError::HttpStatus {
                status: 401,
                body: "nope".to_string()
            }
            .to_string(),
            "HTTP 401: nope"
        );
        assert_eq!(
            LlmError::EmptyContent {
                finish_reason: "length".to_string(),
                reasoning_content_len: 512,
            }
            .to_string(),
            "empty response from model (finish_reason=\"length\", \
             reasoning_content_length=512)"
        );
        assert_eq!(
            LlmError::Truncated { max_tokens: 16384 }.to_string(),
            "LLM response truncated at max_tokens=16384 \
             (finish_reason=length); raise max_tokens in the config"
        );
        assert_eq!(
            LlmError::Configuration("api_base_url must not be empty".to_string()).to_string(),
            "invalid llm configuration: api_base_url must not be empty"
        );
    }
}
