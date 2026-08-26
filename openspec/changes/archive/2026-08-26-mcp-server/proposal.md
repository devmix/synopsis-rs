# Proposal: mcp-server

## Change name
`mcp-server`

## Why

The knowledge base is complete and searchable; nothing exposes it yet. The MCP
layer is the product surface: exactly 12 read-only tools consumed by LLM agents.
The frozen contract (`openspec/specs/mcp-contract/spec.md`, transcribed from
`../synopsis/internal/mcp/tools.go`) defines names, parameter schemas, response
shapes, approved-only fact filtering, and the modernized transport. This change
implements that contract; it does NOT change it.

Transport per frozen decision D8 (2026-08-18): rmcp 3.x over Streamable HTTP;
the oracle's legacy SSE (`GET /sse` + `POST /message`) is deliberately not
ported. Parity lives at the tool-response level.

## What changes

1. **Server wiring** — rmcp Streamable HTTP server + axum-mounted `GET /health`
   (status/version/counters, structure compatible with the Go binary), tool
   registry for the 12 tools, JSON error mapping to MCP tool errors.
2. **Cursor pagination helper** — port of `handlers/pagination.go` semantics
   (opaque cursor, limit handling) shared by the paginated tools.
3. **Tool handlers** (thin functions over existing crates — search, db DAOs,
   graph): search; catalog_overview; catalog_documents; catalog_entities;
   search_entities_by_type; search_facts; get_document_context; get_chunk_by_id;
   get_fact_by_id; get_entity_dossier; get_entity_relations; get_entity_links.
   Each with handler-level tests against in-memory SQLite mirroring the oracle's
   handler tests.
4. **Integration test** — round-trip through the parity-harness rmcp client
   pattern against the running server: initialize → tools/list returns exactly
   12 → call representative tools → structured results.

## Non-goals

- No write tools (frozen contract: read-only at this stage).
- No legacy SSE transport (frozen D8).
- No auth/middleware beyond what the oracle has (its middleware.go is a request
  logger — covered by eprintln convention or omitted; decide at implementation).
- No fixture recording from the Go binary in THIS change: handler tests pin
  semantics against oracle Go-test expectations; on-disk fixture parity runs are
  the parity-harness change's concern once a real knowledge.db exists.

## Risks

- rmcp 3.x API surface (tool macros vs manual schema) must produce tools/list
  schemas matching the frozen contract — pinned by a schema test.
- /health must be served alongside the MCP endpoint — axum router composition
  needs verification against rmcp's Streamable HTTP service shape.
