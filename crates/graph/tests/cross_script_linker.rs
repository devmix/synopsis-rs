//! Within-domain cross-script linking pipeline (change
//! multilingual-entity-resolution, task 7.2, design D7): entities of the same
//! (domain, type) written in different dominant scripts are judged by the
//! `llm` method ONLY, and the decision is acted on — merge at/above the merge
//! threshold (default 0.95), link at/above the link threshold (0.7), no
//! action below (the decision is cached either way).
//!
//! Scenarios (generic names, content-aware stub LLM — the `llm_linker_pipeline`
//! pattern):
//! - (a) confidence 0.97 → merged, canonical = the entity with more source
//!   documents, both names aliased, dependent fact re-pointed;
//! - (a, tie-breakers) equal sources → longer name; equal length → lower id;
//! - (b) confidence 0.8 → `same_entity` link, no merge;
//! - (c) confidence 0.5 → no action, decision cached;
//! - (d) same-script pair → not generated (no LLM call);
//! - (e) pair the resolution tiers already settle (shared tier-1/tier-2 key,
//!   same script) → not generated (no LLM call);
//! - (f) repeat run → cached decision, no second LLM call, no duplicate
//!   link/merge;
//! - (g) per-pair stub failure → recorded, pipeline continues;
//! - (h) `linker.disabled=true` → no cross-script actioning;
//! - (i) stale candidate: the first merge deletes a shared member of the
//!   second pair, whose merge then hits the `merge_entities` precondition —
//!   recorded, pipeline continues.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use config::ontology::{CrossDomainLinksConfig, LinkMethod};
use config::preset::{LinkerConfig, LlmConfig, ResponseFormat};
use db::test_util::in_memory_db;
use db::{
    ConnectionOrTx, Db, DocumentDao, EntityAliasDao, EntityDao, EntityLink, EntityLinkDao,
    EntitySourceDao, FactDao,
};
use graph::{LinkResult, build_entity_links};

/// A nonexistent prompts path: the `llm` method falls back to the embedded
/// templates (the normal case, design D3).
const EMBEDDED_PROMPTS_PATH: &str = "/nonexistent/prompts";

/// An `llm`-only links config with the default link threshold (0.7) and the
/// default merge threshold (`None` → the linker's 0.95).
fn links_config() -> CrossDomainLinksConfig {
    CrossDomainLinksConfig {
        methods: vec![LinkMethod::Llm],
        equals: None,
        llm_confidence_threshold: 0.7,
        merge_confidence_threshold: None,
        batch_size: 5,
        expressions: Vec::new(),
    }
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
            reasoning_effort: String::new(),
        },
    }
}

/// Run the `llm`-only pipeline once over `db` (with the optional cache DB).
fn run(db: &Db, cache: Option<&Db>, linker: &LinkerConfig) -> LinkResult {
    build_entity_links(
        db,
        cache,
        &links_config(),
        linker,
        EMBEDDED_PROMPTS_PATH,
        None,
    )
    .unwrap()
}

/// Insert (type, name, domain) entities and return their ids in order.
fn insert_entities(db: &Db, rows: &[(&str, &str, &str)]) -> Vec<i64> {
    db.with_conn(|conn| {
        let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
        rows.iter()
            .map(|(entity_type, name, domain)| {
                entities.create(entity_type, name, domain, None, None, None)
            })
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap()
    .unwrap()
}

/// Link `count` distinct documents to `entity_id` (the canonical-selection
/// input, design D7).
fn seed_sources(db: &Db, entity_id: i64, count: i64) {
    db.with_conn(|conn| -> Result<(), db::DbError> {
        let documents = DocumentDao::new(ConnectionOrTx::Connection(conn));
        let sources = EntitySourceDao::new(ConnectionOrTx::Connection(conn));
        for i in 0..count {
            let document =
                documents.create("text", &format!("doc-{entity_id}-{i}.txt"), None, None)?;
            sources.create(entity_id, document)?;
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
}

/// The entity_links rows, in insertion (rowid) order.
fn all_links(db: &Db) -> Vec<EntityLink> {
    db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap()
}

/// The (id, name) of every entity, in id order.
fn entity_rows(db: &Db) -> Vec<(i64, String)> {
    let mut rows = db
        .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|entity| (entity.id, entity.name))
        .collect::<Vec<_>>();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// The aliases recorded for one entity, ordered by alias.
fn aliases_of(db: &Db, entity_id: i64) -> Vec<String> {
    db.with_conn(|conn| EntityAliasDao::new(ConnectionOrTx::Connection(conn)).aliases_of(entity_id))
        .unwrap()
        .unwrap()
}

/// The decision-cache entries (key, value) in `llm_linker_cache` on the cache
/// database (task 1.10).
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

// ── Mock LLM server ────────────────────────────────────────────────────────
//
// The `crates/llm` / `linker.rs` TcpListener pattern (keeps CI network-free),
// extended to be content-aware: the first route whose marker occurs in the
// rendered user prompt answers the request with its own decision (or an
// invalid body, for the per-pair failure scenario). Every request is counted.

/// One mock route: matched when the rendered user prompt contains `marker`.
/// `valid` false answers with a non-decision body (a strict parse error).
#[derive(Clone, Copy)]
struct Route {
    marker: &'static str,
    same_entity: bool,
    confidence: f64,
    reasoning: &'static str,
    valid: bool,
}

struct MockLlm {
    url: String,
    requests: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockLlm {
    fn start(routes: &[Route]) -> Self {
        let routes = routes.to_vec();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (thread_requests, thread_shutdown) = (Arc::clone(&requests), Arc::clone(&shutdown));
        let thread = thread::spawn(move || {
            loop {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        handle_mock_request(stream, &routes);
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
            shutdown,
            thread: Some(thread),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
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

/// Serve one connection: read the full request, answer with the routed body.
fn handle_mock_request(mut stream: std::net::TcpStream, routes: &[Route]) {
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
    let user = user_prompt_of(&received).unwrap_or_default();
    let body = routes
        .iter()
        .find(|route| user.contains(route.marker))
        .map(|route| {
            if route.valid {
                decision_body(route.same_entity, route.confidence, route.reasoning)
            } else {
                invalid_body()
            }
        })
        .unwrap_or_else(invalid_body);
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

/// A chat-completions envelope whose content is the given decision JSON.
fn decision_body(same_entity: bool, confidence: f64, reasoning: &str) -> String {
    let content = serde_json::json!({
        "same_entity": same_entity,
        "confidence": confidence,
        "reasoning": reasoning,
    })
    .to_string();
    serde_json::json!({
        "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
    })
    .to_string()
}

/// A 200 envelope whose content is NOT the decision JSON (strict parse error
/// for the pair, non-fatal for the run).
fn invalid_body() -> String {
    serde_json::json!({
        "choices": [{ "message": { "content": "not a decision" } }],
    })
    .to_string()
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

// ── The scenarios (a)–(h) ──────────────────────────────────────────────────

/// (a) confidence 0.97 ≥ the merge threshold (0.95): the pair is merged, the
/// canonical is the entity with MORE source documents, both names are
/// aliased, and the dependent fact is re-pointed.
#[test]
fn merge_above_threshold_more_sources_survives() {
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let (latin_id, cyrillic_id) = (ids[0], ids[1]);
    seed_sources(&db, latin_id, 2);
    seed_sources(&db, cyrillic_id, 1);
    let fact_id = db
        .with_conn(|conn| {
            FactDao::new(ConnectionOrTx::Connection(conn)).create(
                Some(cyrillic_id),
                "located_in",
                None,
                "hr",
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap();

    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.97,
        reasoning: "alpha match",
        valid: true,
    }]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(result.entities_merged, 1);
    assert_eq!(result.links_created, 0);
    assert_eq!(result.links_skipped, 0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    assert_eq!(server.request_count(), 1, "one LLM call for the pair");

    // The canonical (more sources) survives; the duplicate row is deleted.
    assert_eq!(
        entity_rows(&db),
        vec![(latin_id, "Alpha Site".to_owned())],
        "the duplicate must be gone"
    );
    // Both names are recorded as aliases of the survivor (ordered by alias).
    assert_eq!(
        aliases_of(&db, latin_id),
        vec!["Alpha Site".to_owned(), "Альфа Сайт".to_owned()]
    );
    // The dependent fact is re-pointed onto the survivor.
    let subject: i64 = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT subject_entity_id FROM facts WHERE id = ?",
                [fact_id],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(subject, latin_id, "the fact must follow the survivor");
}

/// (a, tie-breakers) equal source counts: the LONGER name wins; on an equal
/// name length the LOWER id wins.
#[test]
fn merge_tie_breakers_longer_name_then_lower_id() {
    // Tie 1: equal sources (1 each), different lengths (10 vs 5 chars) → the
    // longer name ("Alpha Site") survives over "Альфа".
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа", "hr"),
        ],
    );
    let (long_id, short_id) = (ids[0], ids[1]);
    seed_sources(&db, long_id, 1);
    seed_sources(&db, short_id, 1);
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.97,
        reasoning: "alpha match",
        valid: true,
    }]);
    let result = run(&db, None, &llm_linker_config(&server.url));
    assert_eq!(result.entities_merged, 1);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    assert_eq!(
        entity_rows(&db),
        vec![(long_id, "Alpha Site".to_owned())],
        "tie: the longer name survives"
    );

    // Tie 2: equal sources (1 each), equal lengths (5 chars each) → the lower
    // id survives.
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha", "hr"),
            ("ORGANIZATION", "Альфа", "hr"),
        ],
    );
    let (low_id, high_id) = (ids[0], ids[1]);
    seed_sources(&db, low_id, 1);
    seed_sources(&db, high_id, 1);
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.97,
        reasoning: "alpha match",
        valid: true,
    }]);
    let result = run(&db, None, &llm_linker_config(&server.url));
    assert_eq!(result.entities_merged, 1);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    assert_eq!(
        entity_rows(&db),
        vec![(low_id, "Alpha".to_owned())],
        "tie on length: the lower id survives"
    );
}

/// (b) confidence 0.8, between the link threshold (0.7) and the merge
/// threshold (0.95): a `same_entity` link is created, no merge happens.
#[test]
fn link_between_thresholds_no_merge() {
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.8,
        reasoning: "alpha match",
        valid: true,
    }]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(result.links_created, 1);
    assert_eq!(result.entities_merged, 0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    assert_eq!(server.request_count(), 1);

    let links = all_links(&db);
    assert_eq!(links.len(), 2, "one bidirectional pair");
    for link in &links {
        assert_eq!(link.method, "llm");
        assert_eq!(link.relation_type, "same_entity");
        assert!((link.confidence - 0.8).abs() < f64::EPSILON);
        assert_eq!(link.evidence.as_deref(), Some("alpha match"));
        assert!(
            (link.subject_entity_id, link.target_entity_id) == (ids[0], ids[1])
                || (link.subject_entity_id, link.target_entity_id) == (ids[1], ids[0])
        );
    }
    // No merge: both entities survive, no aliases recorded.
    assert_eq!(entity_rows(&db).len(), 2);
    assert!(aliases_of(&db, ids[0]).is_empty());
}

/// (c) confidence 0.5, below the link threshold (0.7): no action, but the
/// decision is still cached.
#[test]
fn below_threshold_no_action_cached() {
    let db = in_memory_db();
    insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.5,
        reasoning: "uncertain",
        valid: true,
    }]);
    let cache = in_memory_db();
    let result = run(&db, Some(&cache), &llm_linker_config(&server.url));

    assert_eq!(result.links_created, 0);
    assert_eq!(result.links_skipped, 1);
    assert_eq!(result.entities_merged, 0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    assert!(all_links(&db).is_empty());
    assert_eq!(entity_rows(&db).len(), 2, "no merge");

    // The below-threshold decision is still cached (task 1.10).
    let entries = cached_decisions(&cache);
    assert_eq!(entries.len(), 1);
    let decision: serde_json::Value = serde_json::from_str(&entries[0].1).unwrap();
    assert_eq!(decision["same_entity"], true);
    assert_eq!(decision["confidence"], 0.5);
}

/// (d) a same-script pair is not generated as a cross-script candidate: no
/// LLM call at all.
#[test]
fn same_script_pairs_not_generated() {
    let db = in_memory_db();
    insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Beta Site", "hr"),
        ],
    );
    let server = MockLlm::start(&[]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(
        server.request_count(),
        0,
        "no LLM call for same-script pairs"
    );
    assert_eq!(result.links_created, 0);
    assert_eq!(result.entities_merged, 0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
}

/// (e) pairs the resolution tiers already settle (same script, shared
/// tier-1/tier-2 key) are not cross-script candidates: no LLM call.
#[test]
fn tier_resolved_pairs_not_generated() {
    let db = in_memory_db();
    insert_entities(
        &db,
        &[
            // Tier-1 shared: the leading article is stripped.
            ("PERSON", "The City of Ash", "hr"),
            ("PERSON", "City of Ash", "hr"),
            // Tier-2 shared: the Porter stem of "gates" is "gate".
            ("PERSON", "Gates", "it"),
            ("PERSON", "Gate", "it"),
        ],
    );
    let server = MockLlm::start(&[]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(
        server.request_count(),
        0,
        "no LLM call for tier-resolved pairs"
    );
    assert_eq!(result.links_created, 0);
    assert_eq!(result.entities_merged, 0);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
}

/// (f) repeat run: the cached decision applies — no second LLM call, no
/// duplicate link (after a merge the pair no longer exists, so nothing is
/// re-merged either).
#[test]
fn repeat_run_cached_no_duplicate_link_or_merge() {
    // Link case: the second run hits the cache, the rows already exist.
    let db = in_memory_db();
    insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.8,
        reasoning: "alpha match",
        valid: true,
    }]);
    let cache = in_memory_db();
    let linker = llm_linker_config(&server.url);

    let first = run(&db, Some(&cache), &linker);
    assert_eq!(first.links_created, 1);
    assert_eq!(server.request_count(), 1);

    let second = run(&db, Some(&cache), &linker);
    assert_eq!(second.links_created, 0, "no duplicate link");
    assert_eq!(
        second.links_skipped, 1,
        "cached decision, row already exists"
    );
    assert!(second.errors.is_empty(), "errors: {:?}", second.errors);
    assert_eq!(all_links(&db).len(), 2, "still one bidirectional pair");
    assert_eq!(server.request_count(), 1, "cache hit: no second LLM call");

    // Merge case: the second run has no pair left to judge.
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let (latin_id, cyrillic_id) = (ids[0], ids[1]);
    seed_sources(&db, latin_id, 2);
    seed_sources(&db, cyrillic_id, 1);
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.97,
        reasoning: "alpha match",
        valid: true,
    }]);
    let cache = in_memory_db();
    let linker = llm_linker_config(&server.url);

    let first = run(&db, Some(&cache), &linker);
    assert_eq!(first.entities_merged, 1);
    assert_eq!(server.request_count(), 1);

    let second = run(&db, Some(&cache), &linker);
    assert_eq!(second.entities_merged, 0, "no re-merge of a deleted pair");
    assert_eq!(second.links_created, 0);
    assert!(second.errors.is_empty(), "errors: {:?}", second.errors);
    assert_eq!(
        server.request_count(),
        1,
        "the pair is gone: no second LLM call"
    );
    assert_eq!(entity_rows(&db), vec![(latin_id, "Alpha Site".to_owned())]);
}

/// (g) a per-pair stub failure is recorded in the result and the pipeline
/// continues with the remaining pairs.
#[test]
fn pair_failure_recorded_pipeline_continues() {
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let (alpha_latin, alpha_cyrillic) = (ids[0], ids[1]);
    insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Beta Site", "it"),
            ("ORGANIZATION", "Бета Сайт", "it"),
        ],
    );
    // The Alpha pair gets a valid 0.8 decision (→ link); the Beta pair gets
    // an invalid body (→ a recorded parse error).
    let server = MockLlm::start(&[
        Route {
            marker: "Alpha",
            same_entity: true,
            confidence: 0.8,
            reasoning: "alpha match",
            valid: true,
        },
        Route {
            marker: "Beta",
            same_entity: true,
            confidence: 0.8,
            reasoning: "unreachable",
            valid: false,
        },
    ]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(server.request_count(), 2, "both pairs were consulted");
    assert_eq!(result.links_created, 1, "the Alpha pair was linked");
    assert_eq!(result.entities_merged, 0);
    assert!(
        result
            .errors
            .iter()
            .any(|err| err.starts_with("llm pair (") && err.contains("invalid decision JSON")),
        "the failed Beta pair must be recorded: {:?}",
        result.errors
    );
    // The pipeline is alive: the Alpha pair's bidirectional link exists (the
    // failed Beta pair created none).
    let links = all_links(&db);
    assert_eq!(links.len(), 2);
    assert!(
        links.iter().all(|link| {
            (link.subject_entity_id, link.target_entity_id) == (alpha_latin, alpha_cyrillic)
                || (link.subject_entity_id, link.target_entity_id) == (alpha_cyrillic, alpha_latin)
        }),
        "only the Alpha pair may be linked: {links:?}"
    );
}

/// (h) `linker.disabled=true` excludes the `llm` method entirely: no
/// cross-script actioning, no LLM call.
#[test]
fn linker_disabled_no_cross_script_actioning() {
    let db = in_memory_db();
    insert_entities(
        &db,
        &[
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
        ],
    );
    let server = MockLlm::start(&[Route {
        marker: "Alpha",
        same_entity: true,
        confidence: 0.97,
        reasoning: "alpha match",
        valid: true,
    }]);
    let disabled = LinkerConfig {
        disabled: true,
        ..llm_linker_config(&server.url)
    };

    let result = run(&db, None, &disabled);

    assert_eq!(server.request_count(), 0, "disabled: no LLM call");
    assert_eq!(result.links_created, 0);
    assert_eq!(result.entities_merged, 0);
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("excluded by linker.disabled"))
    );
    assert_eq!(entity_rows(&db).len(), 2, "both entities survive");
}

/// (i) stale candidate: two cross-script pairs share a member (the middle id),
/// and the first merge — the higher-confidence pair — deletes it. The second
/// pair's merge then hits the `merge_entities` precondition (the entity is
/// gone): the error is recorded and the pipeline continues without panicking.
#[test]
fn stale_candidate_after_first_merge_recorded() {
    let db = in_memory_db();
    let ids = insert_entities(
        &db,
        &[
            // The cross-script pairs are (1, 2) and (2, 3): the middle
            // (Cyrillic) entity is shared by both.
            ("ORGANIZATION", "Alpha Site", "hr"),
            ("ORGANIZATION", "Альфа Сайт", "hr"),
            ("ORGANIZATION", "Gamma Site", "hr"),
        ],
    );
    let (alpha_id, shared_id, gamma_id) = (ids[0], ids[1], ids[2]);
    // The first pair's canonical is the entity with MORE sources (Alpha), so
    // the merge deletes the shared member.
    seed_sources(&db, alpha_id, 2);
    seed_sources(&db, shared_id, 1);
    let server = MockLlm::start(&[
        Route {
            marker: "Alpha",
            same_entity: true,
            confidence: 0.97,
            reasoning: "alpha match",
            valid: true,
        },
        Route {
            marker: "Gamma",
            same_entity: true,
            confidence: 0.96,
            reasoning: "gamma match",
            valid: true,
        },
    ]);
    let result = run(&db, None, &llm_linker_config(&server.url));

    assert_eq!(
        server.request_count(),
        2,
        "both pairs were consulted (the pipeline continued)"
    );
    assert_eq!(result.entities_merged, 1, "only the first pair merged");
    assert_eq!(result.links_created, 0);
    assert_eq!(result.links_skipped, 1, "the stale pair counts as skipped");
    assert_eq!(
        result.errors.len(),
        1,
        "exactly one error: {:?}",
        result.errors
    );
    assert!(
        result.errors[0].starts_with(&format!("llm pair ({shared_id} <-> {gamma_id}): "))
            && result.errors[0].contains("merge precondition violated")
            && result.errors[0].contains(&format!("entity {shared_id} does not exist")),
        "the stale pair's precondition failure must be recorded: {:?}",
        result.errors
    );

    // Final state: the shared member is gone, the survivor keeps its aliases,
    // the third entity is untouched, and no links were created.
    assert_eq!(
        entity_rows(&db),
        vec![
            (alpha_id, "Alpha Site".to_owned()),
            (gamma_id, "Gamma Site".to_owned()),
        ]
    );
    assert_eq!(
        aliases_of(&db, alpha_id),
        vec!["Alpha Site".to_owned(), "Альфа Сайт".to_owned()]
    );
    assert!(all_links(&db).is_empty());
}
