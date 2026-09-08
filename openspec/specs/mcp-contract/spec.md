# mcp-contract Specification

## Purpose

The external contract of the Synopsis MCP API: the tool set, their parameters and responses, and the transport. Tool contracts (names, JSON schemas, semantics) are fixed by this contract — clients (LLM agents) must not notice implementation changes in the data. The transport is dual: Streamable HTTP (official SDK rmcp 3.x, design D8) + legacy HTTP+SSE (override of D8, human decision 2026-08-31; wire contract mcp-go v0.57.0).

## Requirements

### Requirement: Tool set

The system provides exactly 12 MCP tools with the following names: `search`, `catalog_overview`, `catalog_documents`, `catalog_entities`, `search_entities_by_type`, `search_facts`, `get_document_context`, `get_chunk_by_id`, `get_fact_by_id`, `get_entity_dossier`, `get_entity_relations`, `get_entity_links`.

#### Scenario: Full tool list
- **WHEN** a client requests tools/list from the Rust server
- **THEN** the response contains exactly these 12 names, with no extras and no missing ones

### Requirement: Tool parameter and response schemas

For each tool, the field names, types, and optionality of the parameters from the JSON schemas (tools/list) and the result structure (field names, typing, presence/absence of optional fields for the same inputs) are fixed by this contract and the recorded fixtures.

#### Scenario: Tool JSON schemas
- **WHEN** a machine-diff of the Rust server's tools/list response against the fixture recorded once on the same knowledge.db (with description normalization) is run
- **THEN** the parameter schemas of each of the 12 tools are identical

#### Scenario: Response parity on identical data
- **WHEN** a Rust server tool is called with the arguments from a fixture (recorded once on the same knowledge.db)
- **THEN** the result matches the response recorded in the fixture in structure and content, except for fields explicitly marked approximate (ANN scores, see data/search specs), where a deviation within the recall gate is allowed

### Requirement: Transport

The server provides the MCP protocol over HTTP on two transports simultaneously (both always enabled, no flags or settings): (1) Streamable HTTP (MCP spec ≥ 2025-11-25; official SDK rmcp 3.x, design D8) — a single endpoint for POST JSON-RPC with the response as plain JSON or an SSE stream; (2) legacy HTTP+SSE (mcp-go v0.57.0 SSEServer — wire contract, override of D8 by human decision 2026-08-31): `GET /sse` → `200 text/event-stream`, the first frame is `event: endpoint` with data = absolute URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host from `X-Forwarded-Proto`/`X-Forwarded-Host`, otherwise `http` + the `Host` header; server-generated UUIDv4), then `event: message` frames with JSON-RPC 2.0; `POST /message?sessionId=<id>` → `202 Accepted` (empty body), responses arrive over the SSE stream; missing/unknown sessionId → `400`; keep-alive is disabled (default); a session lives until the SSE stream is closed, 300 s of inactivity (idle reaper — an extension of the wire contract for the service model, human decision 2026-09-01), or the server performs a graceful shutdown (the server terminates all active legacy SSE sessions — the streams end and the clients see a clean EOF — so the HTTP drain completes within the bounded shutdown; an extension of the wire contract, human decision 2026-09-08); the wildcard `Access-Control-Allow-Origin` header is NOT issued (a deviation from the wire contract, security of a shared service, human decision 2026-09-01). The JSON-RPC methods of the legacy transport — tools-only semantics of mcp-go: `initialize` (result: protocolVersion/capabilities/serverInfo — serverInfo matches the name/version of the Streamable HTTP path), `notifications/initialized` (no response), `ping` (`{}`), `tools/list` (the same 12 tools), `tools/call` (the same `Server::dispatch`; tool error → result with `isError: true`, protocol error → JSON-RPC error); unknown method → `-32601`. Additionally: `GET /health` — status, version, and knowledge base counters; the health endpoint response structure is fixed by this contract.

#### Scenario: Connecting a modern MCP client
- **WHEN** an MCP client (Streamable HTTP) performs initialize and tools/list against the Rust server
- **THEN** the handshake completes with an agreed protocol version, and tools/list returns the 12 tools

#### Scenario: Health endpoint
- **WHEN** GET /health after a successful start
- **THEN** 200 with the structure (status/version/sync state/counters) fixed by this contract

#### Scenario: Connecting a legacy SSE client
- **WHEN** an MCP client (legacy SSE) performs GET /sse
- **THEN** 200 text/event-stream; the first frame is `event: endpoint` with data = absolute URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host from `X-Forwarded-Proto`/`X-Forwarded-Host`, otherwise `http` + `Host`)

#### Scenario: Session idle timeout
- **WHEN** a legacy SSE session receives no JSON-RPC request for 300 seconds
- **THEN** the reaper closes the session (the SSE stream ends); subsequent POSTs for this sessionId get 400

#### Scenario: Sessions on graceful shutdown
- **WHEN** the server performs a graceful shutdown (SIGINT/SIGTERM) and a legacy SSE session is active
- **THEN** the server terminates the session (the SSE stream ends, the client sees a clean EOF), the HTTP drain completes within the bounded shutdown, and the process exits with code 0

#### Scenario: Tool call via legacy SSE
- **WHEN** a legacy SSE client sends a `tools/call` to POST /message?sessionId=<id>
- **THEN** the server answers 202 Accepted, and the result arrives as an `event: message` frame (JSON-RPC response) over the SSE stream of the same session; the same call over Streamable HTTP returns identical JSON (except fields marked approximate)

#### Scenario: Invalid session
- **WHEN** POST /message?sessionId=<id> with a missing or unknown sessionId
- **THEN** 400 Bad Request; existing sessions are unaffected

#### Scenario: Connection drop
- **WHEN** a legacy SSE client closes the GET /sse connection
- **THEN** the session is removed from the registry; subsequent POSTs for this sessionId get 400

### Requirement: Read-only, fact statuses — approved only

All read results contain only facts with `status = 'approved'` (pending is available only in the tools where that behavior is documented). Write tools are NOT provided at this stage (write tools — a separate future change).

#### Scenario: approved-only filter
- **WHEN** knowledge.db has facts with status pending and approved, and a client requests search/dossier results
- **THEN** the results contain only approved facts

### Requirement: search result carries chunk metadata
The `search` tool's result item SHALL carry a `metadata` field holding the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) as a raw JSON object — the `SearchResult.Metadata` field, which was previously dropped. The bag SHALL be passed through uncured; an empty bag SHALL be omitted from the JSON. The `text` field's type is unchanged (a string) — only its content is now the chunk's pure body (`chunk_text`) rather than the breadcrumb-prefixed `search_text`. Of the *document-level* keys in `SearchResult.Metadata`, `updated_at` is now exposed as a top-level result field (see "search result carries document freshness"); the remaining keys (`document_source_type`, `document_metadata_json`) are still a deferred concern and SHALL NOT be part of the `metadata` bag.

#### Scenario: Sectioned chunk metadata in the response
- **WHEN** a search returns a chunk produced under a heading hierarchy
- **THEN** the result item's `metadata` object carries `section_title`, `heading_level`, and `breadcrumb`

#### Scenario: Empty bag omitted
- **WHEN** a search returns a chunk with no chunk-specific metadata
- **THEN** the result item has no `metadata` field (omitted, not `{}`)

#### Scenario: text is the pure body
- **WHEN** a search returns a chunk
- **THEN** the result item's `text` is the chunk's pure body (the byte-offset slice), and the section context is in `metadata`, not glued into `text`

### Requirement: search result carries document freshness
The `search` tool's result item SHALL carry an `updated_at` field holding the owning document's `updated_at` normalized to RFC3339 (the value the enricher already computes into the result's enrichment bag). The field SHALL be omitted from the JSON when the document has no parseable timestamp. This is a deliberate, additive divergence from the legacy MCP wire item, which does not expose `updated_at` per result: a RAG client can now see hit freshness without a second `get_document_context` call. It is distinct from the chunk's own `metadata` bag — `updated_at` is a top-level result field, not a bag key. The parity harness strips `updated_at` from the `search` response before comparison (it is a non-deterministic timestamp), so content parity is unaffected.

#### Scenario: Document with a parseable timestamp
- **WHEN** a search returns a chunk whose document has a parseable `updated_at`
- **THEN** the result item's `updated_at` is that timestamp in RFC3339 form

#### Scenario: Document with no parseable timestamp
- **WHEN** a search returns a chunk whose document has no (or unparseable) `updated_at`
- **THEN** the result item has no `updated_at` field (omitted, not `null`)
