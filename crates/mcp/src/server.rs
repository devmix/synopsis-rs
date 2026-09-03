//! MCP server: dual transport composed into one axum router (design D1/D5)
//! with the frozen 12-tool registry (`mcp-contract`): the rmcp 3.x
//! Streamable HTTP service as the fallback (design D8) plus the Go oracle's
//! legacy HTTP+SSE wire contract on explicit `GET /sse` + `POST /message`
//! routes (D8 override, user decision 2026-08-31; wire reference mcp-go
//! v0.57.0).
//!
//! Oracle mapping: `../synopsis/internal/mcp/{server.go,tools.go}`. The Go
//! code is the behavior/contract reference only — this is the Rust
//! re-architecture (functional copy, not a code copy): tool schemas are
//! transcribed from `tools.go` as `rmcp::model::Tool` objects, and every
//! registered tool is backed by a real handler (design D2).

use std::sync::{Arc, PoisonError, RwLock};

use axum::Router;
use axum::routing::{get, post};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations, object,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use search::Searcher;
use serde_json::{Value, json};

use crate::error::McpError;
use crate::health::{HealthState, handler as health_handler};
use crate::tools;
use crate::transport;

/// The MCP server: injected collaborators (design D1) + the frozen tool
/// registry. Cloned per session by the rmcp service factory.
///
/// The search and graph handles are hot-swappable (the design D8 seam): the
/// search crate's hybrid searcher is immutable by design, so the CLI
/// rebuilds it after a knowledge-graph reload and swaps both handles in via
/// [`Self::set_searcher`] / [`Self::set_graph`]. Every session clone shares
/// the same lock pair, so a swap is visible to all in-flight and future
/// sessions — the Rust form of the oracle's `mcpSrv.SetGraph`.
#[derive(Clone)]
pub struct Server {
    name: String,
    version: String,
    db: db::Db,
    searcher: Arc<RwLock<Arc<dyn Searcher + Send + Sync>>>,
    graph: Arc<RwLock<Arc<graph::GraphIndex>>>,
    tools: Vec<Tool>,
}

impl Server {
    /// Build the server. `name`/`version` come from config `server.*`
    /// (oracle `NewServer(cfg, ...)`); `graph` carries the config-driven
    /// Ready/Unavailable state — the Rust form of the oracle's `*graph.Graph`
    /// nil check (the task body's "Option&lt;Graph&gt;" is the graph crate's
    /// own `GraphIndex` modeling of config-driven optionality).
    pub fn new(
        name: String,
        version: String,
        db: db::Db,
        searcher: Arc<dyn Searcher + Send + Sync>,
        graph: Arc<graph::GraphIndex>,
    ) -> Self {
        Self {
            name,
            version,
            db,
            searcher: Arc::new(RwLock::new(searcher)),
            graph: Arc::new(RwLock::new(graph)),
            tools: tool_definitions(),
        }
    }

    /// Assemble the axum router (design D1/D5): dual transport on one
    /// instance.
    ///
    /// Explicit routes: `GET /health` (design D5), `GET /sse` + `POST
    /// /message` — the oracle's legacy HTTP+SSE wire contract (mcp-go
    /// v0.57.0) — plus the rmcp Streamable HTTP service as the fallback for
    /// every other path (design D8; the oracle mounted its SSE server at
    /// "/"). The SSE routes are mounted BEFORE the fallback; both transports
    /// share one `Arc<Server>` and one [`transport::SseSessionMap`], so a
    /// tool call served over either leg runs the same `Server::dispatch`
    /// seam.
    ///
    /// The legacy SSE leg was deliberately dropped by design D8 (2026-08-18)
    /// and restored by an explicit user decision (2026-08-31, D8 override):
    /// both transports are always on — no flags, no config (oracle parity:
    /// the oracle's SSE server was the only transport and had no transport
    /// switch).
    ///
    /// Axum consumes the SSE routes' state (`.with_state`) before the
    /// state-less `GET /health` route joins — its handler state
    /// (`HealthState`) is consumed per route.
    ///
    /// The idle reaper (design D9, task 1.5) is spawned once here from the
    /// shared [`transport::SseSessionMap`] — see the `spawn_reaper` call below
    /// for why its `JoinHandle` is not held.
    pub fn router(self) -> Router {
        let health_state = HealthState::new(self.db.clone(), self.version.clone());
        // One Arc<Server> shared by the Streamable HTTP factory and the SSE
        // routes (design D5: cheap clones per request).
        let server = Arc::new(self);
        let service = StreamableHttpService::new(
            {
                let server = server.clone();
                move || Ok(server.as_ref().clone())
            },
            Arc::new(LocalSessionManager::default()),
            // The oracle had no host validation (default bind 0.0.0.0);
            // rmcp's loopback-only default would break LAN access for a
            // personal server. Deviation recorded (design D8 keeps the
            // transport, not the oracle's missing validation).
            StreamableHttpServerConfig::default().disable_allowed_hosts(),
        );
        let sessions = transport::SseSessionMap::new();
        // Idle reaper (design D9, task 1.5): a detached process-lifetime task
        // that reaps sessions idle beyond the 300 s default every 30 s — a
        // general-service hardening the oracle (single local user) never
        // needed. The JoinHandle is deliberately NOT held: the reaper is
        // process-lifetime and self-terminating in effect (once the map
        // drains it removes nothing; process exit is the only shutdown,
        // design D6). `spawn_reaper` is context-tolerant — this assembly runs
        // on the cli's sync owner thread, outside any runtime context.
        sessions.clone().spawn_reaper();
        Router::new()
            .route("/sse", get(transport::sse::handle_sse))
            .route("/message", post(transport::sse::handle_message))
            .with_state(transport::SseState { sessions, server })
            .route("/health", get(health_handler).with_state(health_state))
            .fallback_service(service)
    }

    /// The injected database handle (cheap pool-handle clone).
    pub fn db(&self) -> &db::Db {
        &self.db
    }

    /// The current search contract handle (a clone of the active handle).
    #[must_use]
    pub fn searcher(&self) -> Arc<dyn Searcher + Send + Sync> {
        read_slot(&self.searcher)
    }

    /// The current knowledge-graph index handle (Ready/Unavailable per
    /// config; a clone of the active handle).
    #[must_use]
    pub fn graph(&self) -> Arc<graph::GraphIndex> {
        read_slot(&self.graph)
    }

    /// Replaces the active search handle (design D8 hot-swap seam).
    ///
    /// The search crate's hybrid searcher is immutable by design — there is
    /// no `SetGraph`-style mutation. The CLI (task 1.6) rebuilds the
    /// searcher after a knowledge-graph reload and swaps it in here; the
    /// swap is visible to every session clone (they share this lock).
    pub fn set_searcher(&self, searcher: Arc<dyn Searcher + Send + Sync>) {
        write_slot(&self.searcher, searcher);
    }

    /// Replaces the active knowledge-graph handle — the Rust form of the
    /// oracle's `mcpSrv.SetGraph`: after a graph reload the CLI swaps the
    /// freshly loaded index in so the graph tools serve the new state.
    pub fn set_graph(&self, graph: Arc<graph::GraphIndex>) {
        write_slot(&self.graph, graph);
    }

    /// The frozen tool registry (12 tools, `mcp-contract`).
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Dispatch a registered tool call (design D2 seam: parse args → call
    /// crate API → serialize the oracle-shaped payload). An unknown tool
    /// name never reaches this method — `call_tool` rejects it as a protocol
    /// error first; the catch-all arm is defense in depth.
    pub fn dispatch(&self, name: &str, args: Option<&Value>) -> Result<Value, McpError> {
        let searcher = self.searcher.read().unwrap_or_else(PoisonError::into_inner);
        let graph = self.graph.read().unwrap_or_else(PoisonError::into_inner);
        match name {
            "search" => tools::search::handle_search(&self.db, &**searcher, args),
            "catalog_overview" => tools::catalog::handle_catalog_overview(&self.db, args),
            "catalog_documents" => tools::catalog::handle_catalog_documents(&self.db, args),
            "catalog_entities" => tools::entities_catalog::handle_catalog_entities(&self.db, args),
            "search_entities_by_type" => {
                tools::entities_catalog::handle_search_entities_by_type(&self.db, args)
            }
            "search_facts" => tools::facts::handle_search_facts(&self.db, args),
            "get_fact_by_id" => tools::facts::handle_get_fact_by_id(&self.db, args),
            "get_document_context" => tools::documents::handle_get_document_context(&self.db, args),
            "get_chunk_by_id" => tools::documents::handle_get_chunk_by_id(&self.db, args),
            "get_entity_dossier" => {
                tools::dossier::handle_get_entity_dossier(&self.db, &graph, args)
            }
            "get_entity_relations" => {
                tools::graph_tools::handle_get_entity_relations(&self.db, &graph, args)
            }
            "get_entity_links" => tools::graph_tools::handle_get_entity_links(&self.db, args),
            // Defense in depth: unregistered names are rejected upstream.
            _ => Err(McpError::NotYetImplemented(name.to_owned())),
        }
    }
}

/// Reads the current handle from a hot-swap slot, recovering from a
/// poisoned lock: a swap can only poison the lock if a holder panics, and
/// the previous value is still intact and usable.
fn read_slot<T>(slot: &Arc<RwLock<T>>) -> T
where
    T: Clone,
{
    slot.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Replaces the handle in a hot-swap slot (poison recovery as in
/// [`read_slot`]).
fn write_slot<T>(slot: &Arc<RwLock<T>>, value: T) {
    *slot.write().unwrap_or_else(PoisonError::into_inner) = value;
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            // The oracle advertised tools.listChanged = true (server.go);
            // keep the same capability surface.
            .enable_tool_list_changed()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(self.name.clone(), self.version.clone()))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
        let name = request.name.clone();
        // Unknown tool: protocol error, not a tool error (oracle parity:
        // mcp-go returns METHOD_NOT_FOUND for unregistered tools).
        if !self
            .tools
            .iter()
            .any(|tool| tool.name.as_ref() == name.as_ref())
        {
            return Err(rmcp::model::ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("tool `{name}` not found"),
                None,
            ));
        }
        let args = request
            .arguments
            .as_ref()
            .map(|map| Value::Object(map.clone()));
        // The handler body is synchronous and may block (SQLite through the
        // pool, the ONNX embedding model, the vector index — the db/vectors/
        // embedding crate docs: those sync facades run only in sync
        // contexts or on `spawn_blocking` workers, never inside an async
        // task). One hop at the dispatch boundary covers all 12 tools and
        // keeps panics from crossing the handler boundary (design D7) —
        // the same precedent as `GET /health`.
        let server = self.clone();
        let dispatched = tokio::task::spawn_blocking(move || server.dispatch(&name, args.as_ref()))
            .await
            .map_err(|join_err| McpError::Internal(format!("tool dispatch failed: {join_err}")));
        match dispatched {
            Ok(Ok(payload)) => {
                // A serde_json::Value always serializes; the fallback keeps
                // the no-panic rule (design D7) without an unwrap.
                let text = serde_json::to_string(&payload).unwrap_or_else(|err| err.to_string());
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(text),
                ])))
            }
            Ok(Err(err)) | Err(err) => Ok(err.into_tool_result()),
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .cloned()
    }
}

/// One transcribed tool parameter (oracle `mcp.With*` property).
fn param(r#type: &str, description: &str, default: Option<Value>) -> Value {
    let mut property = json!({ "type": r#type, "description": description });
    if let Some(default) = default {
        property["default"] = default;
    }
    property
}

/// A frozen tool: name + description + input schema transcribed from
/// `../synopsis/internal/mcp/tools.go` (`mcp-contract`). Property and
/// required lists sort alphabetically in the serde_json BTreeMap — the same
/// wire order the Go oracle's JSON marshal produces. `annotations: {}` is
/// always present, as in mcp-go's output.
fn tool(
    name: &'static str,
    description: &'static str,
    props: &[(&str, Value)],
    required: &[&str],
) -> Tool {
    let properties: serde_json::Map<String, Value> = props
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    let schema = object(json!({
        "type": "object",
        "properties": properties,
        "required": required,
    }));
    Tool::new(name, description, Arc::from(schema)).annotate(ToolAnnotations::default())
}

/// The frozen 12-tool registry (`mcp-contract` "Набор инструментов"): names,
/// descriptions and parameter schemas transcribed verbatim from the Go
/// oracle `../synopsis/internal/mcp/tools.go`.
pub fn tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "search",
            "Performs hybrid search over the knowledge base combining lexical (FTS5/BM25) and semantic (vector/cosine) results via Reciprocal Rank Fusion. Returns ranked text chunks with full metadata including document_id, chunk_id, sequence_num, offsets, domains, score, and associated entities.",
            &[
                ("query", param("string", "Search query string", None)),
                (
                    "top_k",
                    param(
                        "number",
                        "Maximum number of results to return (default 10, range 1-100)",
                        Some(json!(10)),
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter results by domain (e.g., 'hr', 'product', 'engineering'). Empty or omitted = search all domains.",
                        None,
                    ),
                ),
            ],
            &["query"],
        ),
        tool(
            "catalog_overview",
            "Returns aggregate statistics about the knowledge base including document count, chunk count, entity count, fact count, documents by type, entities by type and domain, list of domains, entity types, and graph node/edge counts.",
            &[],
            &[],
        ),
        tool(
            "catalog_documents",
            "Lists documents in the knowledge base with cursor-based pagination. Supports filtering by domain, source type, and name (substring match on original_path). Returns document metadata including id, source_type, original_path, domain array, parsed metadata, created_at, and updated_at.",
            &[
                (
                    "page_size",
                    param(
                        "number",
                        "Number of items per page (1-200, default 20)",
                        Some(json!(20)),
                    ),
                ),
                (
                    "cursor",
                    param(
                        "string",
                        "Base64-encoded cursor for pagination. Omit or empty to start from the beginning.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Empty = all domains.",
                        None,
                    ),
                ),
                (
                    "source_type",
                    param(
                        "string",
                        "Filter by source type (e.g., 'json', 'markdown'). Empty = all types.",
                        None,
                    ),
                ),
                (
                    "name",
                    param(
                        "string",
                        "Filter documents by name substring match on original_path (case-insensitive).",
                        None,
                    ),
                ),
            ],
            &[],
        ),
        tool(
            "catalog_entities",
            "Lists entities in the knowledge base with cursor-based pagination. Supports filtering by entity type, domain, and name (substring match on entity name). Returns entity metadata including id, name, type, domain, description, confidence, and parsed metadata.",
            &[
                (
                    "page_size",
                    param(
                        "number",
                        "Number of items per page (1-200, default 20)",
                        Some(json!(20)),
                    ),
                ),
                (
                    "cursor",
                    param(
                        "string",
                        "Base64-encoded cursor for pagination. Omit or empty to start from the beginning.",
                        None,
                    ),
                ),
                (
                    "type",
                    param(
                        "string",
                        "Filter by entity type (e.g., 'employee', 'department'). Empty = all types.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Empty = all domains.",
                        None,
                    ),
                ),
                (
                    "name",
                    param(
                        "string",
                        "Filter entities by name substring match (case-insensitive).",
                        None,
                    ),
                ),
            ],
            &[],
        ),
        tool(
            "search_entities_by_type",
            "Searches entities by type with cursor-based pagination. Returns entity details including id, name, type, domain, description, confidence, and metadata.",
            &[
                (
                    "entity_type",
                    param(
                        "string",
                        "Entity type to filter by (e.g., 'employee', 'department', 'policy')",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Empty = all domains.",
                        None,
                    ),
                ),
                (
                    "page_size",
                    param(
                        "number",
                        "Number of items per page (1-200, default 20)",
                        Some(json!(20)),
                    ),
                ),
                (
                    "cursor",
                    param(
                        "string",
                        "Base64-encoded cursor for pagination. Omit or empty to start from the beginning.",
                        None,
                    ),
                ),
            ],
            &["entity_type"],
        ),
        tool(
            "search_facts",
            "Searches facts with optional filters including predicate (LIKE), entity_name (subject or object name match), status (default 'approved'), and domain. Returns fact details with entity names and cursor-based pagination.",
            &[
                (
                    "predicate",
                    param(
                        "string",
                        "Filter by predicate substring match (case-insensitive).",
                        None,
                    ),
                ),
                (
                    "entity_name",
                    param(
                        "string",
                        "Filter facts where subject OR object entity name matches this substring (case-insensitive).",
                        None,
                    ),
                ),
                (
                    "status",
                    param(
                        "string",
                        "Filter by fact status. Default: 'approved'. Use 'pending' or other statuses as needed.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Empty = all domains.",
                        None,
                    ),
                ),
                (
                    "page_size",
                    param(
                        "number",
                        "Number of items per page (1-200, default 20)",
                        Some(json!(20)),
                    ),
                ),
                (
                    "cursor",
                    param(
                        "string",
                        "Base64-encoded cursor for pagination. Omit or empty to start from the beginning.",
                        None,
                    ),
                ),
            ],
            &[],
        ),
        tool(
            "get_document_context",
            "Retrieves full document context including metadata, all chunks with offsets and sequence numbers, associated entities, and fact IDs. Supports selective inclusion of chunks, entities, and facts.",
            &[
                (
                    "document_id",
                    param(
                        "string",
                        "Document Predicate in the database (integer as string)",
                        None,
                    ),
                ),
                (
                    "include_chunks",
                    param(
                        "boolean",
                        "Include chunk data with offsets and text (default true)",
                        Some(json!(true)),
                    ),
                ),
                (
                    "include_entities",
                    param(
                        "boolean",
                        "Include entities associated with document chunks (default true)",
                        Some(json!(true)),
                    ),
                ),
                (
                    "include_facts",
                    param(
                        "boolean",
                        "Include fact IDs from approved facts linked to entity in this document (default false)",
                        Some(json!(false)),
                    ),
                ),
            ],
            &["document_id"],
        ),
        tool(
            "get_chunk_by_id",
            "Retrieves a single chunk by Predicate with full text, offsets, document metadata, and associated entities.",
            &[(
                "chunk_id",
                param(
                    "string",
                    "Chunk Predicate in the database (integer as string)",
                    None,
                ),
            )],
            &["chunk_id"],
        ),
        tool(
            "get_fact_by_id",
            "Retrieves a single fact by Predicate with subject/object entity details and source document quotes.",
            &[(
                "fact_id",
                param(
                    "string",
                    "Fact Predicate in the database (integer as string)",
                    None,
                ),
            )],
            &["fact_id"],
        ),
        tool(
            "get_entity_dossier",
            "Retrieves a complete entity dossier including all approved facts, source documents, related entities via graph traversal (BFS), and cross-domain links with provenance. Accepts either entity_id or entity_name (exactly one required). Only approved facts are included in expansion.",
            &[
                (
                    "entity_id",
                    param(
                        "string",
                        "Entity Predicate in the database (integer as string). Provide this OR entity_name, not both.",
                        None,
                    ),
                ),
                (
                    "entity_name",
                    param(
                        "string",
                        "Entity name for lookup. Case-insensitive exact match. Provide this OR entity_id, not both. Use type/domain to disambiguate if multiple entities share the same name.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Used with entity_name to disambiguate when multiple entities share the same name. Case-insensitive.",
                        None,
                    ),
                ),
                (
                    "depth",
                    param(
                        "number",
                        "BFS traversal depth for related entities, range 1-5 (default 2)",
                        Some(json!(2)),
                    ),
                ),
                (
                    "include_facts",
                    param(
                        "boolean",
                        "Include approved facts linked to this entity (default true)",
                        None,
                    ),
                ),
                (
                    "include_sources",
                    param(
                        "boolean",
                        "Include source documents for this entity (default true)",
                        None,
                    ),
                ),
            ],
            &[],
        ),
        tool(
            "get_entity_relations",
            "Traverses the knowledge graph from a given entity using BFS. Accepts either entity_id or entity_name (exactly one required). Returns connected nodes and edges up to the specified depth. Use include_cross_domain=true to follow cross-domain entity links.",
            &[
                (
                    "entity_id",
                    param(
                        "string",
                        "Entity Predicate in the database (integer as string). Provide this OR entity_name, not both.",
                        None,
                    ),
                ),
                (
                    "entity_name",
                    param(
                        "string",
                        "Entity name for lookup. Case-insensitive exact match. Provide this OR entity_id, not both. Use type/domain to disambiguate if multiple entities share the same name.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Used with entity_name to disambiguate when multiple entities share the same name. Case-insensitive.",
                        None,
                    ),
                ),
                (
                    "depth",
                    param(
                        "number",
                        "BFS traversal depth, range 1-10 (default 2)",
                        Some(json!(2)),
                    ),
                ),
                (
                    "include_cross_domain",
                    param(
                        "boolean",
                        "Follow cross-domain entity links during traversal (default false)",
                        None,
                    ),
                ),
            ],
            &[],
        ),
        tool(
            "get_entity_links",
            "Retrieves cross-domain entity links for a given entity with full provenance information including method (rule/equals/llm), confidence, and evidence. Accepts either entity_id or entity_name (exactly one required). Links are created automatically during ingestion.",
            &[
                (
                    "entity_id",
                    param(
                        "string",
                        "Entity Predicate in the database (integer as string). Provide this OR entity_name, not both.",
                        None,
                    ),
                ),
                (
                    "entity_name",
                    param(
                        "string",
                        "Entity name for lookup. Case-insensitive exact match. Provide this OR entity_id, not both. Use type/domain to disambiguate if multiple entities share the same name.",
                        None,
                    ),
                ),
                (
                    "domain",
                    param(
                        "string",
                        "Filter by domain (e.g., 'hr', 'product'). Used with entity_name to disambiguate when multiple entities share the same name. Case-insensitive.",
                        None,
                    ),
                ),
            ],
            &[],
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use db::test_util;
    use graph::GraphIndex;
    use search::{SearchError, SearchResult};
    use serde_json::Map;

    /// The 12 frozen tool names (`mcp-contract` "Набор инструментов"), in
    /// registration order.
    const FROZEN_NAMES: [&str; 12] = [
        "search",
        "catalog_overview",
        "catalog_documents",
        "catalog_entities",
        "search_entities_by_type",
        "search_facts",
        "get_document_context",
        "get_chunk_by_id",
        "get_fact_by_id",
        "get_entity_dossier",
        "get_entity_relations",
        "get_entity_links",
    ];

    /// A Searcher stub: the scaffold only needs an injectable handle.
    struct StubSearcher;

    impl Searcher for StubSearcher {
        fn hybrid_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Lexical("stub".to_owned()))
        }

        fn lexical_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Lexical("stub".to_owned()))
        }

        fn semantic_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Semantic("stub".to_owned()))
        }
    }

    /// A scaffold server over an empty in-memory KB with no graph.
    fn test_server() -> Server {
        Server::new(
            "synopsis-test".to_owned(),
            "0.1.0".to_owned(),
            test_util::in_memory_db(),
            Arc::new(StubSearcher),
            Arc::new(GraphIndex::Unavailable),
        )
    }

    #[test]
    fn tool_registry_matches_frozen_contract() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, FROZEN_NAMES);

        let schema = |name: &str| -> Value {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .unwrap()
                .schema_as_json_value()
        };

        // Property types transcribed from tools.go.
        for (tool, prop, r#type) in [
            ("search", "query", "string"),
            ("search", "top_k", "number"),
            ("search", "domain", "string"),
            ("catalog_documents", "page_size", "number"),
            ("catalog_documents", "cursor", "string"),
            ("catalog_documents", "domain", "string"),
            ("catalog_documents", "source_type", "string"),
            ("catalog_documents", "name", "string"),
            ("catalog_entities", "page_size", "number"),
            ("catalog_entities", "cursor", "string"),
            ("catalog_entities", "type", "string"),
            ("catalog_entities", "domain", "string"),
            ("catalog_entities", "name", "string"),
            ("search_entities_by_type", "entity_type", "string"),
            ("search_entities_by_type", "domain", "string"),
            ("search_entities_by_type", "page_size", "number"),
            ("search_entities_by_type", "cursor", "string"),
            ("search_facts", "predicate", "string"),
            ("search_facts", "entity_name", "string"),
            ("search_facts", "status", "string"),
            ("search_facts", "domain", "string"),
            ("search_facts", "page_size", "number"),
            ("search_facts", "cursor", "string"),
            ("get_document_context", "document_id", "string"),
            ("get_document_context", "include_chunks", "boolean"),
            ("get_document_context", "include_entities", "boolean"),
            ("get_document_context", "include_facts", "boolean"),
            ("get_chunk_by_id", "chunk_id", "string"),
            ("get_fact_by_id", "fact_id", "string"),
            ("get_entity_dossier", "entity_id", "string"),
            ("get_entity_dossier", "entity_name", "string"),
            ("get_entity_dossier", "domain", "string"),
            ("get_entity_dossier", "depth", "number"),
            ("get_entity_dossier", "include_facts", "boolean"),
            ("get_entity_dossier", "include_sources", "boolean"),
            ("get_entity_relations", "entity_id", "string"),
            ("get_entity_relations", "entity_name", "string"),
            ("get_entity_relations", "domain", "string"),
            ("get_entity_relations", "depth", "number"),
            ("get_entity_relations", "include_cross_domain", "boolean"),
            ("get_entity_links", "entity_id", "string"),
            ("get_entity_links", "entity_name", "string"),
            ("get_entity_links", "domain", "string"),
        ] {
            assert_eq!(
                schema(tool)["properties"][prop]["type"],
                json!(r#type),
                "{tool}.{prop}"
            );
        }

        // Defaults transcribed from tools.go.
        assert_eq!(
            schema("search")["properties"]["top_k"]["default"],
            json!(10)
        );
        for tool in [
            "catalog_documents",
            "catalog_entities",
            "search_entities_by_type",
            "search_facts",
        ] {
            assert_eq!(
                schema(tool)["properties"]["page_size"]["default"],
                json!(20),
                "{tool}"
            );
        }
        assert_eq!(
            schema("get_document_context")["properties"]["include_chunks"]["default"],
            json!(true)
        );
        assert_eq!(
            schema("get_document_context")["properties"]["include_entities"]["default"],
            json!(true)
        );
        assert_eq!(
            schema("get_document_context")["properties"]["include_facts"]["default"],
            json!(false)
        );
        assert_eq!(
            schema("get_entity_dossier")["properties"]["depth"]["default"],
            json!(2)
        );
        assert_eq!(
            schema("get_entity_relations")["properties"]["depth"]["default"],
            json!(2)
        );

        // Required fields transcribed from tools.go.
        assert_eq!(schema("search")["required"], json!(["query"]));
        assert_eq!(
            schema("search_entities_by_type")["required"],
            json!(["entity_type"])
        );
        assert_eq!(
            schema("get_document_context")["required"],
            json!(["document_id"])
        );
        assert_eq!(schema("get_chunk_by_id")["required"], json!(["chunk_id"]));
        assert_eq!(schema("get_fact_by_id")["required"], json!(["fact_id"]));
        for tool in [
            "catalog_overview",
            "catalog_documents",
            "catalog_entities",
            "search_facts",
            "get_entity_dossier",
            "get_entity_relations",
            "get_entity_links",
        ] {
            assert_eq!(
                schema(tool)["required"],
                json!(Vec::<String>::new()),
                "{tool}"
            );
        }

        // Wire shape invariants (mcp-go parity): inputSchema.type = object,
        // properties and required always present, annotations always {}.
        for t in &tools {
            let value = serde_json::to_value(t).unwrap();
            assert_eq!(value["inputSchema"]["type"], json!("object"));
            assert!(value["inputSchema"]["properties"].is_object());
            assert!(value["inputSchema"]["required"].is_array());
            assert_eq!(value["annotations"], json!({}));
            assert!(value["description"].is_string());
        }
    }

    #[test]
    fn get_info_advertises_tools_and_identity() {
        let server = test_server();
        let info = server.get_info();
        assert_eq!(info.server_info.name, "synopsis-test");
        assert_eq!(info.server_info.version, "0.1.0");
        let capabilities = serde_json::to_value(&info.capabilities).unwrap();
        assert_eq!(capabilities["tools"]["listChanged"], json!(true));
    }

    #[test]
    fn get_tool_resolves_registry_entries() {
        let server = test_server();
        assert!(server.get_tool("search").is_some());
        assert!(server.get_tool("no_such_tool").is_none());
    }

    /// Wiring test (replaces the task 5.9 stub test): dispatch routes the
    /// graph tools to their real handlers — the no-graph degradation error
    /// comes from the handler, not the stub — and the defensive catch-all
    /// still reports unregistered names as not-yet-implemented.
    #[test]
    fn dispatch_routes_graph_tools_to_real_handlers() {
        let server = test_server();

        // `get_entity_relations` reaches the handler: the Unavailable graph
        // degrades to the handler's not-found tool error.
        let err = server.dispatch("get_entity_relations", None).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        let result = err.into_tool_result();
        let rmcp::model::CallToolResponse::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));

        // `get_entity_links` reaches the handler: missing args are an
        // argument error, not the stub's not-yet-implemented.
        let err = server.dispatch("get_entity_links", None).unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );

        // The defensive catch-all is preserved for unregistered names.
        let err = server.dispatch("no_such_tool", None).unwrap_err();
        assert!(
            matches!(
                err,
                McpError::NotYetImplemented(ref name) if name == "no_such_tool"
            ),
            "got: {err:?}"
        );
    }

    /// A searcher that answers hybrid search with one canned result (the
    /// swap target for the hot-swap test).
    struct CannedSearcher;

    impl Searcher for CannedSearcher {
        fn hybrid_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Ok(vec![SearchResult {
                chunk_id: 7,
                chunk_text: "canned".to_owned(),
                chunk_metadata: Map::new(),
                document_id: 1,
                sequence_num: 0,
                start_offset: None,
                end_offset: None,
                document_path: String::new(),
                score: 1.0,
                rank: 1,
                source_type: "lexical".to_owned(),
                metadata: Map::new(),
                entities: Vec::new(),
            }])
        }

        fn lexical_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Ok(Vec::new())
        }

        fn semantic_search(
            &self,
            _query: &str,
            _top_k: i32,
            _domain: Option<&str>,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Ok(Vec::new())
        }
    }

    /// The design D8 hot-swap seam: a swapped-in searcher and graph are what
    /// `dispatch` serves from then on, and every session clone (the rmcp
    /// factory clones the server per session) shares the swapped handles.
    #[test]
    fn set_searcher_and_set_graph_swap_the_active_handles() {
        use graph::Graph;

        let server = test_server();

        // Before the swap: the stub's lexical error surfaces as a tool
        // error and the scaffold graph is Unavailable.
        let err = server
            .dispatch("search", Some(&json!({ "query": "q" })))
            .unwrap_err();
        assert!(
            matches!(err, McpError::Search(SearchError::Lexical(_))),
            "got: {err:?}"
        );
        assert!(!server.graph().is_available());

        // Swap the searcher: the search now resolves with the canned hit.
        server.set_searcher(Arc::new(CannedSearcher));
        let payload = server
            .dispatch("search", Some(&json!({ "query": "q" })))
            .unwrap();
        assert_eq!(payload["total_count"], json!(1));
        assert_eq!(payload["results"][0]["chunk_id"], json!(7));

        // Swap the graph: the server reports the new (Ready) handle.
        server.set_graph(Arc::new(GraphIndex::Ready(Graph::from_rows(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))));
        assert!(
            server.graph().is_available(),
            "the swapped graph must be served"
        );

        // A session clone sees the same swapped handles.
        let session = server.clone();
        assert!(session.graph().is_available());
        assert_eq!(
            session
                .dispatch("search", Some(&json!({ "query": "q" })))
                .unwrap()["total_count"],
            json!(1)
        );
    }

    /// Behavior test of the axum composition: `GET /health` is served by the
    /// explicit route (not the MCP fallback) with the design D5 shape. The
    /// full rmcp round-trip is task 5.10's integration test.
    #[tokio::test]
    async fn router_serves_health_on_explicit_route() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let response = test_server()
            .router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], "0.1.0");
        assert_eq!(json["sync_state"], "idle");
        // The scaffold test server has an empty in-memory KB.
        assert_eq!(json["counters"]["documents"], 0);
        assert_eq!(json["counters"]["chunks"], 0);
        assert_eq!(json["counters"]["entities"], 0);
        assert_eq!(json["counters"]["facts"], 0);
    }

    // --- dual-transport coexistence (add-legacy-sse-transport task 1.3) ---

    /// A temp fixture DB: 1 document + 2 chunks. The in-memory pool is the
    /// temp storage; the seeded rows give the `/health` leg real counters to
    /// assert (the same seed shape as `tests/server_integration.rs`).
    fn fixture_db() -> db::Db {
        use db::{ChunkDao, ConnectionOrTx, DocumentDao};

        let db = test_util::in_memory_db();
        let seeded = db.with_conn(|conn| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let doc =
                DocumentDao::new(exec).create("markdown", "/docs/hr-policy.md", None, None)?;
            let chunks = ChunkDao::new(exec);
            chunks.create(doc, "quarterly hiring policy", 0, Some(0), Some(25))?;
            chunks.create(doc, "vacation rules", 1, None, None)?;
            Ok(())
        });
        seeded.expect("pool checkout").expect("seed rows");
        db
    }

    /// The coexistence test server: the fixture DB, the stub searcher, no
    /// graph (identity `synopsis-router-test 9.9.9`).
    fn fixture_server() -> Server {
        Server::new(
            "synopsis-router-test".to_owned(),
            "9.9.9".to_owned(),
            fixture_db(),
            Arc::new(StubSearcher),
            Arc::new(GraphIndex::Unavailable),
        )
    }

    /// Reads the next complete SSE frame (terminated by the blank line) from
    /// a body data stream, buffering partial chunks (hyper may split or
    /// coalesce frames across chunk boundaries).
    async fn next_sse_frame<S>(data: &mut S, buffer: &mut String) -> String
    where
        S: tokio_stream::StreamExt<Item = std::result::Result<axum::body::Bytes, axum::Error>>
            + Unpin,
    {
        loop {
            if let Some(end) = buffer.find("\n\n") {
                let frame = buffer[..end].to_owned();
                *buffer = buffer[end + 2..].to_owned();
                return frame;
            }
            let Some(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_secs(2), data.next())
                    .await
                    .expect("a frame arrives in time")
            else {
                panic!("the stream ended before a complete frame");
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// Dual-transport coexistence on the REAL `Server::router()` (task 1.3
    /// acceptance): one router serves `GET /health`, the legacy SSE pair
    /// (`GET /sse` + `POST /message`), and the Streamable HTTP fallback
    /// (rmcp client) at the same time. The oneshot legs run on clones of the
    /// same router — axum `Router` clones share the router state (one
    /// `Arc<Server>`, one session map) — and the rmcp leg serves the
    /// original router value on a listener.
    #[tokio::test]
    async fn router_serves_health_sse_and_streamable_http_together() {
        use axum::body::Body;
        use axum::http::header;
        use axum::http::{Request, StatusCode};
        use rmcp::ServiceExt as _;
        use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
        use rmcp::transport::StreamableHttpClientTransport;
        use tower::ServiceExt as _;

        let router = fixture_server().router();

        // 1. GET /health → 200 with the seeded counters (existing behavior
        //    unchanged).
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let health: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], "9.9.9");
        assert_eq!(health["counters"]["documents"], json!(1));
        assert_eq!(health["counters"]["chunks"], json!(2));

        // 2. GET /sse → 200 text/event-stream; the first frame is the
        //    endpoint event (proxy-aware absolute URL, no proxy headers).
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sse")
                    .header(header::HOST, "localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let mut data = response.into_body().into_data_stream();
        let mut buffer = String::new();
        let endpoint = next_sse_frame(&mut data, &mut buffer).await;
        assert!(
            endpoint.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
            "got: {endpoint}"
        );
        let session_id = endpoint
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .and_then(|url| url.split("sessionId=").nth(1))
            .expect("the endpoint URL carries a sessionId")
            .to_owned();

        // 3. POST /message?sessionId → 202 Accepted; the initialize response
        //    arrives as an `event: message` frame on the same stream.
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        });
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/message?sessionId={session_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&initialize).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        drop(response);
        let frame = next_sse_frame(&mut data, &mut buffer).await;
        assert!(frame.starts_with("event: message\n"), "got: {frame}");
        let reply: Value = serde_json::from_str(
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("the message frame carries data"),
        )
        .unwrap();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], json!(1));
        assert_eq!(
            reply["result"]["serverInfo"]["name"],
            "synopsis-router-test"
        );

        // 4. Streamable HTTP on the same router instance: the rmcp client
        //    (the tests/server_integration.rs pattern) completes initialize
        //    + tools/list.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
        let mut client = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new(
                "synopsis-router-coexistence-test",
                env!("CARGO_PKG_VERSION"),
            ),
        )
        .serve(transport)
        .await
        .unwrap();
        let peer = client.peer_info().expect("peer info after initialize");
        assert_eq!(
            peer.server_info.as_ref().expect("server info").name,
            "synopsis-router-test"
        );
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 12);
        client.close().await.unwrap();

        // Drop the SSE stream: the client disconnect removes the session.
        drop(data);
    }

    // --- idle reaper on the real router (add-legacy-sse-transport task 1.5) ---

    /// The idle reaper is live on the REAL `Server::router()` (task 1.5
    /// acceptance): the threshold is not injectable through `Server` (no
    /// config surface is invented — the oracle has none), so the production
    /// defaults (300 s threshold / 30 s tick, design D9) are exercised on a
    /// paused clock: a `/sse` session with no activity is reaped at the first
    /// reaper pass past the threshold (t=330 s), and the client observes the
    /// stream end. Short-threshold behavior is covered by the map-level unit
    /// tests in `transport/sse.rs`.
    #[tokio::test(start_paused = true)]
    async fn router_spawns_idle_reaper() {
        use axum::body::Body;
        use axum::http::header;
        use axum::http::{Request, StatusCode};
        use tokio_stream::StreamExt as _;
        use tower::ServiceExt as _;

        let router = test_server().router();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sse")
                    .header(header::HOST, "localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut data = response.into_body().into_data_stream();
        let first = data
            .next()
            .await
            .expect("the endpoint frame arrives")
            .expect("no frame error");
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(
            first.starts_with("event: endpoint\ndata: http://localhost:3000/message?sessionId="),
            "got: {first}"
        );

        // The production defaults (design D9): 300 s threshold, 30 s tick —
        // the first reaping pass lands at t=330 s. The paused clock
        // auto-advances through each reaper tick as the test sleeps (no real
        // waiting); sleeping to t=331 s guarantees that pass is complete.
        tokio::time::sleep(std::time::Duration::from_secs(331)).await;
        // The reaper dropped the session's sender: the stream ends (EOF).
        assert!(
            data.next().await.is_none(),
            "the idle session on the real router must be reaped"
        );
    }
}
