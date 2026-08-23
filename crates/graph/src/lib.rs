//! Knowledge graph: entity storage over SQLite with a derived in-memory
//! index, and CEL-based cross-domain entity linking.
//!
//! Oracle mapping: `../synopsis/internal/graph` + `internal/relations` +
//! `internal/expression` (design.md D1/D2/D5). The Go code is a reference
//! for behavior and contracts only, not a code blueprint.
//!
//! Architecture (design D1, hybrid storage): the `entities` / `entity_links`
//! tables in SQLite (accessed through the `db` crate DAOs) are the single
//! source of truth. This crate builds a `petgraph::DiGraph` plus
//! `HashMap`-based name and type indexes as a derived, read-only in-memory
//! index, rebuilt in one pass at startup (D3); all writes go through the db
//! DAOs, never through the index. Cross-domain entity linking evaluates
//! ontology CEL rules on the `cel` crate (D2, replaces cel-interpreter)
//! with the contract functions `facts`, `has_fact`, `chunks`,
//! `chunk_contains`, `neighbors`, `path_exists` (D5), heavy indexes built
//! lazily per evaluation scope.
//!
//! The public API grows with the change tasks: index builder (1.2), entity
//! lookup (1.3), domain-bounded traversal (1.4), metrics and DOT export
//! (1.5), CEL engine and contract functions (1.6–1.8), linkers (1.9).

pub mod cel;
pub mod error;
pub mod graph;
pub mod linker;
pub mod metrics;
pub mod prompts;
pub mod traverser;

pub use cel::{
    CelEngine, ChunkIndex, FactIndex, FunctionInstaller, ReachabilityIndex, ScopeCache,
    build_chunk_index, build_fact_index, build_reachability_index, register_data_functions,
    register_graph_functions,
};
pub use error::GraphError;
pub use graph::{EdgeKind, EntityNode, Graph, GraphEdge, GraphIndex};
pub use linker::{LinkResult, build_entity_links};
pub use metrics::GraphStats;
pub use prompts::{
    EntityData, EntityLinkerPrompts, LinkerInput, TemplateHashes, load_entity_linker_prompts,
};
pub use traverser::{
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES, Direction, HARD_MAX_DEPTH, TraversalEdge,
    TraverseOptions, TraverseResult,
};

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (the smoke fixtures are
    // compile-time constants).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::error::GraphError;
    use petgraph::graph::DiGraph;

    /// Feasibility smoke (task 1.1): proves the `cel` dependency links and
    /// runs — parse a trivial expression and evaluate it.
    #[test]
    fn smoke_cel_parse_and_evaluate() {
        let program = cel::Program::compile("1 + 1 == 2").unwrap();
        let value = program.execute(&cel::Context::default()).unwrap();
        assert_eq!(value, cel::Value::Bool(true));
    }

    /// Feasibility smoke (task 1.1): proves the `petgraph` dependency links
    /// and runs — the index type the whole crate is built on.
    #[test]
    fn smoke_petgraph_empty_digraph() {
        let graph: DiGraph<(), ()> = DiGraph::new();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn cel_parse_error_maps_to_graph_error() {
        let parse_err =
            cel::Program::compile("1 +").expect_err("truncated expression must not parse");
        let graph_err: GraphError = parse_err.into();
        assert!(matches!(graph_err, GraphError::CelParse { .. }));
        assert!(graph_err.to_string().starts_with("CEL parse error"));
        assert!(std::error::Error::source(&graph_err).is_some());
    }

    #[test]
    fn db_error_maps_to_graph_error() {
        let graph_err = GraphError::from(db::DbError::NestedTransaction);
        assert!(matches!(graph_err, GraphError::Db(..)));
        assert!(graph_err.to_string().starts_with("storage error"));
        assert!(std::error::Error::source(&graph_err).is_some());
    }
}
