//! CEL engine for ontology linking rules (design D2/D5, task 1.6).
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
//! - [`ScopeCache`] — per-evaluation lazy indexes: heavy data is built by
//!   the first function call that needs it and shared by every later call
//!   in the same evaluation.
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
//! `false` — never an error (a lookup miss is an empty result).
//!
//! **Storage failure:** a db error while building an index (e.g. the table
//! was dropped) is bridged with `ExecutionError::function_error` and
//! surfaces as [`GraphError::CelEval`] from ALL four functions — a broken
//! database must not silently produce "no link" decisions (see the design
//! decisions below).
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
//! # Design decisions
//!
//! - Every evaluation gets a FRESH [`ScopeCache`] that dies with the
//!   evaluation: simpler than a single long-lived engine-owned cache (whose
//!   TTL expiry is never set, so entries can go stale), and a changed
//!   database is always re-read.
//! - The `cel` crate has no environment extension — functions are installed
//!   on the per-evaluation context instead (the installer pattern above).
//! - The D5 "GraphIndex" lazy slot is named [`ReachabilityIndex`]: the name
//!   `GraphIndex` is already taken in this crate by the index-availability
//!   enum (task 1.2).
//! - (task 1.7) `facts(e)` takes ONE argument (the frozen contract, design
//!   D5). Filtering by predicate stays available in expressions through the
//!   built-in `exists` / `filter` macros over the returned list (e.g.
//!   `facts(e).exists(f, f.predicate == 'works_at')`); the cel crate's
//!   function registry is one function per name, so overloading is
//!   impossible anyway.
//! - (task 1.7) The 3-arg `has_fact(e, k, v)` compares `v` against the
//!   fact's VALUE — the object entity's name — and requires the predicate
//!   to equal `k` as well. The 2-arg overload is not in the frozen contract
//!   and is not implemented.
//! - (task 1.7) the `facts(e)` map carries the `value` key (object entity
//!   name, `""` when the fact has no object endpoint) in addition to
//!   `{id, predicate, domain}` — it makes the map self-consistent with
//!   `has_fact`.
//! - (task 1.7) a db failure during index building surfaces as a CEL
//!   function error from ALL FOUR functions. A broken database must not
//!   silently produce "no link" decisions. (A MISSING entity is still
//!   empty/false, not an error.)
//! - (task 1.7) `chunk_contains` is an EXACT, case-SENSITIVE substring test
//!   over the chunk texts — not FTS, not `LIKE`, no normalization.
//! - (task 1.8) `neighbors(e)` takes ONE argument (the frozen contract,
//!   design D5) and returns the DIRECT adjacency over ALL edge kinds, both
//!   directions, deduplicated and ascending.
//! - (task 1.8) both graph functions are real (a direct adjacency read and
//!   a depth-bounded BFS, no prebuilt layers — see [`ReachabilityIndex`]).
//! - (task 1.8) `path_exists` traverses BOTH edge populations under the D4
//!   boundary rule with entity-link crossing ENABLED (the traverse mode in
//!   which cross-domain reachability is possible at all — the function's
//!   purpose in the cross-domain linker): a fact edge never leaves the
//!   START entity's domain, an entity-link edge may. `max_depth` is used as
//!   given (0 = identity only) and clamped to the D4 hard max; it is NOT
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
/// The APPROVED facts (the db crate's `FactDao::list_all`, `status =
/// 'approved'`, `ORDER BY id`) are attached to BOTH endpoint entities, and
/// a `names` map resolves a fact's value (see [`Self::value_of`]).
///
/// A fact with a `NULL` endpoint (the v5 schema allows it) is attached to
/// its existing endpoint only.
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
    /// entity (a lookup miss is an empty result, not an error).
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
/// facts via `FactDao::list_all` plus all entity names for value
/// resolution.
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
/// One query over the `chunk_entities` join — no DAO covers the
/// all-entities shape.
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
    /// a missing entity (a lookup miss is an empty result, not an error).
    #[must_use]
    pub fn texts(&self, entity_id: i64) -> &[String] {
        match self.by_entity.get(&entity_id) {
            Some(texts) => texts.as_slice(),
            None => &[],
        }
    }

    /// Whether any chunk text of the entity contains `text` as an EXACT,
    /// CASE-SENSITIVE substring (not FTS, not `LIKE`, no normalization).
    #[must_use]
    pub fn contains(&self, entity_id: i64, text: &str) -> bool {
        self.texts(entity_id)
            .iter()
            .any(|chunk| chunk.contains(text))
    }
}

/// Build the chunk-text index from SQLite in one pass (design D5): every
/// (entity, chunk_text) pair of the `chunk_entities` join, ordered by
/// `sequence_num` with an `id` tie-break (house convention; a
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
/// microseconds-to-milliseconds. A prebuilt transitive closure would cost
/// O(V^2) memory — infeasible at 100k entities — for no measurable gain;
/// YAGNI.
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
/// evaluation path always reads the current database state.
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
/// Heavy data is built by the FIRST function call that needs it and cached
/// for the rest of the evaluation. A fresh cache is created by
/// [`CelEngine::evaluate`] for EVERY evaluation and dies with it — nothing
/// survives, so a changed database is always re-read (a single long-lived
/// cache would allow stale entries — see the module docs).
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
/// from every function (see the module docs, design decisions); a missing
/// entity is empty/false, never an error.
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
/// [`GraphError::CelEval`] from both functions (see the module docs, design
/// decisions); a missing entity is empty / false, never an error.
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
