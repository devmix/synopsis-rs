# Design: mcp-server

Oracle references (read-only): `../synopsis/internal/mcp/{tools.go,server.go,
middleware.go}`, `../synopsis/internal/mcp/handlers/*.go` (+ tests).
Frozen contract: `openspec/specs/mcp-contract/spec.md` — implemented, not amended.

## D1 — rmcp 3.x server, axum-mounted, single port

rmcp's Streamable HTTP service composes into an axum router; `GET /health` is a
plain axum handler on the same listener (frozen contract requires both on one
server). Server construction takes injected collaborators (Db handle, Searcher,
Graph handle optional per config) — same injection discipline as the pipeline.
The mcp crate depends on {config, db, search, graph} per D1 layering.

Tool schemas: prefer rmcp's tool macro if it can express the frozen parameter
schemas exactly; otherwise build `rmcp::model::Tool` objects manually from
serde_json schemas transcribed from tools.go. A unit test pins tools/list
against the frozen contract names + required/optional field shapes.

## D2 — Handlers are thin: parse args → call crate API → serialize

Each handler: deserialize the tool arguments struct (serde, matching the frozen
schema field names), invoke the existing crate API (Searcher methods, DAOs,
graph traverser/link queries), map results into the oracle response JSON shape
(serde-tagged structs or json! literals where shapes are one-off). No business
logic lives in mcp. Handler errors become MCP tool errors (isError content) with
the message text; argument-parse failures likewise.

Approved-only fact filtering is enforced by the existing DAOs (fact queries
already filter status='approved' where the oracle does) — handlers must not
loosen it.

## D3 — Cursor pagination (port of pagination.go)

Opaque cursor = base64 of the last-seen sort key; limit clamped to the oracle's
bounds; response carries next_cursor only when more rows exist. Shared helper
used by catalog_documents, catalog_entities, search_entities_by_type,
search_facts. Differential parity against oracle handler tests that exercise
pagination.

## D4 — Tool-by-tool mapping

| Tool | Backing API |
|---|---|
| search | Searcher::hybrid_search (+ lexical/semantic via args if the frozen schema exposes mode) |
| catalog_overview | count queries across documents/entities/facts/chunks (DAO counts) |
| catalog_documents | DocumentDao list_paginated |
| catalog_entities | EntityDao paginated listing |
| search_entities_by_type | EntityDao by-type query + pagination |
| search_facts | FactDao filtered query + pagination |
| get_document_context | DocumentDao + ChunkDao by doc + chunk entities + fact ids |
| get_chunk_by_id | ChunkDao get + document info + entities |
| get_fact_by_id | FactDao get + entities + sources |
| get_entity_dossier | EntityDao resolve (id or name) + facts + sources + related + cross-domain links |
| get_entity_relations | graph traverser from entity id/name |
| get_entity_links | entity_link DAO queries with provenance |

Exact response field names/shapes are transcribed from the oracle handlers and
pinned by tests asserting the JSON structure (keys, types, optionality).

## D5 — /health

axum GET /health returns the Go-compatible structure: status ("ok"), version
(crate version), sync state placeholder, knowledge-base counters (documents/
chunks/entities/facts). Shape pinned by a test asserting keys/types.

## D6 — Testing strategy

- Handler tests: in-memory SQLite seeded via DAOs (db test_util pattern),
  invoking handler functions directly with parsed args — mirroring the oracle's
  handler_test.go cases (happy path, not-found, invalid args, pagination edges).
- Schema test: tools/list output vs frozen contract (names exact set, required
  fields present).
- Integration test: spawn the axum+rmcp server on an ephemeral port, connect
  with the parity-harness rmcp client pattern, initialize → tools/list → call
  search + one catalog tool → structured assertions.

## D7 — Errors

MCP tool error results carry human-readable messages; internal errors wrap
crate errors (thiserror McpError). Panics never cross the handler boundary.
