//! LLM NER provider (ingestion-ner design D5, task 2.5).
//!
//! Composes the finished pieces: [`NerPrompts`] renders the per-domain
//! system/user prompts (task 2.2, design D4), [`build_cache_key`] +
//! [`LlmNerCache`] persist responses (task 2.4, design D6),
//! [`generate_json_schema`] builds the structured-output schema and
//! [`parse_llm_response`] applies the D5 parse/validate rules (task 2.3).
//!
//! Extraction (design D5/D7): domains are processed in config order. Per
//! domain: render system (with the JSON example) + user (type lists + clean
//! content + the Document context block), build the cache key, check the
//! cache — on a miss call [`LlmClient::call`](llm::LlmClient::call) with the
//! generated schema and schema name `ner_result`, parse/validate, tag every
//! entity/fact with the normalized domain name, and store the tagged result
//! in the cache BEFORE merging it into the result (the composite stage
//! enriches the metadata with source data afterwards, design D7).
//!
//! # Design decisions
//!
//! - **`Ok(None)` for an empty merge.** The trait contract (design D2) says
//!   "nothing found" is `Ok(None)` — an empty merge becomes `Ok(None)`,
//!   like `RegexNer`.
//! - **Empty content short-circuits.** Empty/whitespace content returns
//!   `Ok(None)` without I/O (design D2; avoids a wasted HTTP round-trip).
//! - **No silent config defaults.** [`LlmClient::new`](llm::LlmClient::new)
//!   fails fast on a negative temperature or a non-positive `max_tokens`
//!   (llm crate design) instead of clamping them, so the validated config
//!   values are used verbatim in the cache key.
//! - **Schema is always passed to the call.** The mode decision lives inside
//!   [`LlmClient`](llm::LlmClient) (`ResponseFormat`): in `json_object`
//!   mode the schema arguments are ignored, in `json_schema` mode the schema
//!   is embedded under `ner_result`. Passing it unconditionally is
//!   behavior-identical and keeps the call site uniform.
//! - **Content renders into the user prompt** (no attachments parameter —
//!   design D4, recorded in `prompts.rs`).
//! - **The serde_json::Error from [`parse_llm_response`] is mapped at this
//!   boundary** into [`IngestionError::LlmNerParse`] (design D10: LLM
//!   failures are fatal for the extraction call; the parser itself stays
//!   pure, task 2.3).
//! - **Domain re-tagging on a cache hit is load-bearing.** The rendered
//!   prompts carry no domain name, so two domains with identical schemas
//!   produce the same cache key: the second domain's call hits the first
//!   domain's (already tagged) entry and must be re-tagged. The miss path
//!   stores the tagged result, so a shared key is last-writer-wins in the
//!   table.
//!
//! # Database access
//!
//! The provider owns an `Option<db::Db>` pool: `Some` enables caching,
//! `None` disables it. The cache holds its own pool independent of the
//! pipeline's transaction — cache writes are plain autocommit statements,
//! never part of a pipeline transaction. Each cache operation checks a
//! connection out via [`Db::with_conn`] for the duration of that operation
//! only, so a pooled slot is never held across the HTTP round-trip
//! (db design D11).

use std::fmt;

use config::DomainConfig;
use config::preset::LlmConfig;
use db::{ConnectionOrTx, Db};
use llm::LlmClient;
use serde_json::{Map, Value};

use super::regex::normalize;
use super::{
    LlmNerCache, NerPrompts, NerProvider, NerResult, build_cache_key, generate_json_schema,
    parse_llm_response,
};
use crate::error::IngestionError;

/// One domain in config order: the normalized name (for tagging) plus the
/// config the prompts and schema render from.
struct DomainEntry {
    /// Normalized domain name.
    name: String,
    /// The domain config (owned copy; the caller's slice may not outlive
    /// the provider).
    config: DomainConfig,
}

/// LLM-based NER provider over the domain configs (design D5).
///
/// Built once per pipeline run: the validated [`LlmClient`], the loaded
/// [`NerPrompts`], the domain configs in order, and an optional database
/// pool for the response cache (`None` disables caching). `Send + Sync` —
/// shareable behind `Arc` or a trait object (design D2).
///
/// The client is blocking by design (llm crate D2): call
/// [`extract_entities`](NerProvider::extract_entities) from sync contexts
/// or `spawn_blocking` workers.
pub struct LlmNer {
    /// The validated chat-completions client.
    client: LlmClient,
    /// The loaded system/user prompt templates.
    prompts: NerPrompts,
    /// Domain configs in config order with their normalized names.
    domains: Vec<DomainEntry>,
    /// Response cache pool; `None` disables caching.
    cache: Option<Db>,
    /// Cache-key parameters from the validated config (server, model,
    /// sampling, token budget).
    server: String,
    model: String,
    temperature: f64,
    max_tokens: i32,
}

impl fmt::Debug for LlmNer {
    /// Prints the construction inputs (the client and the pool are opaque
    /// handles, so their internals are not echoed).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmNer")
            .field("server", &self.server)
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("domains", &self.domains.len())
            .field("cache_enabled", &self.cache.is_some())
            .finish()
    }
}

impl LlmNer {
    /// Builds the provider from a validated LLM config and the domain
    /// configs.
    ///
    /// `domain_configs` must contain at least one entry
    /// ([`IngestionError::LlmNerNoDomains`] otherwise). `prompts` is the
    /// loaded template set (task 2.2); `cache` is the database pool for the
    /// response cache, or `None` to disable caching.
    ///
    /// # Errors
    ///
    /// - [`IngestionError::LlmNerNoDomains`] when no domain config is given;
    /// - [`IngestionError::Llm`] when the client configuration is invalid
    ///   (the llm crate's fail-fast validation).
    pub fn new(
        config: &LlmConfig,
        domain_configs: &[DomainConfig],
        prompts: NerPrompts,
        cache: Option<Db>,
    ) -> Result<Self, IngestionError> {
        if domain_configs.is_empty() {
            return Err(IngestionError::LlmNerNoDomains);
        }
        let client = LlmClient::new(config)?;
        let domains = domain_configs
            .iter()
            .map(|domain| DomainEntry {
                name: normalize(&domain.name),
                config: domain.clone(),
            })
            .collect();
        Ok(Self {
            server: config.api_base_url.clone(),
            model: config.model_name.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            client,
            prompts,
            domains,
            cache,
        })
    }

    /// The cached extraction result for `key`, or `None` on a miss (or when
    /// caching is disabled).
    fn cache_get(&self, key: &str) -> Result<Option<NerResult>, IngestionError> {
        let Some(cache) = &self.cache else {
            return Ok(None);
        };
        let stored = cache
            .with_conn(|conn| LlmNerCache::new(ConnectionOrTx::Connection(conn)).get(key))??;
        Ok(stored)
    }

    /// Stores `result` under `key` (a no-op when caching is disabled).
    ///
    /// A separate `with_conn` checkout from the read: the connection is
    /// held only for the INSERT, never across the HTTP round-trip.
    fn cache_set(&self, key: &str, result: &NerResult) -> Result<(), IngestionError> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        cache.with_conn(|conn| {
            LlmNerCache::new(ConnectionOrTx::Connection(conn)).set(key, result)
        })??;
        Ok(())
    }
}

impl NerProvider for LlmNer {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn extract_entities(
        &self,
        content: &str,
        metadata: &Map<String, Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        // Design D2: nothing to extract, no I/O.
        let normalized_content = content.trim();
        if normalized_content.is_empty() {
            return Ok(None);
        }

        let mut merged = NerResult::default();
        for domain in &self.domains {
            let system = self.prompts.render_system(&domain.config, true)?;
            let user = self
                .prompts
                .render_user(&domain.config, normalized_content, metadata)?;
            let key = build_cache_key(
                &self.server,
                &self.model,
                self.temperature,
                self.max_tokens,
                &system,
                &user,
            );

            // Cache check BEFORE the call (design D6): a hit skips HTTP.
            if let Some(cached) = self.cache_get(&key)? {
                let cached = tag_domain(cached, &domain.name);
                merged.entities.extend(cached.entities);
                merged.facts.extend(cached.facts);
                continue;
            }

            let raw = self.client.call(
                &system,
                &user,
                Some(&generate_json_schema(&domain.config)),
                Some("ner_result"),
            )?;
            let result =
                parse_llm_response(&raw).map_err(|source| IngestionError::LlmNerParse {
                    domain: domain.name.clone(),
                    source,
                })?;
            // Tag the domain before the cache write: the stored entry
            // carries the domain, and the composite stage's
            // source-metadata enrichment happens later (design D7).
            let result = tag_domain(result, &domain.name);
            self.cache_set(&key, &result)?;

            merged.entities.extend(result.entities);
            merged.facts.extend(result.facts);
        }

        // Design D2: an empty merge is "nothing found", not an empty result.
        if merged.entities.is_empty() && merged.facts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(merged))
        }
    }
}

/// Tags every entity and fact of `result` with `domain` (the parser leaves
/// the domain empty; the provider stamps it, and the re-tagging on a cache
/// hit is load-bearing: see the module docs).
fn tag_domain(mut result: NerResult, domain: &str) -> NerResult {
    for entity in &mut result.entities {
        entity.domain = domain.to_owned();
    }
    for fact in &mut result.facts {
        fact.domain = domain.to_owned();
    }
    result
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are valid and the
    // mock server is local).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use config::ontology::{EntityDef, ExtractionDef, RelationDef};
    use config::preset::ResponseFormat;
    use config::{ConfidencePolicy, DomainConfig};
    use db::test_util::in_memory_db;
    use llm::LlmError;
    use serde_json::json;

    use super::*;
    use crate::ner::{NerEntity, load_ner_prompts};

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// A valid config pointing at `base_url`; `max_retries = 0` keeps the
    /// error-path tests fast (no backoff sleeps).
    fn llm_config(base_url: &str) -> LlmConfig {
        LlmConfig {
            api_base_url: base_url.to_owned(),
            api_key: String::new(),
            model_name: "test-model".to_owned(),
            temperature: 0.0,
            max_tokens: 1024,
            seed: 0,
            response_format: ResponseFormat::JsonObject,
            timeout_ms: 5000,
            max_retries: 0,
        }
    }

    /// A small domain schema (one entity, one relation).
    fn domain(name: &str) -> DomainConfig {
        DomainConfig {
            name: name.to_owned(),
            version: "1".to_owned(),
            description: String::new(),
            entities: vec![EntityDef {
                id: "employee".to_owned(),
                name: "Employee".to_owned(),
                description: "A person".to_owned(),
                attributes: vec![],
                synonyms: vec![],
            }],
            relations: vec![RelationDef {
                source: "employee".to_owned(),
                predicate: "works_for".to_owned(),
                target: "company".to_owned(),
                description: "An employee works for a company".to_owned(),
                attributes: vec![],
            }],
            extraction: ExtractionDef::default(),
            confidence: ConfidencePolicy::default(),
        }
    }

    /// The model's canned NER response: one entity, one fact.
    const NER_JSON: &str = r#"{"entities":[{"name":"Alice","type":"employee","description":"A person.","attributes":{}}],"relations":[{"subject_name":"Alice","subject_type":"employee","predicate":"works_for","object_name":"Acme","object_type":"company","attributes":{}}]}"#;

    /// A chat-completions 200 envelope carrying `content`.
    fn success_body(content: &str) -> Vec<u8> {
        json!({"choices":[{"message":{"content":content},"finish_reason":"stop"}]})
            .to_string()
            .into_bytes()
    }

    /// The embedded-default prompts (no overrides on disk).
    fn prompts() -> NerPrompts {
        load_ner_prompts("/nonexistent-ner-prompts").unwrap()
    }

    /// Builds a provider against `server` with `JsonObject` mode.
    fn build_ner(server: &MockServer, domains: &[DomainConfig], cache: Option<Db>) -> LlmNer {
        LlmNer::new(&llm_config(&server.url), domains, prompts(), cache).unwrap()
    }

    // ── Mock OpenAI-compatible server (pattern: crates/llm tests) ──────────

    /// Minimal HTTP/1.1 server on 127.0.0.1 with an ephemeral port: records
    /// each request's JSON body and serves the handler's `(status, body)`
    /// with `Connection: close`. Keeps the tests network-free.
    struct MockServer {
        /// The base URL (`http://127.0.0.1:<port>`).
        url: String,
        /// Number of requests received.
        requests: Arc<AtomicUsize>,
        /// The JSON request bodies, in order.
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(handler: impl Fn(usize) -> (u16, Vec<u8>) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let (s_requests, s_bodies, s_shutdown) = (
                Arc::clone(&requests),
                Arc::clone(&bodies),
                Arc::clone(&shutdown),
            );
            let thread = std::thread::spawn(move || {
                loop {
                    if s_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let index = s_requests.fetch_add(1, Ordering::SeqCst);
                            handle_mock_connection(stream, index, &s_bodies, &handler);
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
                bodies,
                shutdown,
                thread: Some(thread),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }

        fn request_body(&self, i: usize) -> serde_json::Value {
            self.bodies.lock().unwrap()[i].clone()
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
    fn read_mock_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut received = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            if let Some(total) = expected_mock_request_len(&received)
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

    /// Total expected request length once the header block is complete;
    /// `None` while more header bytes are still needed.
    fn expected_mock_request_len(received: &[u8]) -> Option<usize> {
        let header_end = received
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)?;
        if received.len() < header_end {
            return None;
        }
        let content_length = parse_mock_content_length(&received[..header_end]).unwrap_or(0);
        Some(header_end + content_length)
    }

    fn parse_mock_content_length(headers: &[u8]) -> Option<usize> {
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

    fn handle_mock_connection(
        mut stream: TcpStream,
        index: usize,
        bodies: &Arc<Mutex<Vec<serde_json::Value>>>,
        handler: &impl Fn(usize) -> (u16, Vec<u8>),
    ) {
        let _ = stream.set_nonblocking(false);
        let raw = read_mock_request(&mut stream);
        if let Ok(text) = std::str::from_utf8(&raw)
            && let Some((_, body)) = text.split_once("\r\n\r\n")
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
        {
            bodies.lock().unwrap().push(value);
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

    // ── Constructor ─────────────────────────────────────────────────────────

    /// An empty domain config list is an error
    /// ([`IngestionError::LlmNerNoDomains`]).
    #[test]
    fn constructor_requires_at_least_one_domain() {
        let err = LlmNer::new(&llm_config("http://127.0.0.1:1"), &[], prompts(), None).unwrap_err();
        assert!(matches!(err, IngestionError::LlmNerNoDomains), "{err}");
    }

    /// Invalid client config fails fast (llm crate validation, design D10).
    #[test]
    fn constructor_rejects_invalid_llm_config() {
        let mut config = llm_config("http://127.0.0.1:1");
        config.api_base_url.clear();

        let err = LlmNer::new(&config, &[domain("alpha")], prompts(), None).unwrap_err();
        assert!(
            matches!(err, IngestionError::Llm(LlmError::Configuration(_))),
            "{err}"
        );
    }

    /// The provider is object-safe (`Box<dyn NerProvider>`, design D2) and
    /// reports its stable name.
    #[test]
    fn provider_is_object_safe_and_named() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let provider: Box<dyn NerProvider> = Box::new(build_ner(&server, &[domain("alpha")], None));
        assert_eq!(provider.name(), "llm");
    }

    /// Shared across threads (the pipeline runner keeps one instance).
    #[test]
    fn is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LlmNer>();
    }

    // ── Extraction ──────────────────────────────────────────────────────────

    /// Design D2: empty content → `Ok(None)` without any HTTP call.
    #[test]
    fn empty_content_short_circuits_without_http() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let ner = build_ner(&server, &[domain("alpha")], None);

        assert_eq!(ner.extract_entities("   ", &Map::new()).unwrap(), None);
        assert_eq!(server.request_count(), 0);
    }

    /// Multiple domains: one call per domain in config order, every
    /// entity/fact tagged with the normalized domain name.
    #[test]
    fn extract_calls_model_per_domain_and_tags_domain() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let ner = build_ner(&server, &[domain("Alpha"), domain("Beta")], None);

        let result = ner
            .extract_entities("Alice works at Acme.", &Map::new())
            .unwrap()
            .expect("entities found");

        assert_eq!(server.request_count(), 2, "one HTTP call per domain");
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.facts.len(), 2);
        assert_eq!(
            result.entities[0].domain, "alpha",
            "normalized (lowercased)"
        );
        assert_eq!(result.entities[1].domain, "beta", "config order preserved");
        assert_eq!(result.facts[0].domain, "alpha");
        assert_eq!(result.facts[1].domain, "beta");
        assert_eq!(result.entities[0].name, "Alice");
        assert_eq!(result.facts[0].predicate, "works_for");
    }

    /// Miss path: the model is called, the tagged result is stored, and a
    /// repeat of the same content is served from the cache (no second
    /// HTTP call).
    #[test]
    fn cache_miss_calls_model_then_repeat_is_served_from_cache() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let db = in_memory_db();
        let ner = build_ner(&server, &[domain("alpha")], Some(db.clone()));
        let metadata = Map::new();

        let first = ner
            .extract_entities("Alice works at Acme.", &metadata)
            .unwrap()
            .expect("entities found");
        assert_eq!(
            server.request_count(),
            1,
            "first extraction calls the model"
        );

        let rows: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM llm_ner_cache", [], |r| r.get(0))
            })
            .unwrap()
            .unwrap();
        assert_eq!(rows, 1, "the miss populated the cache table");

        let second = ner
            .extract_entities("Alice works at Acme.", &metadata)
            .unwrap()
            .expect("entities found");
        assert_eq!(
            server.request_count(),
            1,
            "second extraction is a cache hit"
        );
        assert_eq!(first, second);
    }

    /// Pre-seeded cache hit: no HTTP at all, and the stored (possibly
    /// stale) domain tag is re-tagged with this domain's name.
    #[test]
    fn pre_seeded_cache_hit_skips_http_and_re_tags_domain() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let db = in_memory_db();
        let d = domain("alpha");
        let content = "Alice works at Acme.";

        // Compute the same key the provider will: rendered prompts + config.
        let p = prompts();
        let system = p.render_system(&d, true).unwrap();
        let user = p.render_user(&d, content, &Map::new()).unwrap();
        let key = build_cache_key(&server.url, "test-model", 0.0, 1024, &system, &user);
        let seeded = NerResult {
            entities: vec![NerEntity {
                name: "Bob".to_owned(),
                entity_type: "employee".to_owned(),
                description: String::new(),
                confidence: 0.8,
                // A stale tag: the hit path must overwrite it.
                domain: "stale".to_owned(),
                metadata: Map::new(),
            }],
            facts: vec![],
        };
        db.with_conn(|conn| LlmNerCache::new(ConnectionOrTx::Connection(conn)).set(&key, &seeded))
            .unwrap()
            .unwrap();

        let ner = build_ner(&server, &[d], Some(db));
        let result = ner
            .extract_entities(content, &Map::new())
            .unwrap()
            .expect("cached entities found");

        assert_eq!(server.request_count(), 0, "cache hit must skip HTTP");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "Bob");
        assert_eq!(
            result.entities[0].domain, "alpha",
            "the hit re-tags the domain"
        );
    }

    /// Disabled cache (`None`): every extraction performs the HTTP call.
    #[test]
    fn disabled_cache_calls_model_every_time() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let ner = build_ner(&server, &[domain("alpha")], None);

        assert!(
            ner.extract_entities("Alice works at Acme.", &Map::new())
                .unwrap()
                .is_some()
        );
        assert!(
            ner.extract_entities("Alice works at Acme.", &Map::new())
                .unwrap()
                .is_some()
        );

        assert_eq!(server.request_count(), 2, "no cache: two calls");
    }

    /// A model error (5xx, budget exhausted) is fatal for the call
    /// (design D10) and propagates as [`IngestionError::Llm`].
    #[test]
    fn llm_error_propagates_as_err() {
        let server = MockServer::start(move |_| (500, b"boom".to_vec()));
        let ner = build_ner(&server, &[domain("alpha")], None);

        let err = ner
            .extract_entities("Alice works at Acme.", &Map::new())
            .unwrap_err();
        assert!(matches!(err, IngestionError::Llm(_)), "{err}");
        assert_eq!(server.request_count(), 1);
    }

    /// A 200 whose content is not the expected JSON maps to
    /// [`IngestionError::LlmNerParse`] at this boundary (design D10).
    #[test]
    fn unparseable_response_maps_to_parse_error() {
        let server = MockServer::start(move |_| (200, success_body("this is not json")));
        let ner = build_ner(&server, &[domain("alpha")], None);

        let err = ner
            .extract_entities("Alice works at Acme.", &Map::new())
            .unwrap_err();
        match err {
            IngestionError::LlmNerParse { domain, .. } => {
                assert_eq!(domain, "alpha", "the domain is named");
            }
            other => panic!("expected LlmNerParse, got {other}"),
        }
    }

    /// `json_schema` mode: the generated schema is embedded under the
    /// `ner_result` name (the client decides by `ResponseFormat`).
    #[test]
    fn json_schema_mode_embeds_ner_result_schema() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let mut config = llm_config(&server.url);
        config.response_format = ResponseFormat::JsonSchema;
        let ner = LlmNer::new(&config, &[domain("alpha")], prompts(), None).unwrap();

        assert!(
            ner.extract_entities("Alice works at Acme.", &Map::new())
                .unwrap()
                .is_some()
        );

        let body = server.request_body(0);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "ner_result");
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["type"],
            "object"
        );
        // The schema is generated from the domain config: the entity type
        // enum carries the domain's entity ids.
        let entity_enum = &body["response_format"]["json_schema"]["schema"]["properties"]["entities"]
            ["items"]["properties"]["type"]["enum"];
        assert!(
            entity_enum
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "employee")
        );
    }

    /// `json_object` mode: the schema arguments are ignored by the client.
    #[test]
    fn json_object_mode_sends_plain_json_object_format() {
        let server = MockServer::start(move |_| (200, success_body(NER_JSON)));
        let ner = build_ner(&server, &[domain("alpha")], None);

        assert!(
            ner.extract_entities("Alice works at Acme.", &Map::new())
                .unwrap()
                .is_some()
        );

        let body = server.request_body(0);
        assert_eq!(body["response_format"]["type"], "json_object");
        assert!(body["response_format"].get("json_schema").is_none());
    }
}
