//! CEL engine contract functions (design D2/D5, tasks 1.6-1.8): the
//! engine, the per-evaluation scope cache, and the six contract functions
//! over a fixed database (the four data functions and the two graph
//! functions).
//!
//! Oracle mapping: Go `internal/expression/{engine.go,scope_cache.go}` plus
//! the function registration in `internal/relations/expression_linker.go` —
//! a functional reference for behavior and contracts only, not a code
//! blueprint.

// Test code: unwrap/expect/panic are intentional (the fixtures are
// compile-time constants).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cel::objects::{Key as CelKey, Map as CelMap};
use cel::{Context, Value};
use db::test_util::in_memory_db;
use db::{
    ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, Entity, EntityDao, EntityLink,
    EntityLinkDao, Fact, FactDao,
};
use graph::{
    CelEngine, FactIndex, Graph, GraphError, ReachabilityIndex, ScopeCache, build_chunk_index,
    build_fact_index, build_reachability_index, register_data_functions, register_graph_functions,
};

/// Design D7: the engine, the scope cache and the cached program type
/// are shareable across threads (one engine behind an `Arc` for all MCP
/// handler threads).
#[test]
fn engine_is_send_sync_for_mcp_sharing() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CelEngine>();
    assert_send_sync::<ScopeCache>();
    assert_send_sync::<cel::Program>();
}

#[test]
fn trivial_expression_evaluates() {
    let engine = CelEngine::new();
    let program = engine.compile("1 + 1 == 2").unwrap();
    assert_eq!(engine.evaluate(&program, &[]).unwrap(), Value::Bool(true));
}

#[test]
fn expression_uses_variable_bindings() {
    let engine = CelEngine::new();
    let program = engine.compile("name == 'alpha' && count > 1").unwrap();
    let bindings = [
        ("name", Value::String(Arc::new("alpha".to_string()))),
        ("count", Value::Int(2)),
    ];
    assert_eq!(
        engine.evaluate(&program, &bindings).unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn parse_error_maps_to_graph_error() {
    let engine = CelEngine::new();
    let err = engine.compile("1 +").unwrap_err();
    assert!(matches!(err, GraphError::CelParse { .. }));
}

#[test]
fn compile_caches_programs_by_source() {
    let engine = CelEngine::new();
    let first = engine.compile("true").unwrap();
    let again = engine.compile("true").unwrap();
    let other = engine.compile("false").unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert!(!Arc::ptr_eq(&first, &other));
}

#[test]
fn unknown_function_is_an_explicit_error_not_a_panic() {
    let engine = CelEngine::new();
    // compile is parse-only: the unknown function is accepted here ...
    let program = engine.compile("no_such_function(1)").unwrap();
    // ... and rejected at evaluation with an explicit error.
    let err = engine.evaluate(&program, &[]).unwrap_err();
    match err {
        GraphError::CelEval { source } => {
            let message = source.to_string();
            assert!(
                message.contains("no_such_function"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected CelEval, got {other:?}"),
    }
}

#[test]
fn registered_function_receives_typed_arguments() {
    let mut engine = CelEngine::new();
    engine.register(|_scope, ctx| {
        // Return the primitive directly: a bare `Value` is not a
        // supported return type (only `Result<Value, ExecutionError>`).
        // The name avoids the built-in `int`/`double`/... conversion
        // functions, which shadow custom ones (see the next test).
        ctx.add_function("double_it", |value: i64| value * 2);
    });
    let program = engine.compile("double_it(21) == 42").unwrap();
    assert_eq!(engine.evaluate(&program, &[]).unwrap(), Value::Bool(true));
}

/// Built-in conversion functions (`int`, `double`, `string`, `size`, ...)
/// are resolved from the environment BEFORE the context's custom
/// function registry, so a custom function CANNOT shadow a built-in
/// name. Tasks 1.7/1.8 must pick contract-function names that do not
/// collide with the built-ins (the six contract names do not).
#[test]
fn builtin_names_shadow_custom_functions() {
    let mut engine = CelEngine::new();
    engine.register(|_scope, ctx| {
        // Registered, but must lose to the built-in `size`.
        ctx.add_function("size", |_value: Value| 999i64);
    });
    let program = engine.compile("size([1, 2])").unwrap();
    // The built-in `size` (list length) wins over the custom 999.
    assert_eq!(engine.evaluate(&program, &[]).unwrap(), Value::Int(2));
}

#[test]
fn function_can_return_a_list() {
    let mut engine = CelEngine::new();
    engine.register(|_scope, ctx| {
        ctx.add_function("ids", |entity: i64| {
            Arc::new(vec![Value::Int(entity), Value::Int(entity + 1)])
        });
    });
    let program = engine.compile("size(ids(1)) == 2").unwrap();
    assert_eq!(engine.evaluate(&program, &[]).unwrap(), Value::Bool(true));
}

/// Acceptance criterion: an index is NOT built if its function is never
/// called; when called twice in one evaluation it is built exactly once;
/// a fresh evaluation gets a fresh cache.
#[test]
fn indexes_are_lazy_cached_per_evaluation_and_shared_across_calls() {
    let builds = Arc::new(AtomicUsize::new(0));
    let builds_for_register = Arc::clone(&builds);
    let mut engine = CelEngine::new();
    engine.register(move |scope: Arc<ScopeCache>, ctx: &mut Context<'static>| {
        let builds = Arc::clone(&builds_for_register);
        ctx.add_function("probe", move |_entity: i64| {
            // Clone the Arc per call: the builder closure is created on
            // every call but must not move the closure's own captures.
            let builds = Arc::clone(&builds);
            scope
                .facts(move || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(FactIndex::from_rows(Vec::new(), Vec::new()))
                })
                .map_err(|err| cel::ExecutionError::function_error("probe", err.to_string()))?;
            Ok(Value::Bool(true))
        });
    });

    // Laziness: an expression that never calls the function must not
    // build the index.
    let idle = engine.compile("true").unwrap();
    assert_eq!(engine.evaluate(&idle, &[]).unwrap(), Value::Bool(true));
    assert_eq!(
        builds.load(Ordering::SeqCst),
        0,
        "index built although no function used it"
    );

    // Caching: two calls in one evaluation build the index exactly once.
    let probe = engine.compile("probe(1) && probe(2)").unwrap();
    assert_eq!(engine.evaluate(&probe, &[]).unwrap(), Value::Bool(true));
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "index rebuilt within one evaluation"
    );

    // Scope lifetime: a fresh evaluation gets a fresh cache.
    assert_eq!(engine.evaluate(&probe, &[]).unwrap(), Value::Bool(true));
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "scope cache outlived its evaluation"
    );
}

/// A builder failure (e.g. a db error) is bridged into CEL as a function
/// error and surfaces as `GraphError::CelEval` — never a panic.
#[test]
fn builder_failure_surfaces_as_cel_eval_error() {
    let mut engine = CelEngine::new();
    engine.register(|scope, ctx| {
        ctx.add_function("boom", move |_entity: i64| {
            scope
                .chunks(|| Err(GraphError::EmptyQuery { what: "chunks" }))
                .map_err(|err| cel::ExecutionError::function_error("boom", err.to_string()))?;
            Ok(Value::Bool(false))
        });
    });
    let program = engine.compile("boom(1)").unwrap();
    let err = engine.evaluate(&program, &[]).unwrap_err();
    match err {
        GraphError::CelEval { source } => {
            let message = source.to_string();
            assert!(message.contains("boom"), "unexpected message: {message}");
            assert!(
                message.contains("empty query"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected CelEval, got {other:?}"),
    }
}

// ── task 1.7: the data contract functions on a fixed database ─────

/// A fixed fixture: 4 entities, 4 facts (f3 made draft, f4 with a
/// `NULL` subject), one document with 3 chunks linked to entities.
/// Returns `(db, alice, bob, acme, eng, f1, f2, f3, f4)`.
fn fixture_db() -> (db::Db, i64, i64, i64, i64, i64, i64, i64, i64) {
    let db = in_memory_db();
    let (alice, bob, acme, eng, f1, f2, f3, f4) = db
        .with_conn(|conn| -> Result<_, db::DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let entities = EntityDao::new(exec);
            let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let bob = entities.create("PERSON", "Bob", "hr", None, None, None)?;
            let acme = entities.create("ORGANIZATION", "Acme", "hr", None, None, None)?;
            let eng = entities.create("DEPARTMENT", "Engineering", "hr", None, None, None)?;

            let docs = DocumentDao::new(exec);
            let doc = docs.create("markdown", "/fixture/doc.md", None, None)?;
            let chunks = ChunkDao::new(exec);
            let c0 = chunks.create(doc, "Alice works at Acme", 0, None, None)?;
            let c1 = chunks.create(doc, "Bob manages Engineering", 1, None, None)?;
            let c2 = chunks.create(doc, "The quick brown fox", 2, None, None)?;
            let links = ChunkEntityDao::new(exec);
            links.link(c0, alice)?;
            links.link(c1, bob)?;
            links.link(c2, alice)?;

            let facts = FactDao::new(exec);
            let f1 = facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
            let f2 = facts.create(Some(alice), "manages", Some(eng), "hr", None, None, None)?;
            // Draft: must not enter the index (approved only).
            let f3 = facts.create(Some(bob), "knows", Some(alice), "hr", None, None, None)?;
            // NULL subject: attached to the object endpoint only.
            let f4 = facts.create(None, "located_in", Some(acme), "geo", None, None, None)?;
            Ok((alice, bob, acme, eng, f1, f2, f3, f4))
        })
        .unwrap()
        .unwrap();
    db.with_conn(|conn| {
        conn.execute("UPDATE facts SET status = 'draft' WHERE id = ?", [f3])
            .unwrap();
    })
    .unwrap();
    (db, alice, bob, acme, eng, f1, f2, f3, f4)
}

/// An engine with the four data functions registered over `db`.
fn fixture_engine(db: &db::Db) -> CelEngine {
    let mut engine = CelEngine::new();
    register_data_functions(&mut engine, Arc::new(db.clone()));
    engine
}

/// Compile + evaluate a single expression.
fn eval(engine: &CelEngine, source: &str) -> Value {
    let program = engine.compile(source).unwrap();
    engine.evaluate(&program, &[]).unwrap()
}

/// The expected `facts` list element (see the module docs for the keys).
fn fact_value(id: i64, predicate: &str, domain: &str, value: &str) -> Value {
    let mut map = HashMap::with_capacity(4);
    map.insert(CelKey::String(Arc::new("id".to_string())), Value::Int(id));
    map.insert(
        CelKey::String(Arc::new("predicate".to_string())),
        Value::String(Arc::new(predicate.to_string())),
    );
    map.insert(
        CelKey::String(Arc::new("domain".to_string())),
        Value::String(Arc::new(domain.to_string())),
    );
    map.insert(
        CelKey::String(Arc::new("value".to_string())),
        Value::String(Arc::new(value.to_string())),
    );
    Value::Map(CelMap { map: Arc::new(map) })
}

fn as_bool(value: Value) -> bool {
    match value {
        Value::Bool(flag) => flag,
        other => panic!("expected bool, got {other:?}"),
    }
}

/// `facts(e)`: the approved facts attached to the entity (subject OR
/// object side), in id order, as maps `{id, predicate, domain, value}`.
#[test]
fn facts_returns_the_entity_facts_as_maps() {
    let (db, alice, ..) = fixture_db();
    let engine = fixture_engine(&db);

    let value = eval(&engine, &format!("facts({alice})"));
    assert_eq!(
        value,
        Value::List(Arc::new(vec![
            fact_value(1, "works_at", "hr", "Acme"),
            fact_value(2, "manages", "hr", "Engineering"),
        ]))
    );
}

/// A fact with a `NULL` subject is attached to the object endpoint only.
#[test]
fn facts_attached_to_both_endpoints() {
    let (db, _, _, acme, ..) = fixture_db();
    let engine = fixture_engine(&db);

    // Acme: f1 (as object) and f4 (as object, NULL subject).
    let value = eval(&engine, &format!("facts({acme})"));
    assert_eq!(
        value,
        Value::List(Arc::new(vec![
            fact_value(1, "works_at", "hr", "Acme"),
            fact_value(4, "located_in", "geo", "Acme"),
        ]))
    );
}

/// A missing entity yields the EMPTY list, not an error (oracle parity).
#[test]
fn facts_missing_entity_is_empty_not_error() {
    let (db, ..) = fixture_db();
    let engine = fixture_engine(&db);
    let value = eval(&engine, "facts(999999)");
    assert_eq!(value, Value::List(Arc::new(Vec::new())));
}

/// `has_fact(e, k, v)`: predicate == k AND object entity name == v
/// (the Go bug fix). Draft facts never match; a missing entity is
/// false, not an error.
#[test]
fn has_fact_matches_predicate_and_object_name() {
    let (db, alice, bob, acme, ..) = fixture_db();
    let engine = fixture_engine(&db);

    assert!(as_bool(eval(
        &engine,
        &format!("has_fact({alice}, 'works_at', 'Acme')")
    )));
    // Attached on the object side (NULL-subject fact).
    assert!(as_bool(eval(
        &engine,
        &format!("has_fact({acme}, 'located_in', 'Acme')")
    )));
    // Wrong value.
    assert!(!as_bool(eval(
        &engine,
        &format!("has_fact({alice}, 'works_at', 'Beta')")
    )));
    // Wrong predicate (alice has no located_in fact; acme does).
    assert!(!as_bool(eval(
        &engine,
        &format!("has_fact({alice}, 'located_in', 'Acme')")
    )));
    // Draft fact (bob knows alice) must not match.
    assert!(!as_bool(eval(
        &engine,
        &format!("has_fact({bob}, 'knows', 'Alice')")
    )));
    // Missing entity: false, not an error.
    assert!(!as_bool(eval(
        &engine,
        "has_fact(999999, 'works_at', 'Acme')"
    )));
}

/// `chunks(e)`: the chunk texts in `sequence_num` order.
#[test]
fn chunks_returns_texts_in_sequence_order() {
    let (db, alice, bob, ..) = fixture_db();
    let engine = fixture_engine(&db);

    let alice_chunks = eval(&engine, &format!("chunks({alice})"));
    assert_eq!(
        alice_chunks,
        Value::List(Arc::new(vec![
            Value::String(Arc::new("Alice works at Acme".to_string())),
            Value::String(Arc::new("The quick brown fox".to_string())),
        ]))
    );
    let bob_chunks = eval(&engine, &format!("chunks({bob})"));
    assert_eq!(
        bob_chunks,
        Value::List(Arc::new(vec![Value::String(Arc::new(
            "Bob manages Engineering".to_string()
        ))]))
    );
}

/// A missing entity yields the EMPTY list, not an error (oracle parity).
#[test]
fn chunks_missing_entity_is_empty_not_error() {
    let (db, ..) = fixture_db();
    let engine = fixture_engine(&db);
    let value = eval(&engine, "chunks(999999)");
    assert_eq!(value, Value::List(Arc::new(Vec::new())));
}

/// `chunk_contains(e, t)`: exact, case-SENSITIVE substring over the
/// chunk texts (oracle parity: Go `strings.Contains`).
#[test]
fn chunk_contains_is_exact_case_sensitive_substring() {
    let (db, alice, bob, ..) = fixture_db();
    let engine = fixture_engine(&db);

    assert!(as_bool(eval(
        &engine,
        &format!("chunk_contains({alice}, 'quick brown')")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("chunk_contains({alice}, 'works at Acme')")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("chunk_contains({bob}, 'manages')")
    )));
    // Case-sensitive.
    assert!(!as_bool(eval(
        &engine,
        &format!("chunk_contains({alice}, 'Quick')")
    )));
    // No such text.
    assert!(!as_bool(eval(
        &engine,
        &format!("chunk_contains({alice}, 'no such text')")
    )));
    // Missing entity: false, not an error.
    assert!(!as_bool(eval(&engine, "chunk_contains(999999, 'x')")));
}

/// The returned maps are consumable in expressions: field access, the
/// built-in `exists`/`all` macros and `size` — so the 1-arg `facts(e)`
/// loses no filtering power versus the oracle's 2-arg overload.
#[test]
fn returned_maps_are_consumable_in_expressions() {
    let (db, alice, ..) = fixture_db();
    let engine = fixture_engine(&db);

    let expr = format!(
        "size(facts({alice})) == 2 \
         && facts({alice}).exists(f, f.predicate == 'manages' && f.value == 'Engineering') \
         && facts({alice}).all(f, f.domain == 'hr')"
    );
    assert_eq!(eval(&engine, &expr), Value::Bool(true));
}

/// A db failure during the lazy index build surfaces as
/// `GraphError::CelEval` from ALL FOUR data functions (deviation from
/// the oracle: `has_fact`/`chunk_contains` must not silently return
/// false).
#[test]
fn storage_failure_surfaces_as_cel_eval_error() {
    let (db, alice, ..) = fixture_db();
    let engine = fixture_engine(&db);

    // Break the storage of BOTH indexes: the facts index reads `facts`,
    // the chunk index joins `chunk_entities` (drop the child table —
    // FK-safe; the build query then fails with "no such table").
    db.with_conn(|conn| {
        conn.execute("DROP TABLE facts", []).unwrap();
        conn.execute("DROP TABLE chunk_entities", []).unwrap();
    })
    .unwrap();

    for source in [
        format!("facts({alice})"),
        format!("has_fact({alice}, 'works_at', 'Acme')"),
        format!("chunks({alice})"),
        format!("chunk_contains({alice}, 'works at')"),
    ] {
        let program = engine.compile(&source).unwrap();
        let err = engine.evaluate(&program, &[]).unwrap_err();
        match err {
            GraphError::CelEval { source } => {
                let message = source.to_string();
                assert!(
                    message.contains("Error executing function"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected CelEval for {source}, got {other:?}"),
        }
    }
}

/// The index is built ONCE per evaluation and shared: repeated accessors
/// on the same slot return the same `Arc`.
#[test]
fn indexes_are_built_once_and_shared_per_evaluation() {
    let (db, ..) = fixture_db();
    let scope = Arc::new(ScopeCache::new());

    let first = scope.facts(|| build_fact_index(&db)).unwrap();
    let second = scope.facts(|| build_fact_index(&db)).unwrap();
    assert!(
        Arc::ptr_eq(&first, &second),
        "facts index rebuilt within one scope"
    );

    let c_first = scope.chunks(|| build_chunk_index(&db)).unwrap();
    let c_second = scope.chunks(|| build_chunk_index(&db)).unwrap();
    assert!(
        Arc::ptr_eq(&c_first, &c_second),
        "chunk index rebuilt within one scope"
    );

    let r_first = scope
        .reachability(|| build_reachability_index(&db))
        .unwrap();
    let r_second = scope
        .reachability(|| build_reachability_index(&db))
        .unwrap();
    assert!(
        Arc::ptr_eq(&r_first, &r_second),
        "reachability index rebuilt within one scope"
    );
}

// ── task 1.8: the graph contract functions ───────────────────────────

/// Row builders for the pure (SQLite-free) index tests.
fn entity(id: i64, domain: &str) -> Entity {
    Entity {
        id,
        entity_type: "PERSON".into(),
        name: format!("E{id}"),
        domain: domain.into(),
        description: None,
        confidence: None,
        metadata_json: None,
        created_at: String::new(),
    }
}

fn fact(id: i64, subject: i64, object: i64) -> Fact {
    Fact {
        id,
        subject_entity_id: Some(subject),
        predicate: "knows".into(),
        object_entity_id: Some(object),
        domain: String::new(),
        metadata_json: None,
        status: "approved".into(),
        valid_from: None,
        valid_to: None,
        weight: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn link(subject: i64, target: i64) -> EntityLink {
    EntityLink {
        subject_entity_id: subject,
        target_entity_id: target,
        relation_type: "same_entity".into(),
        method: "rule".into(),
        confidence: 0.9,
        evidence: None,
    }
}

/// `neighbors`: both directions, ALL edge kinds (fact + link),
/// deduplicated, ascending; a missing entity is empty, not an error.
#[test]
fn neighbors_is_both_directions_deduplicated_sorted() {
    // 1→2 fact, 3→1 fact, 1→4 cross-domain fact, 1→5 link, 4→1 link
    // (the pair 1–4 has a fact AND a link: deduplicated to one id).
    let graph = Graph::from_rows(
        vec![
            entity(1, "hr"),
            entity(2, "hr"),
            entity(3, "hr"),
            entity(4, "it"),
            entity(5, "it"),
        ],
        vec![fact(1, 1, 2), fact(2, 3, 1), fact(3, 1, 4)],
        vec![link(1, 5), link(4, 1)],
    );
    let index = ReachabilityIndex::new(graph);

    assert_eq!(index.neighbors(1), vec![2, 3, 4, 5]);
    assert_eq!(index.neighbors(2), vec![1]);
    assert_eq!(index.neighbors(3), vec![1]);
    assert_eq!(index.neighbors(4), vec![1]);
    assert_eq!(
        index.neighbors(999),
        Vec::<i64>::new(),
        "missing entity → empty, not an error"
    );
}

/// `path_exists`: connected / disconnected pairs, identity, missing
/// entities.
#[test]
fn path_exists_connected_disconnected_and_identity() {
    // chain 1→2→3 (hr) plus an isolated node 4 (it).
    let graph = Graph::from_rows(
        vec![
            entity(1, "hr"),
            entity(2, "hr"),
            entity(3, "hr"),
            entity(4, "it"),
        ],
        vec![fact(1, 1, 2), fact(2, 2, 3)],
        Vec::new(),
    );
    let index = ReachabilityIndex::new(graph);

    assert!(index.path_exists(1, 3, 2), "chain reached within 2");
    assert!(index.path_exists(3, 1, 2), "BFS is bidirectional");
    assert!(!index.path_exists(1, 4, 5), "disconnected pair");
    assert!(!index.path_exists(4, 1, 5), "disconnected pair (reverse)");
    assert!(index.path_exists(1, 1, 0), "identity is a zero-length path");
    assert!(!index.path_exists(999, 999, 5), "missing entity → false");
    assert!(!index.path_exists(1, 999, 5), "missing target → false");
}

/// `path_exists`: `max_depth` is respected and clamped to the D4 hard
/// max (10).
#[test]
fn path_exists_respects_and_clamps_max_depth() {
    // 12-node chain (hr): the distance 1→12 is 11 edges.
    let graph = Graph::from_rows(
        (1..=12).map(|id| entity(id, "hr")).collect(),
        (1..12).map(|id| fact(id, id, id + 1)).collect(),
        Vec::new(),
    );
    let index = ReachabilityIndex::new(graph);

    assert!(!index.path_exists(1, 12, 10), "distance 11 > depth 10");
    assert!(
        !index.path_exists(1, 12, 100),
        "depth 100 is clamped to the hard max (10) < distance 11"
    );
    assert!(!index.path_exists(1, 12, 0), "depth 0 is identity only");
    assert!(index.path_exists(1, 11, 10), "distance 10 == hard max");
}

/// D4 boundary (the same rule as traverse with link crossing enabled):
/// a cross-domain FACT edge is never traversed, an ENTITY-LINK edge may
/// cross domains, and the boundary is relative to the START entity's
/// domain — once crossed, fact edges inside the crossed domain are
/// blocked too.
#[test]
fn path_exists_applies_the_d4_boundary_rule() {
    // f2 crosses hr→it; the links 1→4→5 cross domains.
    let graph = Graph::from_rows(
        vec![
            entity(1, "hr"),
            entity(2, "hr"),
            entity(3, "it"),
            entity(4, "it"),
            entity(5, "it"),
        ],
        vec![fact(1, 1, 2), fact(2, 1, 3)],
        vec![link(1, 4), link(4, 5)],
    );
    let index = ReachabilityIndex::new(graph);

    assert!(index.path_exists(1, 2, 1), "in-domain fact edge");
    assert!(
        !index.path_exists(1, 3, 5),
        "the cross-domain fact edge is never traversed"
    );
    assert!(
        index.path_exists(1, 4, 1),
        "an entity link may cross domains"
    );
    assert!(index.path_exists(1, 5, 2), "two link hops across domains");
    assert!(!index.path_exists(1, 5, 1), "depth 1 reaches only 4");

    // The same rule from the crossed side: 1→2 via the link is allowed,
    // but 2→3 via the fact (inside the crossed domain) is not; starting
    // from 2, that fact edge is in-domain and is traversed.
    let crossed = Graph::from_rows(
        vec![entity(1, "hr"), entity(2, "it"), entity(3, "it")],
        vec![fact(1, 2, 3)],
        vec![link(1, 2)],
    );
    let index = ReachabilityIndex::new(crossed);
    assert!(
        !index.path_exists(1, 3, 2),
        "fact edge in the crossed domain"
    );
    assert!(index.path_exists(2, 3, 1), "in-domain when starting from 2");
}

/// A two-domain fixture with facts and entity links (the graph-function
/// counterpart of `fixture_db`). Returns `(db, alice, bob, carol, acme,
/// globex, dave)`.
fn graph_fixture_db() -> (db::Db, i64, i64, i64, i64, i64, i64) {
    let db = in_memory_db();
    let (alice, bob, carol, acme, globex, dave) = db
        .with_conn(|conn| -> Result<_, db::DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let entities = EntityDao::new(exec);
            let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
            let bob = entities.create("PERSON", "Bob", "hr", None, None, None)?;
            let carol = entities.create("PERSON", "Carol", "hr", None, None, None)?;
            let acme = entities.create("ORGANIZATION", "Acme", "it", None, None, None)?;
            let globex = entities.create("ORGANIZATION", "Globex", "it", None, None, None)?;
            let dave = entities.create("PERSON", "Dave", "geo", None, None, None)?;

            let facts = FactDao::new(exec);
            facts.create(Some(alice), "knows", Some(bob), "hr", None, None, None)?;
            facts.create(Some(bob), "knows", Some(carol), "hr", None, None, None)?;
            facts.create(
                Some(carol),
                "reports_to",
                Some(alice),
                "hr",
                None,
                None,
                None,
            )?;
            // Cross-domain fact edge: adjacency only, never traversed.
            facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;

            let links = EntityLinkDao::new(exec);
            links.create(&EntityLink {
                subject_entity_id: alice,
                target_entity_id: globex,
                relation_type: "same_entity".into(),
                method: "rule".into(),
                confidence: 0.95,
                evidence: None,
            })?;
            links.create(&EntityLink {
                subject_entity_id: acme,
                target_entity_id: alice,
                relation_type: "related_to".into(),
                method: "equals".into(),
                confidence: 0.8,
                evidence: None,
            })?;
            Ok((alice, bob, carol, acme, globex, dave))
        })
        .unwrap()
        .unwrap();
    (db, alice, bob, carol, acme, globex, dave)
}

/// An engine with the two graph functions registered over `db`.
fn graph_engine(db: &db::Db) -> CelEngine {
    let mut engine = CelEngine::new();
    register_graph_functions(&mut engine, Arc::new(db.clone()));
    engine
}

/// Acceptance: `neighbors` is correct for BOTH directions over all edge
/// kinds (fact + link), deduplicated and ascending.
#[test]
fn cel_neighbors_is_correct_for_both_directions() {
    let (db, alice, bob, carol, acme, globex, ..) = graph_fixture_db();
    let engine = graph_engine(&db);

    // Outgoing: bob (fact), acme (cross-domain fact), globex (link).
    // Incoming: carol (fact), acme (link) — deduplicated with the fact.
    let value = eval(&engine, &format!("neighbors({alice})"));
    assert_eq!(
        value,
        Value::List(Arc::new(vec![
            Value::Int(bob),
            Value::Int(carol),
            Value::Int(acme),
            Value::Int(globex),
        ]))
    );
    // Pure incoming side (the acme→alice link).
    let value = eval(&engine, &format!("neighbors({acme})"));
    assert_eq!(value, Value::List(Arc::new(vec![Value::Int(alice)])));
    // Both directions on bob: alice (in), carol (out).
    let value = eval(&engine, &format!("neighbors({bob})"));
    assert_eq!(
        value,
        Value::List(Arc::new(vec![Value::Int(alice), Value::Int(carol)]))
    );
    // Missing entity: empty list, not an error.
    assert_eq!(
        eval(&engine, "neighbors(999999)"),
        Value::List(Arc::new(Vec::new()))
    );
}

/// Acceptance: `path_exists` is true on connected pairs (both
/// directions, facts and links) and false on disconnected ones.
#[test]
fn cel_path_exists_true_false_on_connected_disconnected_pairs() {
    let (db, alice, bob, carol, _acme, globex, dave) = graph_fixture_db();
    let engine = graph_engine(&db);

    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({alice}, {carol}, 2)")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({carol}, {bob}, 2)")
    )));
    // Cross-domain via the entity link (a cross-domain FACT edge alone
    // would never cross).
    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({alice}, {globex}, 1)")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({globex}, {alice}, 1)")
    )));
    // Disconnected (dave has no edges at all). Identity and missing
    // entities are pinned at the index level.
    assert!(!as_bool(eval(
        &engine,
        &format!("path_exists({alice}, {dave}, 5)")
    )));
    assert!(!as_bool(eval(
        &engine,
        &format!("path_exists({dave}, {alice}, 5)")
    )));
}

/// Acceptance: `max_depth` is respected at the CEL surface (the depth-0
/// and hard-max clamp edge cases are pinned at the index level).
#[test]
fn cel_path_exists_respects_max_depth() {
    let (db, _alice, bob, carol, acme, globex, ..) = graph_fixture_db();
    let engine = graph_engine(&db);

    // carol→acme needs exactly 2 hops: carol→alice (fact), then
    // alice→acme via the INCOMING link (the cross-domain FACT edge alone
    // would be blocked).
    assert!(!as_bool(eval(
        &engine,
        &format!("path_exists({carol}, {acme}, 1)")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({carol}, {acme}, 2)")
    )));
    // bob→globex needs exactly 2 hops: bob→alice (fact), alice→globex
    // (link).
    assert!(!as_bool(eval(
        &engine,
        &format!("path_exists({bob}, {globex}, 1)")
    )));
    assert!(as_bool(eval(
        &engine,
        &format!("path_exists({bob}, {globex}, 2)")
    )));
}

/// A db failure during the lazy reachability build surfaces as
/// `GraphError::CelEval` from BOTH graph functions (the same rule as the
/// data functions: a broken database must not silently produce "no
/// link" decisions).
#[test]
fn graph_storage_failure_surfaces_as_cel_eval_error() {
    let (db, alice, bob, ..) = graph_fixture_db();
    let engine = graph_engine(&db);

    db.with_conn(|conn| {
        conn.execute("DROP TABLE entity_links", []).unwrap();
    })
    .unwrap();

    for source in [
        format!("neighbors({alice})"),
        format!("path_exists({alice}, {bob}, 2)"),
    ] {
        let program = engine.compile(&source).unwrap();
        let err = engine.evaluate(&program, &[]).unwrap_err();
        match err {
            GraphError::CelEval { source } => {
                let message = source.to_string();
                assert!(
                    message.contains("Error executing function"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected CelEval for {source}, got {other:?}"),
        }
    }
}
