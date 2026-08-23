//! End-to-end LLM linking pipeline (llm change, task 2.3 acceptance): the
//! ontology is loaded from a real `global.xml` (config crate, methods
//! `equals` + `expression` + `llm`), the pipeline runs over a two-domain
//! database against a content-aware mock LLM server, and the mock's request
//! counter proves the decision cache (no re-calls on the identical re-run)
//! and the template-hash cache invalidation (a changed `user.tmpl` override
//! re-consults the model for the same pairs).
//!
//! The three scenarios are one sequential test: scenario 2 asserts on the
//! cache state written by scenario 1, and scenario 3 asserts on the cache
//! state written by scenario 2.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use config::ontology::{LinkMethod, load_global_config};
use config::preset::{LinkerConfig, LlmConfig, ResponseFormat};
use db::test_util::in_memory_db;
use db::{ConnectionOrTx, EntityDao, EntityLinkDao, FactDao};
use graph::build_entity_links;

/// A nonexistent prompts path: the `llm` method falls back to the embedded
/// templates (the normal case, design D3).
const EMBEDDED_PROMPTS_PATH: &str = "/nonexistent/prompts";

/// The `user.tmpl` override written by the template-invalidation scenario: a
/// different source changes the template hash and therefore the decision
/// cache key (design D4).
const OVERRIDE_USER_TEMPLATE: &str = "Override: compare {{ entity_a.name }} ({{ entity_a.domain }}) \
     with {{ entity_b.name }} ({{ entity_b.domain }}).\n";

fn ontology_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/llm-linker-ontology")
}

/// Loads the fixture ontology and requires its `<cross-domain-links>` block.
fn links_config() -> config::ontology::CrossDomainLinksConfig {
    let global = load_global_config(ontology_dir())
        .expect("fixture global.xml must load")
        .expect("fixture must carry a cross-domain-links block");
    global
        .cross_domain_links
        .expect("fixture must carry a cross-domain-links block")
}

/// The two-domain fixture database:
/// - "Acme" (ORGANIZATION) in `hr` and `it` — one word: too short for
///   `equals` (min-words 2), so only the `llm` method can link it;
/// - "John Doe" (PERSON) in `hr` and `it` — the equals pair AND the
///   expression pair (John in `hr` works at Acme).
///
/// Returns the entity ids in insertion order.
fn fixture_db() -> (db::Db, Vec<i64>) {
    let db = in_memory_db();
    let ids = db
        .with_conn(|conn| -> Result<Vec<i64>, db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let mut ids: Vec<i64> = Vec::new();
            for (entity_type, name, domain) in [
                ("ORGANIZATION", "Acme", "hr"),
                ("ORGANIZATION", "Acme", "it"),
                ("PERSON", "John Doe", "hr"),
                ("PERSON", "John Doe", "it"),
            ] {
                ids.push(entities.create(entity_type, name, domain, None, None, None)?);
            }
            let facts = FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(ids[2]),
                "works_at",
                Some(ids[0]),
                "hr",
                None,
                None,
                None,
            )?;
            Ok(ids)
        })
        .unwrap()
        .unwrap();
    (db, ids)
}

fn all_links(db: &db::Db) -> Vec<db::EntityLink> {
    db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap()
}

/// The `llm_link_*` decision-cache entries (key, value) in `app_kv`.
fn cached_decisions(db: &db::Db) -> Vec<(String, String)> {
    db.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT key, value FROM app_kv WHERE key LIKE 'llm_link%'")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .unwrap()
}

/// A `LinkerConfig` pointing the LLM client at `base_url` (no retries: one
/// attempt per pair, so the mock's request count is deterministic).
fn llm_linker_config(base_url: &str) -> LinkerConfig {
    LinkerConfig {
        disabled: false,
        llm: LlmConfig {
            api_base_url: base_url.to_owned(),
            api_key: String::new(),
            model_name: "test-model".to_owned(),
            temperature: 0.0,
            max_tokens: 256,
            seed: 1,
            response_format: ResponseFormat::JsonObject,
            timeout_ms: 5000,
            max_retries: 0,
        },
    }
}

// ── Mock LLM server ────────────────────────────────────────────────────────
//
// The `crates/llm` / `linker.rs` TcpListener pattern (keeps CI network-free),
// extended to be content-aware: it routes on the rendered user prompt, so the
// Acme pair gets a `same_entity: true` decision and every other pair a
// `same_entity: false` one. Every request is counted and captured raw.

struct MockLlm {
    url: String,
    requests: Arc<AtomicUsize>,
    captured: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockLlm {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (thread_requests, thread_captured, thread_shutdown) = (
            Arc::clone(&requests),
            Arc::clone(&captured),
            Arc::clone(&shutdown),
        );
        let thread = thread::spawn(move || {
            loop {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        handle_mock_request(stream, &thread_captured);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
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

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The rendered `user` prompt (messages[1].content) of request `i`.
    fn user_message(&self, i: usize) -> String {
        user_prompt_of(self.captured.lock().unwrap()[i].as_bytes()).unwrap()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop polls the shutdown flag, so the join returns
        // promptly.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serve one connection: read the full request, record it, and answer with
/// the decision routed on the user prompt.
fn handle_mock_request(mut stream: std::net::TcpStream, captured: &Mutex<Vec<String>>) {
    let _ = stream.set_nonblocking(false);
    let mut received = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        if let Some(total) = mock_request_len(&received)
            && received.len() >= total
        {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => received.extend_from_slice(&buffer[..n]),
        }
    }
    if let Ok(text) = std::str::from_utf8(&received) {
        captured.lock().unwrap().push(text.to_string());
    }
    // Route on the rendered user prompt: the Acme pair matches, the rest not.
    let user = user_prompt_of(&received).unwrap_or_default();
    let (same_entity, confidence, reasoning) = if user.contains("Acme") {
        (true, 0.95, "acme match")
    } else {
        (false, 0.9, "different people")
    };
    let content = serde_json::json!({
        "same_entity": same_entity,
        "confidence": confidence,
        "reasoning": reasoning,
    })
    .to_string();
    let body = serde_json::json!({
        "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    // Let the client drain the response before the socket is closed.
    thread::sleep(Duration::from_millis(25));
}

/// The rendered `user` prompt (messages[1].content) of one raw HTTP request.
fn user_prompt_of(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let body = text.split_once("\r\n\r\n")?.1;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value["messages"][1]["content"].as_str().map(str::to_owned)
}

/// Total expected request length (headers + body) once the header block is
/// complete; `None` while more header bytes are still needed.
fn mock_request_len(received: &[u8]) -> Option<usize> {
    let header_end = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)?;
    let headers = std::str::from_utf8(&received[..header_end]).ok()?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        })
        .unwrap_or(0);
    Some(header_end + content_length)
}

/// A temp dir unique to this test (the repo's established pattern:
/// `temp_dir()` + `process::id()` + a per-test name suffix). Removed first so
/// a stale override from an earlier run cannot leak into this one.
fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graph-llm-e2e-override-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── The three scenarios (sequential: each builds on the previous state) ────

#[test]
fn end_to_end_llm_linking_caching_and_template_invalidation() {
    let config = links_config();
    assert_eq!(
        config.methods,
        vec![LinkMethod::Equals, LinkMethod::Expression, LinkMethod::Llm],
        "the fixture applies equals, then expression, then llm"
    );
    assert_eq!(
        config.llm_confidence_threshold, 0.7,
        "the loader must apply the oracle default threshold"
    );

    let (db, ids) = fixture_db();
    let server = MockLlm::start();
    let linker = llm_linker_config(&server.url);

    // ── Scenario 1: full run with llm enabled ─────────────────────────────
    // equals skips Acme (one word < min-words 2) and links John; expression
    // links John (works_at Acme); llm consults the model for BOTH pairs.
    let first =
        build_entity_links(&db, &config, &linker, EMBEDDED_PROMPTS_PATH).expect("first run");
    assert!(
        first.errors.is_empty(),
        "first run must not error: {:?}",
        first.errors
    );
    assert_eq!(
        first.links_created, 3,
        "equals: John; expression: John; llm: Acme"
    );
    assert_eq!(
        first.links_skipped, 1,
        "llm: John decided not the same (cached, no link)"
    );

    let links = all_links(&db);
    assert_eq!(
        links.len(),
        6,
        "John equals (2) + John expression (2) + Acme llm (2)"
    );

    let equals_rows: Vec<&db::EntityLink> = links
        .iter()
        .filter(|link| link.method == "equals")
        .collect();
    assert_eq!(equals_rows.len(), 2, "the John pair, bidirectional");
    for link in &equals_rows {
        assert_eq!(link.relation_type, "same_entity");
        assert!((link.confidence - 0.9).abs() < f64::EPSILON);
        assert!(
            (link.subject_entity_id, link.target_entity_id) == (ids[2], ids[3])
                || (link.subject_entity_id, link.target_entity_id) == (ids[3], ids[2])
        );
    }

    let expression_rows: Vec<&db::EntityLink> = links
        .iter()
        .filter(|link| link.method == "expression")
        .collect();
    assert_eq!(expression_rows.len(), 2, "the John pair, bidirectional");
    for link in &expression_rows {
        assert_eq!(link.relation_type, "works_at_same_org");
        assert!((link.confidence - 1.0).abs() < f64::EPSILON);
        assert_eq!(link.evidence.as_deref(), Some("expression: acme_workers"));
    }

    // The llm link: created for the pair the mock answers same_entity=true
    // about (Acme), with the model's confidence and reasoning.
    let llm_rows: Vec<&db::EntityLink> = links.iter().filter(|link| link.method == "llm").collect();
    assert_eq!(llm_rows.len(), 2, "the Acme pair, bidirectional");
    for link in &llm_rows {
        assert_eq!(link.relation_type, "same_entity");
        assert!((link.confidence - 0.95).abs() < f64::EPSILON);
        assert_eq!(link.evidence.as_deref(), Some("acme match"));
        assert!(
            (link.subject_entity_id, link.target_entity_id) == (ids[0], ids[1])
                || (link.subject_entity_id, link.target_entity_id) == (ids[1], ids[0])
        );
    }

    // One LLM call per candidate pair (Acme + John), with the embedded
    // templates.
    assert_eq!(server.request_count(), 2, "one LLM call per candidate pair");
    assert!(
        server.user_message(0).contains("Name: Acme"),
        "the first call must carry the Acme pair's embedded-template prompt"
    );

    // Both decisions are cached in app_kv — including the negative one.
    let entries = cached_decisions(&db);
    assert_eq!(
        entries.len(),
        2,
        "both decisions cached (including the negative one)"
    );
    let decisions: Vec<serde_json::Value> = entries
        .iter()
        .map(|(_, value)| serde_json::from_str(value).unwrap())
        .collect();
    let matched = decisions.iter().find(|d| d["same_entity"] == true).unwrap();
    assert_eq!(matched["confidence"], 0.95);
    assert_eq!(matched["reasoning"], "acme match");
    let rejected = decisions
        .iter()
        .find(|d| d["same_entity"] == false)
        .unwrap();
    assert_eq!(rejected["confidence"], 0.9);
    assert_eq!(rejected["reasoning"], "different people");

    // ── Scenario 2: identical re-run — idempotent, no new LLM calls ───────
    let second =
        build_entity_links(&db, &config, &linker, EMBEDDED_PROMPTS_PATH).expect("second run");
    assert!(
        second.errors.is_empty(),
        "second run must not error: {:?}",
        second.errors
    );
    assert_eq!(second.links_created, 0, "no duplicates on the re-run");
    assert_eq!(
        second.links_skipped, 4,
        "equals John + expression John + both llm pairs served from cache"
    );
    assert_eq!(all_links(&db).len(), 6, "the row count is unchanged");
    assert_eq!(
        server.request_count(),
        2,
        "cache hit: no second LLM call for either pair"
    );

    // ── Scenario 3: template override invalidates the decision cache ──────
    // A changed `user.tmpl` source changes the template hash and therefore
    // the cache key (design D4): the same pairs must be re-consulted.
    let prompts_dir = temp_dir();
    std::fs::create_dir_all(prompts_dir.join("entity-linker")).unwrap();
    std::fs::write(
        prompts_dir.join("entity-linker").join("user.tmpl"),
        OVERRIDE_USER_TEMPLATE,
    )
    .unwrap();

    let third = build_entity_links(&db, &config, &linker, &prompts_dir.to_string_lossy())
        .expect("third run");
    assert!(
        third.errors.is_empty(),
        "third run must not error: {:?}",
        third.errors
    );
    assert_eq!(
        third.links_created, 0,
        "the rows already exist: no duplicates"
    );
    assert_eq!(
        third.links_skipped, 4,
        "equals John + expression John + both llm pairs (re-cached decisions)"
    );
    assert_eq!(all_links(&db).len(), 6, "the row count is unchanged");
    assert_eq!(
        server.request_count(),
        4,
        "changed template hash: both pairs re-consult the model"
    );
    // The re-cached decisions live under the new (template-hash) keys.
    assert_eq!(
        cached_decisions(&db).len(),
        4,
        "two new cache entries under the changed template hashes"
    );
    // The override was loaded and rendered for the re-calls.
    assert!(
        third
            .notes
            .iter()
            .any(|note| note.contains("prompt user") && note.contains("user.tmpl")),
        "the loaded override must be noted: {:?}",
        third.notes
    );
    assert!(
        server.user_message(2).starts_with("Override:"),
        "the re-call must carry the override-rendered prompt"
    );
}
