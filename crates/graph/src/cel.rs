//! CEL engine for ontology linking rules (design D2/D5, task 1.6).
//!
//! Oracle mapping: `../synopsis/internal/expression/{engine.go,scope_cache.go}`
//! plus the function registration in
//! `../synopsis/internal/relations/expression_linker.go` — a functional copy,
//! re-architected for Rust (migration principle: not a code copy).
//!
//! # What this module provides
//!
//! - [`CelEngine`] — compiles CEL expression sources ONCE (cached by source
//!   string) and evaluates them on the `cel` crate (0.14, design D2).
//! - A function registration framework ([`CelEngine::register`]) that tasks
//!   1.7/1.8 use to install the six contract functions: `facts`, `has_fact`,
//!   `chunks`, `chunk_contains`, `neighbors`, `path_exists`. The engine
//!   hardcodes none of them.
//! - The four data-backed contract functions (task 1.7) with their lazy
//!   indexes: [`build_fact_index`] / [`FactIndex`] serving `facts` and
//!   `has_fact`, and [`build_chunk_index`] / [`ChunkIndex`] serving `chunks`
//!   and `chunk_contains`. [`register_data_functions`] installs all four on
//!   an engine in one call.
//! - The two graph-backed contract functions (task 1.8): `neighbors` and
//!   `path_exists` over the in-memory graph, served by the lazy
//!   [`ReachabilityIndex`] slot. [`register_graph_functions`] installs both
//!   on an engine in one call.
//! - [`ScopeCache`] — per-evaluation lazy indexes (the Rust re-design of the
//!   oracle's `scope_cache.go`): heavy data is built by the first function
//!   call that needs it and shared by every later call in the same
//!   evaluation.
//!
//! # Threading model (design D7)
//!
//! [`CelEngine`] and [`ScopeCache`] are `Send + Sync` (asserted in tests).
//! The engine is meant to be shared behind an `Arc` — one per MCP server,
//! read by many handler threads. Compiled programs live in a mutex-guarded
//! map; [`CelEngine::evaluate`] builds its scope cache and context on the
//! CALLING thread, so concurrent evaluations share no mutable state.
//! Contract functions run synchronously on the calling thread: their SQLite
//! access is sync (db crate DAOs), and the caller of `evaluate` decides when
//! to hop to `spawn_blocking` (design D5).
//!
//! # Adding a contract function (tasks 1.7/1.8)
//!
//! Consumers install functions through [`CelEngine::register`]. The installer
//! receives the evaluation's `Arc<ScopeCache>` and the fresh root
//! [`Context`]; it registers closures with `Context::add_function`, capturing
//! the scope cache (and any other long-lived state, e.g. an `Arc<db::Db>`)
//! in the closure body. Closures must be `'static + Send + Sync`, so `Arc`
//! is the way to share state — cel 0.14 has no other channel for function
//! state (`FunctionContext::ptx` is an immutable `&Context`):
//!
//! ```
//! use std::sync::Arc;
//!
//! use cel::{Context, Value};
//! use graph::{CelEngine, FactIndex, ScopeCache};
//!
//! let mut engine = CelEngine::new();
//! engine.register(|scope: Arc<ScopeCache>, ctx: &mut Context| {
//!     // `scope` is the per-evaluation cache; heavy data is built lazily
//!     // inside it, once per evaluation.
//!     ctx.add_function("greet", move |name: Arc<String>| {
//!         scope
//!             .facts(|| Ok(FactIndex::from_rows(Vec::new(), Vec::new())))
//!             .map_err(|e| cel::ExecutionError::function_error("greet", e))?;
//!         Ok(Value::String(name))
//!     });
//! });
//!
//! let program = engine.compile("greet('world')").unwrap();
//! let value = engine.evaluate(&program, &[]).unwrap();
//! assert_eq!(value, Value::String(Arc::new("world".to_string())));
//! ```
//!
//! # Value mapping: `cel::Value` ↔ data (task 1.7, frozen)
//!
//! The contract functions map data to `cel::Value` exactly as follows:
//!
//! | CEL | Rust argument / return | Data |
//! |---|---|---|
//! | integer literal | `i64` | entity id |
//! | string literal | `Arc<String>` | predicate, value, chunk text |
//! | `facts(e)` | `Arc<Vec<Value>>` | a list of maps, one per approved fact of `e` (subject OR object side), in `facts.id` order |
//! | `has_fact(e, k, v)` | `bool` | whether `e` has an approved fact with predicate `k` and value `v` |
//! | `chunks(e)` | `Arc<Vec<Value>>` | the chunk texts of `e` (list of strings), in `sequence_num` order |
//! | `chunk_contains(e, t)` | `bool` | whether any chunk text of `e` contains `t` |
//! | `neighbors(e)` | `Arc<Vec<Value>>` | the entity ids directly adjacent to `e` (all edge kinds, both directions), deduplicated, ascending |
//! | `path_exists(from, to, max_depth)` | `bool` | whether `to` is reachable from `from` within `max_depth` steps under the D4 boundary rule |
//!
//! The `facts(e)` map carries exactly the keys `id` (int, the `facts.id`),
//! `predicate` (string), `domain` (string) and `value` (string — the fact's
//! VALUE, see below). `chunks(e)` returns the raw chunk texts.
//!
//! **Fact value:** a fact is a subject—predicate→object triple, so its value
//! is the OBJECT entity's NAME (the `names` map of [`FactIndex`]). A fact
//! with no object endpoint (the v5 schema allows `NULL`) has value `""` in
//! the `facts(e)` map, and never matches `has_fact` (an empty value is not a
//! name).
//!
//! **Missing entity:** an entity id without rows yields the EMPTY list /
//! `false` — never an error (oracle parity: `FactIndex.Lookup` /
//! `ChunkIndex.Texts` return empty on a miss).
//!
//! **Storage failure:** a db error while building an index (e.g. the table
//! was dropped) is bridged with `ExecutionError::function_error` and
//! surfaces as [`GraphError::CelEval`] from ALL four functions — a broken
//! database must not silently produce "no link" decisions (deviation from
//! the oracle, see below).
//!
//! # Verified `cel` 0.14 API notes (registry sources, re-verified in 1.6)
//!
//! - `Program::compile` is PARSE-ONLY (no type checking): a call to an
//!   unknown function parses fine and fails at execution time with
//!   `ExecutionError::UndeclaredReference` — [`CelEngine::evaluate`] maps it
//!   to [`GraphError::CelEval`], so an unknown function is an explicit
//!   error, never a panic.
//! - `Context::add_function` is honored only on a ROOT context; it is
//!   SILENTLY IGNORED on inner scopes (`new_inner_scope()` children). The
//!   engine always evaluates on a fresh root context, so this is safe here;
//!   never register on a child scope.
//! - Built-in function names are RESERVED: the environment's overloads
//!   (conversion functions `int`, `uint`, `double`, `string`, `bytes`, `dyn`,
//!   `size`, `timestamp`, `duration`, `optional.none`, and the operators)
//!   are resolved BEFORE the context's custom function registry, so a custom
//!   function cannot shadow a built-in name. Tasks 1.7/1.8 must keep the
//!   contract names out of that set (the six contract names do not collide).
//! - Closure arguments are converted by the crate: `i64`, `u64`, `f64`,
//!   `Arc<String>`, `Arc<Vec<u8>>`, `bool`, `Arc<Vec<Value>>`, `Value`
//!   (no `Option<T>` — there is no `FromContext` impl for it); closures take
//!   0–9 arguments. Returns are the same set EXCEPT bare `Value` (it is only
//!   accepted inside `Result<Value, ExecutionError>`), and each return `T`
//!   may be wrapped in `Result<T, ExecutionError>`.
//! - Internal failures (e.g. a db error from an index builder) are bridged
//!   into CEL with `ExecutionError::function_error(name, msg)`; the engine
//!   surfaces them as [`GraphError::CelEval`].
//!
//! # Deviations from the oracle (conscious)
//!
//! - The oracle's `ScopeCache` is ONE long-lived cache owned by the engine;
//!   its TTL expiry field is never set by the linker, so entries can go
//!   stale. Here every evaluation gets a FRESH cache that dies with the
//!   evaluation: simpler, and a changed database is always re-read.
//! - The oracle re-extends the cel-go environment per registration; the
//!   `cel` crate has no environment extension — functions are installed on
//!   the per-evaluation context instead (the installer pattern above).
//! - The D5 "GraphIndex" lazy slot is named [`ReachabilityIndex`]: the name
//!   `GraphIndex` is already taken in this crate by the index-availability
//!   enum (task 1.2).
//! - (task 1.7) `facts(e)` takes ONE argument (the frozen contract, design
//!   D5), not the oracle's two (`facts(id, predicate)`). Filtering by
//!   predicate stays available in expressions through the built-in `exists`
//!   / `filter` macros over the returned list (e.g.
//!   `facts(e).exists(f, f.predicate == 'works_at')`); the cel crate's
//!   function registry is one function per name, so overloading is
//!   impossible anyway.
//! - (task 1.7) The 3-arg `has_fact(e, k, v)` FIXES a Go bug: the oracle
//!   compared `v` against the fact's predicate and domain
//!   (`f.Predicate == value || f.Domain == value`), never against the fact's
//!   value. Here `v` is the fact's value — the object entity's name — and
//!   the predicate must equal `k` as well. The oracle's 2-arg overload is
//!   not in the frozen contract and is not implemented.
//! - (task 1.7) the `facts(e)` map gains the `value` key (object entity
//!   name, `""` when the fact has no object endpoint) over the oracle's
//!   `{id, predicate, domain}` — it makes the map self-consistent with the
//!   fixed `has_fact`.
//! - (task 1.7) a db failure during index building surfaces as a CEL
//!   function error from ALL FOUR functions. The oracle was inconsistent:
//!   `facts`/`chunks` returned an error, but `has_fact`/`chunk_contains`
//!   silently returned `false`. A broken database must not silently produce
//!   "no link" decisions. (A MISSING entity is still empty/false, not an
//!   error — oracle parity.)
//! - (task 1.7) `chunk_contains` is an EXACT, case-SENSITIVE substring test
//!   over the chunk texts (oracle parity: Go `strings.Contains`) — not FTS,
//!   not `LIKE`, no normalization.
//! - (task 1.8) `neighbors(e)` takes ONE argument (the frozen contract,
//!   design D5) and returns the DIRECT adjacency over ALL edge kinds, both
//!   directions, deduplicated and ascending; the oracle's `neighbors(e,
//!   hops)` returned the ids at EXACTLY `hops` distance over ENTITY LINKS
//!   only.
//! - (task 1.8) the oracle's `GraphIndex` hop layers are BROKEN: the "skip
//!   if already seen at a lower hop" check compares against the PREVIOUS
//!   layer, but layer 0 contains EVERY entity — so every layer >= 1 is
//!   empty, `neighbors(e, hops)` is always empty and `path_exists(a, b, n)`
//!   degenerates to `a == b`. No oracle test pins the broken behavior; here
//!   both functions are real (a direct adjacency read and a depth-bounded
//!   BFS, no prebuilt layers — see [`ReachabilityIndex`]).
//! - (task 1.8) `path_exists` traverses BOTH edge populations under the D4
//!   boundary rule with entity-link crossing ENABLED (the traverse mode in
//!   which cross-domain reachability is possible at all — the function's
//!   purpose in the cross-domain linker): a fact edge never leaves the
//!   START entity's domain, an entity-link edge may. The oracle applied no
//!   domain rule in its CEL `path_exists` (domain-unaware, links only) —
//!   the D4 rule is the contract-level fix. `max_depth` is used as given
//!   (0 = identity only) and clamped to the D4 hard max; it is NOT
//!   default-filled like traverse's zero-valued option.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use cel::objects::{Key as CelKey, Map as CelMap};
use cel::{Context, ExecutionError, Program, Value};
use db::DbExecutor;
use petgraph::Direction as PetDirection;
use petgraph::visit::EdgeRef;

use crate::error::GraphError;
use crate::graph::{EdgeKind, Graph};
use crate::traverser::HARD_MAX_DEPTH;

/// Locks `mutex`, recovering the guard if another thread panicked while
/// holding it. The critical sections guarded in this module hold no user
/// code (an `Option<Arc<T>>` check/insert or a `HashMap` lookup/insert), so
/// a poisoned lock cannot have corrupted the state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A lazy slot: the value is built by the FIRST accessor and shared (as an
/// `Arc`) by every later one.
struct LazySlot<T> {
    built: Mutex<Option<Arc<T>>>,
}

impl<T> LazySlot<T> {
    const fn new() -> Self {
        Self {
            built: Mutex::new(None),
        }
    }

    /// Returns the slot value, running `build` on the first access only.
    ///
    /// The builder runs OUTSIDE the lock; if two threads race for the first
    /// access both may build, and the second write wins. Harmless in
    /// practice: each [`ScopeCache`] belongs to one evaluation, and CEL
    /// evaluates an expression tree single-threaded.
    fn get_or_build(
        &self,
        build: impl FnOnce() -> Result<T, GraphError>,
    ) -> Result<Arc<T>, GraphError> {
        if let Some(existing) = lock(&self.built).as_ref() {
            return Ok(Arc::clone(existing));
        }
        let value = Arc::new(build()?);
        *lock(&self.built) = Some(Arc::clone(&value));
        Ok(value)
    }
}

/// Facts indexed per entity for the `facts` / `has_fact` contract functions
/// (task 1.7, design D5).
///
/// The Rust re-design of the oracle's `FactIndex`
/// (`internal/relations/scope_builders.go`): the APPROVED facts (the db
/// crate's `FactDao::list_all` — same `status = 'approved'` `ORDER BY id`
/// as the oracle's `ListAll`) are attached to BOTH endpoint entities (the
/// oracle's `for _, eid := range []int{f.SubjectEntityID, f.ObjectEntityID}`),
/// and a `names` map resolves a fact's value (see [`Self::value_of`]).
///
/// A fact with a `NULL` endpoint (the v5 schema allows it) is attached to
/// its existing endpoint only — the oracle skipped its Go zero value `0`,
/// which is the same attachment rule.
#[derive(Debug, Default)]
pub struct FactIndex {
    /// Entity id → the approved facts attached to it (subject OR object
    /// side), in `facts.id` order (the builder's `ORDER BY id`).
    by_entity: HashMap<i64, Vec<db::Fact>>,
    /// Entity id → canonical name (value resolution for `has_fact` and the
    /// `value` key of `facts`).
    names: HashMap<i64, String>,
}

impl FactIndex {
    /// Build the index from already-loaded rows (pure; the SQLite version is
    /// [`build_fact_index`]). A fact is attached to its subject and (when
    /// different) its object entry; `NULL` endpoints attach nothing.
    #[must_use]
    pub fn from_rows(facts: Vec<db::Fact>, entities: Vec<db::Entity>) -> Self {
        let mut names = HashMap::with_capacity(entities.len());
        for entity in &entities {
            names.insert(entity.id, entity.name.clone());
        }
        let mut by_entity: HashMap<i64, Vec<db::Fact>> = HashMap::new();
        for fact in facts {
            if let Some(subject) = fact.subject_entity_id {
                by_entity.entry(subject).or_default().push(fact.clone());
            }
            if let Some(object) = fact.object_entity_id
                && fact.subject_entity_id != Some(object)
            {
                by_entity.entry(object).or_default().push(fact);
            }
        }
        Self { by_entity, names }
    }

    /// The approved facts attached to one entity (subject OR object side),
    /// in `facts.id` order; empty for an entity without facts or a missing
    /// entity (oracle parity: a lookup miss is an empty result, not an
    /// error).
    #[must_use]
    pub fn facts(&self, entity_id: i64) -> &[db::Fact] {
        match self.by_entity.get(&entity_id) {
            Some(facts) => facts.as_slice(),
            None => &[],
        }
    }

    /// The fact's value: the OBJECT entity's name (a fact is a
    /// subject—predicate→object triple, so the object is the value side), or
    /// `None` when the fact has no object endpoint or the object entity is
    /// unknown.
    #[must_use]
    pub fn value_of(&self, fact: &db::Fact) -> Option<&str> {
        fact.object_entity_id
            .and_then(|id| self.names.get(&id))
            .map(String::as_str)
    }

    /// Whether the entity has an approved fact with predicate `key` and
    /// value `value` (the value is the object entity's name — see
    /// [`Self::value_of`]).
    #[must_use]
    pub fn has_fact(&self, entity_id: i64, key: &str, value: &str) -> bool {
        self.facts(entity_id)
            .iter()
            .any(|fact| fact.predicate == key && self.value_of(fact) == Some(value))
    }
}

/// Build the facts index from SQLite in one pass (design D5): approved
/// facts via `FactDao::list_all` (the oracle's `ListAll`) plus all entity
/// names for value resolution.
pub fn build_fact_index(db: &db::Db) -> Result<FactIndex, GraphError> {
    db.with_conn(|conn| -> Result<FactIndex, GraphError> {
        let exec = db::ConnectionOrTx::Connection(conn);
        let facts = db::FactDao::new(exec).list_all()?;
        let entities = db::EntityDao::new(exec).list()?;
        Ok(FactIndex::from_rows(facts, entities))
    })?
}

/// Chunk texts per entity for the `chunks` / `chunk_contains` contract
/// functions (task 1.7, design D5).
///
/// The Rust re-design of the oracle's `ChunkIndex`
/// (`internal/relations/scope_builders.go`): one query over the
/// `chunk_entities` join — the oracle ran the same raw SQL, because no DAO
/// covers the all-entities shape.
#[derive(Debug, Default)]
pub struct ChunkIndex {
    /// Entity id → its chunk texts, in `sequence_num` (id tie-break) order.
    by_entity: HashMap<i64, Vec<String>>,
}

impl ChunkIndex {
    /// Build the index from (entity_id, chunk_text) rows (pure; the SQLite
    /// version is [`build_chunk_index`]). Rows are appended in the given
    /// order, so the caller owns the ordering.
    #[must_use]
    pub fn from_rows(rows: Vec<(i64, String)>) -> Self {
        let mut by_entity: HashMap<i64, Vec<String>> = HashMap::new();
        for (entity_id, text) in rows {
            by_entity.entry(entity_id).or_default().push(text);
        }
        Self { by_entity }
    }

    /// The chunk texts of one entity; empty for an entity without chunks or
    /// a missing entity (oracle parity: a lookup miss is an empty result,
    /// not an error).
    #[must_use]
    pub fn texts(&self, entity_id: i64) -> &[String] {
        match self.by_entity.get(&entity_id) {
            Some(texts) => texts.as_slice(),
            None => &[],
        }
    }

    /// Whether any chunk text of the entity contains `text` as an EXACT,
    /// CASE-SENSITIVE substring (oracle parity: `strings.Contains` — not
    /// FTS, not `LIKE`, no normalization).
    #[must_use]
    pub fn contains(&self, entity_id: i64, text: &str) -> bool {
        self.texts(entity_id)
            .iter()
            .any(|chunk| chunk.contains(text))
    }
}

/// Build the chunk-text index from SQLite in one pass (design D5): every
/// (entity, chunk_text) pair of the `chunk_entities` join, ordered by
/// `sequence_num` with an `id` tie-break (house convention; the oracle's
/// `sequence_num`-only order ties across documents).
pub fn build_chunk_index(db: &db::Db) -> Result<ChunkIndex, GraphError> {
    db.with_conn(|conn| -> Result<ChunkIndex, GraphError> {
        let exec = db::ConnectionOrTx::Connection(conn);
        let pairs = exec.query(
            "SELECT ce.entity_id, c.chunk_text \
             FROM chunk_entities ce INNER JOIN chunks c ON c.id = ce.chunk_id \
             ORDER BY ce.entity_id, c.sequence_num, c.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(ChunkIndex::from_rows(pairs))
    })?
}

/// The in-memory graph behind the `neighbors` / `path_exists` contract
/// functions (task 1.8, design D5).
///
/// Design D5 calls this the "GraphIndex"; the name `GraphIndex` is already
/// taken in this crate by the index-availability enum (task 1.2), so the
/// slot type is named `ReachabilityIndex` instead.
///
/// The slot holds the FULL in-memory [`Graph`] (task 1.2) and answers both
/// functions WITHOUT prebuilt reachability layers:
/// - `neighbors` — a direct O(degree) adjacency read, both directions;
/// - `path_exists` — a per-call depth-bounded BFS (worst case O(V+E), early
///   exit on a hit).
///
/// Scale reasoning (design D1): a personal corpus is thousands to hundreds
/// of thousands of entities (an index of units of MB), so a per-call BFS is
/// microseconds-to-milliseconds. A prebuilt transitive closure (the oracle's
/// hop layers) would cost O(V^2) memory — infeasible at 100k entities — for
/// no measurable gain; YAGNI. The oracle's layers are also broken (see the
/// module docs, deviations).
#[derive(Debug)]
pub struct ReachabilityIndex {
    graph: Graph,
}

impl ReachabilityIndex {
    /// Wrap an already-built index (the SQLite version is
    /// [`build_reachability_index`]).
    #[must_use]
    pub fn new(graph: Graph) -> Self {
        Self { graph }
    }

    /// The entity ids directly adjacent to `entity_id`: every incident edge
    /// (fact AND entity-link, both directions), deduplicated, ascending.
    /// Empty for a missing entity (never an error).
    ///
    /// Adjacency is a one-hop view, not a traversal: the D4 domain boundary
    /// rule (a traversal concept) does not filter it — a cross-domain fact
    /// edge still makes the other endpoint a neighbor.
    #[must_use]
    pub fn neighbors(&self, entity_id: i64) -> Vec<i64> {
        let Some(node) = self.graph.node_index(entity_id) else {
            return Vec::new();
        };
        let g = self.graph.graph();
        // Capacity from the node's actual degree (both directions), not a
        // fixed guess: most entities have few links, hubs have many.
        let degree = g.edges_directed(node, PetDirection::Outgoing).count()
            + g.edges_directed(node, PetDirection::Incoming).count();
        let mut ids = HashSet::with_capacity(degree);
        for edge in g.edges_directed(node, PetDirection::Outgoing) {
            ids.insert(g[edge.target()].id);
        }
        for edge in g.edges_directed(node, PetDirection::Incoming) {
            ids.insert(g[edge.source()].id);
        }
        let mut ids: Vec<i64> = ids.into_iter().collect();
        ids.sort_unstable();
        ids
    }

    /// Whether `to_id` is reachable from `from_id` within `max_depth` edge
    /// steps (both directions), under the SAME D4 boundary rule as
    /// [`crate::Graph::traverse`] with entity-link crossing enabled:
    /// - a step over a FACT edge never leaves the START entity's domain;
    /// - a step over an ENTITY-LINK edge may cross domains.
    ///
    /// `from_id == to_id` is `true` when the entity exists (a zero-length
    /// path). A missing `from_id` or `to_id` is `false`, never an error.
    /// `max_depth` is used as given (0 = identity only) and clamped to the
    /// D4 hard max (`HARD_MAX_DEPTH`); it is not default-filled like
    /// traverse's zero-valued option (an explicit CEL argument is not a zero
    /// value).
    #[must_use]
    pub fn path_exists(&self, from_id: i64, to_id: i64, max_depth: i64) -> bool {
        let (Some(from), Some(to)) = (self.graph.node_index(from_id), self.graph.node_index(to_id))
        else {
            return false;
        };
        if from == to {
            return true;
        }
        let depth = max_depth.clamp(0, HARD_MAX_DEPTH as i64) as u32;
        if depth == 0 {
            return false;
        }

        let g = self.graph.graph();
        let start_domain = g[from].domain.clone();
        let mut visited = HashSet::with_capacity(16);
        visited.insert(from);
        let mut level = vec![from];
        for _ in 0..depth {
            let mut next = Vec::new();
            for &node in &level {
                for (edge, neighbor) in g
                    .edges_directed(node, PetDirection::Outgoing)
                    .map(|e| (e, e.target()))
                    .chain(
                        g.edges_directed(node, PetDirection::Incoming)
                            .map(|e| (e, e.source())),
                    )
                {
                    if visited.contains(&neighbor) {
                        continue; // cycle protection
                    }
                    if g[neighbor].domain != start_domain
                        && edge.weight().kind != EdgeKind::EntityLink
                    {
                        // D4 boundary (the same rule as traverse): a fact
                        // edge never crosses the start domain; only an
                        // entity link may.
                        continue;
                    }
                    if neighbor == to {
                        return true;
                    }
                    visited.insert(neighbor);
                    next.push(neighbor);
                }
            }
            if next.is_empty() {
                return false;
            }
            level = next;
        }
        false
    }
}

/// Build the reachability index from SQLite in one pass (design D5): all
/// entities, approved facts and entity links — the same rows as the startup
/// index (`GraphIndex::from_db`), without its configuration gating: the CEL
/// evaluation path always reads the current database state (oracle parity:
/// the scope loader built the graph index unconditionally).
pub fn build_reachability_index(db: &db::Db) -> Result<ReachabilityIndex, GraphError> {
    db.with_conn(|conn| -> Result<ReachabilityIndex, GraphError> {
        let exec = db::ConnectionOrTx::Connection(conn);
        let entities = db::EntityDao::new(exec).list()?;
        let facts = db::FactDao::new(exec).list_all()?;
        let links = db::EntityLinkDao::new(exec).list_all()?;
        Ok(ReachabilityIndex::new(Graph::from_rows(
            entities, facts, links,
        )))
    })?
}

/// Per-evaluation shared state for the contract functions (design D5).
///
/// The Rust re-design of the oracle's `ScopeCache`
/// (`internal/expression/scope_cache.go`): heavy data is built by the FIRST
/// function call that needs it and cached for the rest of the evaluation. A
/// fresh cache is created by [`CelEngine::evaluate`] for EVERY evaluation
/// and dies with it — nothing survives, so a changed database is always
/// re-read (the oracle's single long-lived cache is a deliberate non-port,
/// see the module docs).
///
/// Functions reach the cache through the `Arc<ScopeCache>` captured in the
/// closures installed via [`CelEngine::register`]. The cache is `Send +
/// Sync`; concurrent access to the same cache is safe (each slot is
/// mutex-guarded) though a single evaluation is single-threaded in practice.
pub struct ScopeCache {
    facts: LazySlot<FactIndex>,
    chunks: LazySlot<ChunkIndex>,
    reachability: LazySlot<ReachabilityIndex>,
}

impl ScopeCache {
    /// Creates an empty cache: no index is built until a function needs it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            facts: LazySlot::new(),
            chunks: LazySlot::new(),
            reachability: LazySlot::new(),
        }
    }

    /// The facts index, built on first access by `build` (the contract
    /// functions use [`build_fact_index`], task 1.7).
    pub fn facts(
        &self,
        build: impl FnOnce() -> Result<FactIndex, GraphError>,
    ) -> Result<Arc<FactIndex>, GraphError> {
        self.facts.get_or_build(build)
    }

    /// The chunk index, built on first access by `build` (the contract
    /// functions use [`build_chunk_index`], task 1.7).
    pub fn chunks(
        &self,
        build: impl FnOnce() -> Result<ChunkIndex, GraphError>,
    ) -> Result<Arc<ChunkIndex>, GraphError> {
        self.chunks.get_or_build(build)
    }

    /// The reachability index, built on first access by `build` (the
    /// contract functions use [`build_reachability_index`], task 1.8).
    pub fn reachability(
        &self,
        build: impl FnOnce() -> Result<ReachabilityIndex, GraphError>,
    ) -> Result<Arc<ReachabilityIndex>, GraphError> {
        self.reachability.get_or_build(build)
    }
}

impl Default for ScopeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Installs contract functions into a fresh evaluation context.
///
/// Called once per [`CelEngine::evaluate`] with that evaluation's
/// `Arc<ScopeCache>` and the fresh root [`Context`]. Implementations register
/// closures with `Context::add_function`, capturing the scope cache (and any
/// other long-lived state) in the closure body — see the module docs for the
/// full pattern.
pub type FunctionInstaller = Box<dyn Fn(Arc<ScopeCache>, &mut Context<'static>) + Send + Sync>;

/// CEL engine: compile expression sources once, evaluate many (design D2/D5).
///
/// See the module docs for the threading model (design D7) and the function
/// registration pattern (tasks 1.7/1.8).
pub struct CelEngine {
    installers: Vec<FunctionInstaller>,
    programs: Mutex<HashMap<String, Arc<Program>>>,
}

impl CelEngine {
    /// Creates an engine with no contract functions registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            installers: Vec::new(),
            programs: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a function installer; it runs for every evaluation.
    ///
    /// Tasks 1.7/1.8 install the six contract functions through this method;
    /// the engine itself hardcodes none.
    pub fn register(
        &mut self,
        installer: impl Fn(Arc<ScopeCache>, &mut Context<'static>) + Send + Sync + 'static,
    ) {
        self.installers.push(Box::new(installer));
    }

    /// Compiles `source` once; later calls with the same source return the
    /// cached program. Parse errors map to [`GraphError::CelParse`].
    pub fn compile(&self, source: &str) -> Result<Arc<Program>, GraphError> {
        if let Some(cached) = lock(&self.programs).get(source) {
            return Ok(Arc::clone(cached));
        }
        let program = Arc::new(Program::compile(source)?);
        lock(&self.programs).insert(source.to_string(), Arc::clone(&program));
        Ok(program)
    }

    /// Evaluates `program` in a fresh per-evaluation scope: a new
    /// [`ScopeCache`], a root context with the cel built-ins plus every
    /// registered contract function, and the given variable bindings.
    ///
    /// Execution errors — including a call to an UNREGISTERED function
    /// (`ExecutionError::UndeclaredReference`, since `compile` is
    /// parse-only) — map to [`GraphError::CelEval`].
    pub fn evaluate(
        &self,
        program: &Program,
        bindings: &[(&str, Value)],
    ) -> Result<Value, GraphError> {
        let scope = Arc::new(ScopeCache::new());
        let mut context: Context<'static> = Context::default();
        for installer in &self.installers {
            installer(Arc::clone(&scope), &mut context);
        }
        for (name, value) in bindings {
            context.add_variable_from_value(*name, value.clone());
        }
        program.execute(&context).map_err(GraphError::from)
    }
}

impl Default for CelEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Map one fact to the CEL value of the `facts` contract function: a map
/// with exactly the keys documented in the module docs (`id` / `predicate` /
/// `domain` / `value`).
fn fact_to_value(index: &FactIndex, fact: &db::Fact) -> Value {
    let value = index
        .value_of(fact)
        .map_or_else(String::new, |name| name.to_string());
    let mut map = HashMap::with_capacity(4);
    map.insert(
        CelKey::String(Arc::new("id".to_string())),
        Value::Int(fact.id),
    );
    map.insert(
        CelKey::String(Arc::new("predicate".to_string())),
        Value::String(Arc::new(fact.predicate.clone())),
    );
    map.insert(
        CelKey::String(Arc::new("domain".to_string())),
        Value::String(Arc::new(fact.domain.clone())),
    );
    map.insert(
        CelKey::String(Arc::new("value".to_string())),
        Value::String(Arc::new(value)),
    );
    Value::Map(CelMap { map: Arc::new(map) })
}

/// Install the four data-backed contract functions (task 1.7, design D5) —
/// `facts`, `has_fact`, `chunks`, `chunk_contains` — on `engine`, bound to
/// `db` (the SQLite source of truth, design D1).
///
/// The indexes are built lazily on the FIRST call of the corresponding
/// function in an evaluation and cached in the per-evaluation
/// [`ScopeCache`] (see the module docs for the installer pattern): `facts`
/// and `has_fact` share the facts slot, `chunks` and `chunk_contains` share
/// the chunks slot. Task 1.8 installs the two graph functions
/// (`neighbors`, `path_exists`) through the same mechanism.
///
/// A db failure during the lazy build is bridged with
/// `ExecutionError::function_error` and surfaces as [`GraphError::CelEval`]
/// from every function (see the module docs for the deviation from the
/// oracle); a missing entity is empty/false, never an error.
pub fn register_data_functions(engine: &mut CelEngine, db: Arc<db::Db>) {
    engine.register(move |scope, ctx| {
        // Each registered function closure is 'static and outlives the
        // installer's parameters, so every one owns its own clones.
        let (facts_scope, has_fact_scope, chunks_scope, contains_scope) = (
            Arc::clone(&scope),
            Arc::clone(&scope),
            Arc::clone(&scope),
            Arc::clone(&scope),
        );
        let (facts_db, has_fact_db, chunks_db, contains_db) = (
            Arc::clone(&db),
            Arc::clone(&db),
            Arc::clone(&db),
            Arc::clone(&db),
        );

        ctx.add_function("facts", move |entity: i64| {
            let index = facts_scope
                .facts(|| build_fact_index(&facts_db))
                .map_err(|err| ExecutionError::function_error("facts", err.to_string()))?;
            let values = index
                .facts(entity)
                .iter()
                .map(|fact| fact_to_value(&index, fact))
                .collect::<Vec<Value>>();
            Ok(Arc::new(values))
        });

        ctx.add_function(
            "has_fact",
            move |entity: i64, key: Arc<String>, value: Arc<String>| {
                let index = has_fact_scope
                    .facts(|| build_fact_index(&has_fact_db))
                    .map_err(|err| ExecutionError::function_error("has_fact", err.to_string()))?;
                Ok(index.has_fact(entity, &key, &value))
            },
        );

        ctx.add_function("chunks", move |entity: i64| {
            let index = chunks_scope
                .chunks(|| build_chunk_index(&chunks_db))
                .map_err(|err| ExecutionError::function_error("chunks", err.to_string()))?;
            let values = index
                .texts(entity)
                .iter()
                .map(|text| Value::String(Arc::new(text.clone())))
                .collect::<Vec<Value>>();
            Ok(Arc::new(values))
        });

        ctx.add_function("chunk_contains", move |entity: i64, text: Arc<String>| {
            let index = contains_scope
                .chunks(|| build_chunk_index(&contains_db))
                .map_err(|err| ExecutionError::function_error("chunk_contains", err.to_string()))?;
            Ok(index.contains(entity, &text))
        });
    });
}

/// Install the two graph-backed contract functions (task 1.8, design D5) —
/// `neighbors` and `path_exists` — on `engine`, bound to `db` (the SQLite
/// source of truth, design D1).
///
/// Both share the lazy reachability slot of the per-evaluation
/// [`ScopeCache`]: the index is built on the FIRST call of either function
/// in an evaluation and cached for the rest of it (see the module docs for
/// the installer pattern). A db failure during the lazy build is bridged
/// with `ExecutionError::function_error` and surfaces as
/// [`GraphError::CelEval`] from both functions (see the module docs for the
/// deviation from the oracle); a missing entity is empty / false, never an
/// error.
pub fn register_graph_functions(engine: &mut CelEngine, db: Arc<db::Db>) {
    engine.register(move |scope, ctx| {
        // Each registered function closure is 'static and outlives the
        // installer's parameters, so every one owns its own clones.
        let (neighbors_scope, path_scope) = (Arc::clone(&scope), Arc::clone(&scope));
        let (neighbors_db, path_db) = (Arc::clone(&db), Arc::clone(&db));

        ctx.add_function("neighbors", move |entity: i64| {
            let index = neighbors_scope
                .reachability(|| build_reachability_index(&neighbors_db))
                .map_err(|err| ExecutionError::function_error("neighbors", err.to_string()))?;
            let ids = index
                .neighbors(entity)
                .into_iter()
                .map(Value::Int)
                .collect::<Vec<Value>>();
            Ok(Arc::new(ids))
        });

        ctx.add_function("path_exists", move |from: i64, to: i64, max_depth: i64| {
            let index = path_scope
                .reachability(|| build_reachability_index(&path_db))
                .map_err(|err| ExecutionError::function_error("path_exists", err.to_string()))?;
            Ok(index.path_exists(from, to, max_depth))
        });
    });
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect/panic are intentional (the fixtures are
    // compile-time constants).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

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

    use db::test_util::in_memory_db;
    use db::{
        ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, EntityDao, EntityLinkDao, FactDao,
    };

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
                let f1 =
                    facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
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

    use db::{Entity, EntityLink, Fact};

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
}
