# data-schema Specification

## MODIFIED Requirements

### Requirement: Compatibility with the v5 schema

The Rust binary builds knowledge.db from scratch (v5 schema after migrations 001–005). The tables `documents`, `chunks`, `entities`, `chunk_entities`, `facts`, `fact_sources`, `entity_sources`, `entity_links`, the FTS5 table `chunks_fts`, and the indexes are present and used identically: the same queries yield the same results. The `app_kv` table is **NOT** in the knowledge DB: it lives in the global cache DB (`<workspace_dir>/db/cache/cache.db`, created by `migrations/cache/1-init/up.sql`) alongside the LLM response caches (`llm_ner_cache`, `llm_linker_cache`).

#### Scenario: Fresh-DB startup
- **WHEN** the Rust server starts with a fresh knowledge.db built from scratch (sync finished)
- **THEN** the migrations do not rewrite data; catalog_overview returns counters computed from the same DB's data

#### Scenario: Read-query repeatability
- **WHEN** the same read queries are sent to the Rust binary (the same DB copy) through the MCP tools
- **THEN** the results match the recorded fixtures (except the ANN fields, see the recall gates)

#### Scenario: app_kv lives in the cache DB
- **WHEN** the knowledge DB and the cache DB are inspected after a fresh build
- **THEN** the knowledge DB has no `app_kv` table, and the cache DB (`migrations/cache/1-init/up.sql`) has `app_kv`
