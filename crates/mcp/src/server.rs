//! MCP server: rmcp 3.x Streamable HTTP transport composed into an axum
//! router (design D1/D8) with the frozen 12-tool registry (`mcp-contract`).
//!
//! Oracle mapping: `../synopsis/internal/mcp/{server.go,tools.go}`. The Go
//! code is the behavior/contract reference only — this is the Rust
//! re-architecture (functional copy, not a code copy): the legacy SSE
//! transport is deliberately not ported (design D8), tool schemas are
//! transcribed from `tools.go` as `rmcp::model::Tool` objects, and the
//! remaining handler bodies are stubs until tasks 5.7–5.9 fill them
//! (design D2).

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
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

/// The MCP server: injected collaborators (design D1) + the frozen tool
/// registry. Cloned per session by the rmcp service factory.
#[derive(Clone)]
pub struct Server {
    name: String,
    version: String,
    db: db::Db,
    searcher: Arc<dyn Searcher + Send + Sync>,
    graph: Arc<graph::GraphIndex>,
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
            searcher,
            graph,
            tools: tool_definitions(),
        }
    }

    /// Assemble the axum router (design D1): `GET /health` is an explicit
    /// route, and the rmcp Streamable HTTP service is the fallback serving
    /// every other path (the oracle mounted its transport at "/").
    pub fn router(self) -> Router {
        let health_state = HealthState::new(self.db.clone(), self.version.clone());
        let service = StreamableHttpService::new(
            {
                let server = Arc::new(self);
                move || Ok(server.as_ref().clone())
            },
            Arc::new(LocalSessionManager::default()),
            // The oracle had no host validation (default bind 0.0.0.0);
            // rmcp's loopback-only default would break LAN access for a
            // personal server. Deviation recorded (design D8 keeps the
            // transport, not the oracle's missing validation).
            StreamableHttpServerConfig::default().disable_allowed_hosts(),
        );
        Router::new()
            .route("/health", get(health_handler).with_state(health_state))
            .fallback_service(service)
    }

    /// The injected database handle (cheap pool-handle clone).
    pub fn db(&self) -> &db::Db {
        &self.db
    }

    /// The injected search contract handle.
    pub fn searcher(&self) -> &Arc<dyn Searcher + Send + Sync> {
        &self.searcher
    }

    /// The injected knowledge-graph index (Ready/Unavailable per config).
    pub fn graph(&self) -> &Arc<graph::GraphIndex> {
        &self.graph
    }

    /// The frozen tool registry (12 tools, `mcp-contract`).
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Dispatch a registered tool call (design D2 seam: parse args → call
    /// crate API → serialize the oracle-shaped payload). Tasks 5.7–5.9
    /// replace the remaining stubs with real handlers. An unknown tool
    /// name never reaches this method — `call_tool` rejects it as a
    /// protocol error first.
    pub fn dispatch(&self, name: &str, args: Option<&Value>) -> Result<Value, McpError> {
        match name {
            "search" => tools::search::handle_search(&self.db, &*self.searcher, args),
            "catalog_overview" => tools::catalog::handle_catalog_overview(&self.db, args),
            "catalog_documents" => tools::catalog::handle_catalog_documents(&self.db, args),
            "catalog_entities" => tools::entities_catalog::handle_catalog_entities(&self.db, args),
            "search_entities_by_type" => {
                tools::entities_catalog::handle_search_entities_by_type(&self.db, args)
            }
            "search_facts" => tools::facts::handle_search_facts(&self.db, args),
            "get_fact_by_id" => tools::facts::handle_get_fact_by_id(&self.db, args),
            // Stub until tasks 5.7–5.9: the tool reports "not implemented
            // yet" as an MCP tool error (design D2/D7).
            _ => Err(McpError::NotYetImplemented(name.to_owned())),
        }
    }
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
        match self.dispatch(&name, args.as_ref()) {
            Ok(payload) => {
                // A serde_json::Value always serializes; the fallback keeps
                // the no-panic rule (design D7) without an unwrap.
                let text = serde_json::to_string(&payload).unwrap_or_else(|err| err.to_string());
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(text),
                ])))
            }
            Err(err) => Ok(err.into_tool_result()),
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

    #[test]
    fn dispatch_stub_reports_not_implemented_as_tool_error() {
        let server = test_server();
        // `get_document_context` is still a stub (task 5.7).
        let err = server.dispatch("get_document_context", None).unwrap_err();
        assert!(
            matches!(err, McpError::NotYetImplemented(ref name) if name == "get_document_context"),
            "got: {err:?}"
        );
        let result = err.into_tool_result();
        let rmcp::model::CallToolResponse::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));
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
}
