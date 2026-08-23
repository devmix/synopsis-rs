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
//!             .facts(|| Ok(FactIndex))
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use cel::{Context, Program, Value};

use crate::error::GraphError;

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

/// Facts indexed per entity for the `facts` / `has_fact` contract functions.
///
/// Placeholder (task 1.6): task 1.7 replaces this unit struct with the real
/// structure — facts per entity id, built from the db crate's `FactDao`
/// (design D5).
pub struct FactIndex;

/// Chunk texts per entity for the `chunks` / `chunk_contains` contract
/// functions.
///
/// Placeholder (task 1.6): task 1.7 replaces this unit struct with the real
/// structure — chunk texts per entity id, built from the db crate's
/// `ChunkDao` / `ChunkEntityDao` (design D5).
pub struct ChunkIndex;

/// Depth-bounded reachability for the `path_exists` contract function.
///
/// Design D5 calls this the "GraphIndex" (BFS reachability layers); the name
/// `GraphIndex` is already taken in this crate by the index-availability
/// enum (task 1.2), so the slot type is named `ReachabilityIndex` instead.
///
/// Placeholder (task 1.6): task 1.8 replaces this unit struct with the real
/// structure (depth-bounded reachability over [`crate::Graph`] with the D4
/// domain boundaries).
pub struct ReachabilityIndex;

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

    /// The facts index, built on first access by `build` (task 1.7).
    pub fn facts(
        &self,
        build: impl FnOnce() -> Result<FactIndex, GraphError>,
    ) -> Result<Arc<FactIndex>, GraphError> {
        self.facts.get_or_build(build)
    }

    /// The chunk index, built on first access by `build` (task 1.7).
    pub fn chunks(
        &self,
        build: impl FnOnce() -> Result<ChunkIndex, GraphError>,
    ) -> Result<Arc<ChunkIndex>, GraphError> {
        self.chunks.get_or_build(build)
    }

    /// The reachability index, built on first access by `build` (task 1.8).
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
                        Ok(FactIndex)
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
}
