//! Synchronous HTTP file downloader with retries, SSRF protection, progress
//! reporting, and post-download size verification (design D8).
//!
//! [`Downloader::download`] fetches a single file from an `http`/`https` URL
//! to a local path. It is the download primitive for the runtime-library
//! manager (task 1.4) and the model manager (task 1.5).
//!
//! Behavior (design D8):
//! - up to 3 retries with a 2 s delay between attempts;
//! - a 10-minute end-to-end timeout per request;
//! - a `synopsis/0.1.0` User-Agent;
//! - SSRF protection: the host is resolved and the download is refused when
//!   any resolved address is loopback, private, link-local, or unspecified —
//!   before any network request is made;
//! - progress reporting through an [`indicatif::ProgressBar`] (auto-hidden
//!   when stdout is not a terminal);
//! - post-download size verification against the caller-provided expected
//!   size;
//! - the destination file is removed on any failure, so a partial or
//!   mismatched download is never left behind.
//!
//! Design decisions:
//! - unresolvable hosts are rejected instead of allowed (the SSRF verdict
//!   must be computable before a request);
//! - permanent failures (non-retryable HTTP status, size mismatch) abort
//!   immediately instead of being retried pointlessly.
//!
//! Known limitation: the SSRF verdict covers the initial host only; a
//! redirect to a private address is not re-checked.

use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use ureq::http::{Response, Uri};
use ureq::{Agent, Body, Error as UreqError};

use crate::error::EmbeddingError;

/// Retry budget after the initial attempt (design D8).
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Delay between retry attempts (design D8).
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// End-to-end timeout for a single request (design D8).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// User-Agent identifying the client.
const USER_AGENT: &str = "synopsis/0.1.0";

/// Chunk size for streaming response bodies to disk.
const READ_BUFFER_SIZE: usize = 64 * 1024;

/// Synchronous HTTP file downloader (design D8).
///
/// The instance holds the connection pool behind an `Arc` and is cheap to
/// share; methods block, so async callers dispatch onto a blocking thread
/// pool.
pub struct Downloader {
    agent: Agent,
    max_retries: u32,
    retry_delay: Duration,
    ssrf_check: bool,
}

impl Downloader {
    /// Creates a downloader with the design D8 defaults: 3 retries with a
    /// 2 s delay, a 10-minute request timeout, the `synopsis` User-Agent, and
    /// SSRF protection enabled.
    #[must_use]
    pub fn new() -> Self {
        Self::with_params(
            DEFAULT_MAX_RETRIES,
            DEFAULT_RETRY_DELAY,
            DEFAULT_TIMEOUT,
            true,
        )
    }

    /// Constructor with explicit parameters (tests use a zero retry delay and
    /// disabled SSRF checking to reach the local mock server;
    /// [`crate::library::LibraryManager::with_downloader`]).
    pub(crate) fn with_params(
        max_retries: u32,
        retry_delay: Duration,
        timeout: Duration,
        ssrf_check: bool,
    ) -> Self {
        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: Agent::new_with_config(config),
            max_retries,
            retry_delay,
            ssrf_check,
        }
    }

    /// Downloads `url` to `dest`, creating parent directories as needed.
    ///
    /// The URL must use the `http` or `https` scheme. With SSRF protection
    /// (the default) the host is resolved first and the download is refused
    /// when any resolved address is loopback, private, link-local, or
    /// unspecified — before any network request is made.
    ///
    /// Transient failures (connection errors, timeouts, 5xx, 408, 429) are
    /// retried up to `max_retries` times with a delay between attempts;
    /// permanent failures (other 4xx, post-download size mismatch) fail
    /// immediately.
    ///
    /// `expected_size`, when `Some`, serves as the progress-bar total when the
    /// server sends no `Content-Length`, and is verified against the number of
    /// bytes actually written (design D8).
    ///
    /// On any failure the destination file (if created) is removed.
    ///
    /// # Errors
    ///
    /// [`EmbeddingError::Download`] for URL/scheme/SSRF/retry/size failures;
    /// [`EmbeddingError::Io`] for filesystem failures.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        expected_size: Option<u64>,
    ) -> Result<(), EmbeddingError> {
        let uri: Uri = url
            .parse()
            .map_err(|err| EmbeddingError::Download(format!("invalid URL {url}: {err}")))?;
        if self.ssrf_check {
            assert_public_host(&uri)?;
        }
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let mut last_error: Option<EmbeddingError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                std::thread::sleep(self.retry_delay);
            }
            match self.attempt_download(url, dest, expected_size) {
                Ok(()) => return Ok(()),
                Err(Failure::Permanent(err)) => return Err(err),
                Err(Failure::Transient(err)) => last_error = Some(err),
            }
        }
        match last_error {
            Some(last) => Err(EmbeddingError::Download(format!(
                "download of {url} failed after {} attempts: {last}",
                self.max_retries + 1,
            ))),
            None => Err(EmbeddingError::Download(format!(
                "download of {url} failed"
            ))),
        }
    }

    /// Performs one download attempt: HTTP GET, stream the body to `dest`,
    /// and verify the size.
    fn attempt_download(
        &self,
        url: &str,
        dest: &Path,
        expected_size: Option<u64>,
    ) -> Result<(), Failure> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| classify_request_error(err, url))?;

        let total = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.parse::<u64>().ok())
            .or(expected_size)
            .unwrap_or(0);

        let bar = create_progress_bar(total);
        let mut file = std::fs::File::create(dest)
            .map_err(|err| Failure::Permanent(EmbeddingError::Io(err)))?;

        let written = stream_body(&mut response, &mut file, &bar).map_err(|err| {
            remove_quietly(dest);
            Failure::Transient(EmbeddingError::Io(err))
        })?;

        if let Some(expected) = expected_size
            && written != expected
        {
            remove_quietly(dest);
            return Err(Failure::Permanent(EmbeddingError::Download(format!(
                "size mismatch for {}: got {written} bytes, expected {expected}",
                dest.display()
            ))));
        }

        bar.finish_and_clear();
        Ok(())
    }
}

impl Default for Downloader {
    fn default() -> Self {
        Self::new()
    }
}

/// Internal outcome of a single download attempt.
enum Failure {
    /// Permanent failure: retrying cannot help.
    Permanent(EmbeddingError),
    /// Transient failure: worth another attempt.
    Transient(EmbeddingError),
}

/// Streams the response body to `file`, updating the progress bar, and
/// returns the number of bytes written.
fn stream_body(
    response: &mut Response<Body>,
    file: &mut std::fs::File,
    bar: &ProgressBar,
) -> std::io::Result<u64> {
    let mut body = response.body_mut().as_reader();
    let mut buffer = [0u8; READ_BUFFER_SIZE];
    let mut written: u64 = 0;
    loop {
        let read = body.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        // `usize` fits a `u64` on every platform this project builds for.
        written += read as u64;
        bar.inc(read as u64);
    }
    file.flush()?;
    Ok(written)
}

/// Creates the progress bar for a download of `total` bytes (0 = unknown).
fn create_progress_bar(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total);
    if total == 0 {
        // Unknown size: show a plain byte counter instead of a bar with a
        // zero denominator.
        if let Ok(style) = ProgressStyle::with_template("{spinner} {bytes} downloaded") {
            bar.set_style(style);
        }
    }
    bar
}

/// Classifies a ureq request error as permanent or transient.
///
/// 408/429 and all 5xx are transient; other 4xx are permanent (retrying a
/// 404 cannot help). Everything else (connection, timeout, protocol, DNS)
/// is transient and may clear on the next attempt.
fn classify_request_error(err: UreqError, url: &str) -> Failure {
    match err {
        UreqError::StatusCode(status) => {
            let error = EmbeddingError::Download(format!("HTTP {status} for {url}"));
            if status == 408 || status == 429 || (500..=599).contains(&status) {
                Failure::Transient(error)
            } else {
                Failure::Permanent(error)
            }
        }
        UreqError::BadUri(detail) => Failure::Permanent(EmbeddingError::Download(format!(
            "invalid URL {url}: {detail}"
        ))),
        other => Failure::Transient(EmbeddingError::Download(format!(
            "request to {url} failed: {other}"
        ))),
    }
}

/// Validates the URL scheme and rejects hosts that resolve to loopback,
/// private, link-local, or unspecified addresses (SSRF protection).
///
/// The check runs before any network request. A host that cannot be resolved
/// is rejected: without a verdict the SSRF check would be meaningless.
fn assert_public_host(uri: &Uri) -> Result<(), EmbeddingError> {
    let scheme = uri.scheme_str().unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(EmbeddingError::Download(format!(
            "URL scheme {scheme:?} is not allowed (only http and https)"
        )));
    }
    let host = uri
        .host()
        .ok_or_else(|| EmbeddingError::Download(format!("URL {uri} has no host")))?;
    // http 1.5 keeps the square brackets in the host for IPv6 literals.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    let addrs = (host, port).to_socket_addrs().map_err(|err| {
        EmbeddingError::Download(format!(
            "cannot verify that host {host} is public (resolution failed: {err})"
        ))
    })?;
    for addr in addrs {
        if is_disallowed_ip(addr.ip()) {
            return Err(EmbeddingError::Download(format!(
                "access to private address denied: host {host} resolves to {}",
                addr.ip()
            )));
        }
    }
    Ok(())
}

/// Returns true for addresses that must never be contacted: loopback,
/// private (RFC 1918 / ULA), link-local, unspecified, and IPv4-mapped IPv6
/// forms of any of those.
///
/// `Ipv6Addr::is_private` is not yet stable in std, so the ULA range
/// (fc00::/7) is checked manually.
fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => is_disallowed_ip(IpAddr::V4(mapped)),
            None => {
                v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unspecified() || is_ula(v6)
            }
        },
    }
}

/// True for IPv6 unique local addresses (fc00::/7).
fn is_ula(addr: std::net::Ipv6Addr) -> bool {
    addr.segments()[0] & 0xfe00 == 0xfc00
}

/// Removes a failed/partial download; best-effort by design.
fn remove_quietly(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Minimal single-threaded HTTP/1.1 server on 127.0.0.1 with an ephemeral
    /// port — plain TCP, no TLS, no keep-alive. Enough for the downloader
    /// tests; keeps CI network-free.
    struct MockServer {
        url: String,
        requests: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        /// Starts the server; `handler` maps the 0-based request index to the
        /// (status code, body) to serve.
        fn start(handler: impl Fn(usize) -> (u16, Vec<u8>) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let shutdown = Arc::new(AtomicBool::new(false));
            let server_requests = Arc::clone(&requests);
            let server_shutdown = Arc::clone(&shutdown);
            let thread = std::thread::spawn(move || {
                loop {
                    if server_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let index = server_requests.fetch_add(1, Ordering::SeqCst);
                            handle_connection(stream, index, &handler);
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
                shutdown,
                thread: Some(thread),
            }
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

    /// Reads one request (headers only — all test requests are GETs) and
    /// serves the handler's response with `Connection: close`.
    fn handle_connection(
        mut stream: TcpStream,
        index: usize,
        handler: &impl Fn(usize) -> (u16, Vec<u8>),
    ) {
        let _ = stream.set_nonblocking(false);
        let mut buffer = [0u8; 4096];
        let mut received = Vec::new();
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    received.extend_from_slice(&buffer[..n]);
                    if received.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let (status, body) = handler(index);
        let reason = if status == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        // Let the client drain the response before the socket is closed.
        std::thread::sleep(Duration::from_millis(25));
    }

    /// Downloader for tests: retry budget like production, zero delay for
    /// speed, SSRF check off so the 127.0.0.1 mock server is reachable.
    fn test_downloader() -> Downloader {
        Downloader::with_params(3, Duration::ZERO, Duration::from_secs(10), false)
    }

    /// Fresh per-test directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embedding-dl-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn downloads_file_verifies_size_and_creates_parent_dirs() {
        let content: &[u8] = b"hello world download data";
        let server = MockServer::start(move |_| (200, content.to_vec()));
        let dir = temp_dir("ok");
        let dest = dir.join("nested").join("file.bin");

        test_downloader()
            .download(&server.url, &dest, Some(content.len() as u64))
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn retries_transient_failures_then_succeeds() {
        let server = MockServer::start(|index| {
            if index < 2 {
                (500, Vec::new())
            } else {
                (200, b"recovered".to_vec())
            }
        });
        let dir = temp_dir("retry");
        let dest = dir.join("file.bin");

        test_downloader()
            .download(&server.url, &dest, Some(9))
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"recovered");
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn exhausts_retries_and_removes_partial_file() {
        let server = MockServer::start(|_| (500, Vec::new()));
        let dir = temp_dir("exhaust");
        let dest = dir.join("file.bin");

        let err = test_downloader()
            .download(&server.url, &dest, None)
            .unwrap_err();

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert!(err.to_string().contains("4 attempts"), "got: {err}");
        assert!(!dest.exists(), "partial file must be removed");
        assert_eq!(server.request_count(), 4, "1 initial + 3 retries");
    }

    #[test]
    fn permanent_4xx_is_not_retried() {
        let server = MockServer::start(|_| (404, Vec::new()));
        let dir = temp_dir("notfound");
        let dest = dir.join("file.bin");

        let err = test_downloader()
            .download(&server.url, &dest, None)
            .unwrap_err();

        assert!(err.to_string().contains("404"), "got: {err}");
        assert_eq!(server.request_count(), 1, "4xx must not be retried");
        assert!(!dest.exists());
    }

    #[test]
    fn size_mismatch_fails_and_removes_file() {
        let server = MockServer::start(|_| (200, b"short body".to_vec()));
        let dir = temp_dir("mismatch");
        let dest = dir.join("file.bin");

        let err = test_downloader()
            .download(&server.url, &dest, Some(1024))
            .unwrap_err();

        assert!(matches!(err, EmbeddingError::Download(_)), "got: {err}");
        assert!(err.to_string().contains("size mismatch"), "got: {err}");
        assert!(!dest.exists(), "mismatched file must be removed");
        assert_eq!(server.request_count(), 1, "mismatch must not be retried");
    }

    #[test]
    fn ssrf_rejects_private_and_loopback_addresses() {
        let downloader = Downloader::new();
        let dir = temp_dir("ssrf");
        let dest = dir.join("x.bin");

        for url in [
            "http://127.0.0.1:80/secret",
            "http://10.0.0.5/file",
            "http://192.168.1.10/file",
            "http://172.16.0.1/file",
            "http://172.31.255.255/file",
            "http://169.254.10.10/file",
            "http://[::1]/file",
            "http://localhost:80/file",
        ] {
            let err = downloader.download(url, &dest, None).unwrap_err();
            assert!(
                matches!(err, EmbeddingError::Download(_))
                    && err.to_string().contains("private address denied"),
                "{url}: got: {err}"
            );
        }
        assert!(!dest.exists());
    }

    #[test]
    fn ssrf_rejects_before_any_request_reaches_the_server() {
        let server = MockServer::start(|_| (200, b"should never be served".to_vec()));
        let downloader = Downloader::new();
        let dir = temp_dir("ssrf-live");
        let dest = dir.join("x.bin");

        let err = downloader.download(&server.url, &dest, None).unwrap_err();

        assert!(
            err.to_string().contains("private address denied"),
            "got: {err}"
        );
        assert_eq!(server.request_count(), 0, "no request may reach the server");
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_invalid_urls_and_non_http_schemes() {
        let downloader = Downloader::new();
        let dir = temp_dir("scheme");
        let dest = dir.join("x.bin");

        // `ftp` parses as a URI but is not an allowed scheme.
        let err = downloader
            .download("ftp://example.com/file", &dest, None)
            .unwrap_err();
        assert!(err.to_string().contains("scheme"), "got: {err}");

        // `file` and garbage do not even parse as an HTTP URI.
        for url in ["file:///etc/passwd", "not a url"] {
            let err = downloader.download(url, &dest, None).unwrap_err();
            assert!(err.to_string().contains("invalid URL"), "{url}: got: {err}");
        }

        assert!(!dest.exists());
    }

    #[test]
    fn progress_bar_handles_known_and_unknown_total() {
        let known = create_progress_bar(1024);
        assert_eq!(known.length(), Some(1024));
        let unknown = create_progress_bar(0);
        assert_eq!(unknown.length(), Some(0));
    }
}
