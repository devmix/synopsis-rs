//! End-to-end LLM linking pipeline (llm change, task 2.3 + task 1.10): the
//! ontology is loaded from a real `global.xml` (config crate, methods
//! `equals` + `expression` + `llm`), the pipeline runs against a content-aware
//! mock LLM server, and the mock's request counter proves the decision cache
//! lives in a SEPARATE cache database (`llm_linker_cache`, keyed by the LLM
//! request signature) and survives a rebuilt knowledge database.
//!
//! The three scenarios are one sequential test sharing ONE cache database:
//! - scenario 1 runs on knowledge DB #1 and writes the decisions to the cache;
//! - scenario 2 runs on a FRESH knowledge DB #2 (same entities, the cache is
//!   shared) and must NOT re-consult the model (cache hit);
//! - scenario 3 changes the `user.tmpl` override, which changes the rendered
//!   user prompt and therefore the request-signature key, so the same pairs
//!   are re-consulted.

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
use db::{ConnectionOrTx, Db, EntityDao, EntityLinkDao, FactDao};
use graph::build_entity_links;

/// A nonexistent prompts path: the `llm` method falls back to the embedded
/// templates (the normal case, design D3).
const EMBEDDED_PROMPTS_PATH: &str = "/nonexistent/prompts";

/// The `user.tmpl` override written by the template-invalidation scenario: a
/// different source changes the rendered user prompt and therefore the
/// request-signature cache key (task 1.10).
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

/// The two-domain fixture database (a FRESH knowledge database per call):
/// - "Acme" (ORGANIZATION) in `hr` and `it` — one word: too short for
///   `equals` (min-words 2), so only the `llm` method can link it;
/// - "John Doe" (PERSON) in `hr` and `it` — the equals pair AND the
///   expression pair (John in `hr` works at Acme).
///
/// Returns the entity ids in insertion order.
fn fixture_db() -> (Db, Vec<i64>) {
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

fn all_links(db: &Db) -> Vec<db::EntityLink> {
    db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap()
}

/// The decision-cache entries (key, value) in `llm_linker_cache` on the cache
/// database (task 1.10). The key is a bare sha256 hex digest (the request
/// signature); the value is the serialized decision.
fn cached_decisions(cache: &Db) -> Vec<(String, String)> {
    cache
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT cache_key, decision FROM llm_linker_cache")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .unwrap()
}

/// The `llm_link*` keys that must NOT exist in `app_kv` on the knowledge DB
/// (task 1.10: decisions moved to `llm_linker_cache`; only `last_linking_run`
/// may remain in `app_kv`, and that is written by the ingestion runner, not
/// the graph crate).
///
/// The prefix filter is applied in Rust (not a SQL `LIKE`) so this assertion
/// does not reintroduce the legacy "decisions in app_kv" query shape.
fn app_kv_llm_rows(db: &Db) -> Vec<String> {
    db.with_conn(|conn| -> Result<Vec<String>, db::DbError> {
        // The knowledge DB does not even carry an `app_kv` table (task 1.9);
        // a query against it must yield nothing, not an error.
        let has_table: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'app_kv'",
            [],
            |r| r.get(0),
        )?;
        if has_table == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare("SELECT key FROM app_kv")?;
        let keys: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(keys
            .into_iter()
            .filter(|key| key.starts_with("llm_link"))
            .collect())
    })
    .unwrap()
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

// ── The three scenarios (sequential: all share ONE cache database) ─────────

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
        "the loader must apply the default threshold"
    );

    // The SHARED cache database (task 1.10): it outlives both knowledge
    // databases, so a rebuilt knowledge DB still hits the decisions.
    let cache = in_memory_db();
    let server = MockLlm::start();
    let linker = llm_linker_config(&server.url);

    // ── Scenario 1: full run on knowledge DB #1 ────────────────────────────
    // equals skips Acme (one word < min-words 2) and links John; expression
    // links John (works_at Acme); llm consults the model for BOTH pairs.
    let (db1, ids1) = fixture_db();
    let first = build_entity_links(&db1, Some(&cache), &config, &linker, EMBEDDED_PROMPTS_PATH)
        .expect("first run");
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

    let links = all_links(&db1);
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
            (link.subject_entity_id, link.target_entity_id) == (ids1[2], ids1[3])
                || (link.subject_entity_id, link.target_entity_id) == (ids1[3], ids1[2])
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
            (link.subject_entity_id, link.target_entity_id) == (ids1[0], ids1[1])
                || (link.subject_entity_id, link.target_entity_id) == (ids1[1], ids1[0])
        );
    }

    // One LLM call per candidate pair (Acme + John), with the embedded
    // templates.
    assert_eq!(server.request_count(), 2, "one LLM call per candidate pair");
    assert!(
        server.user_message(0).contains("Name: Acme"),
        "the first call must carry the Acme pair's embedded-template prompt"
    );

    // Both decisions are cached in `llm_linker_cache` on the cache DB —
    // including the negative one. The key is a bare sha256 hex digest.
    let entries = cached_decisions(&cache);
    assert_eq!(
        entries.len(),
        2,
        "both decisions cached (including the negative one)"
    );
    for (key, _) in &entries {
        assert_eq!(key.len(), 64, "the key is a bare sha256 hex digest: {key}");
    }
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

    // No decision leaked into `app_kv` on the knowledge DB (task 1.10).
    assert!(
        app_kv_llm_rows(&db1).is_empty(),
        "decisions must live in llm_linker_cache, not app_kv"
    );

    // ── Scenario 2: FRESH knowledge DB #2, SAME cache DB ───────────────────
    // The knowledge database is rebuilt from scratch (the production shape
    // after a full re-ingest): the decisions are NOT in it, but they are in
    // the shared cache DB. The request-signature key carries no entity IDs or
    // dataset, so the identical rendered prompts still hit the cache — no new
    // LLM calls. equals/expression re-create the John rows on the fresh DB.
    let (db2, _ids2) = fixture_db();
    let second = build_entity_links(&db2, Some(&cache), &config, &linker, EMBEDDED_PROMPTS_PATH)
        .expect("second run");
    assert!(
        second.errors.is_empty(),
        "second run must not error: {:?}",
        second.errors
    );
    assert_eq!(
        second.links_created, 3,
        "the fresh DB re-creates equals + expression + the cached llm link"
    );
    assert_eq!(second.links_skipped, 1, "the cached negative llm decision");
    assert_eq!(
        all_links(&db2).len(),
        6,
        "the fresh DB has the same six rows"
    );
    assert_eq!(
        server.request_count(),
        2,
        "cache hit across a rebuilt knowledge DB: no second LLM call for either pair"
    );

    // ── Scenario 3: template override invalidates the decision cache ───────
    // A changed `user.tmpl` source changes the rendered user prompt and
    // therefore the request-signature key (task 1.10): the same pairs must be
    // re-consulted.
    let prompts_dir = temp_dir();
    std::fs::create_dir_all(prompts_dir.join("entity-linker")).unwrap();
    std::fs::write(
        prompts_dir.join("entity-linker").join("user.tmpl"),
        OVERRIDE_USER_TEMPLATE,
    )
    .unwrap();

    let third = build_entity_links(
        &db2,
        Some(&cache),
        &config,
        &linker,
        &prompts_dir.to_string_lossy(),
    )
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
    assert_eq!(all_links(&db2).len(), 6, "the row count is unchanged");
    assert_eq!(
        server.request_count(),
        4,
        "changed prompt: both pairs re-consult the model"
    );
    // The re-cached decisions live under the new (rendered-prompt) keys.
    assert_eq!(
        cached_decisions(&cache).len(),
        4,
        "two new cache entries under the changed rendered prompts"
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
