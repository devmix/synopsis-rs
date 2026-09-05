# db-storage Specification

## MODIFIED Requirements

### Requirement: DAO operations over the v5 schema

The DAO layer covers the v5-schema tables: documents, chunks, entities, facts, links (chunk_entities, entity_links, entity_sources, fact_sources). The `app_kv` table is **NOT** a knowledge-DB DAO target: it lives in the global cache DB (`migrations/cache/1-init/up.sql`). Operation behavior is fixed by semantics: CRUD, pagination with filters (domain via json_each, source_type, name), batch operations (IN-lists with placeholders, batches ≤ 500 rows), orphan cleanup (does not delete EntityType or fact references), GetOrCreate/CreateOrIgnore — atomic via UNIQUE constraints and `ON CONFLICT` (fixing the TOCTOU race). The SQLite parameter limit (32766) is not exceeded (batches ≤ 500×2 parameters).

#### Scenario: Document CRUD
- **WHEN** the DAO creates, reads, updates, and deletes a document
- **THEN** all operations return correct data; re-reading a deleted document yields None

#### Scenario: Pagination with filters
- **WHEN** the DAO requests a page of documents/entities with domain/source_type/name filters
- **THEN** only the items satisfying the filters are returned, in the fixed order, with correct offset/limit

#### Scenario: Atomic GetOrCreate
- **WHEN** two GetOrCreate calls with identical keys (type, name, domain) run concurrently
- **THEN** exactly one record is created and both calls return the same ID (no race)

#### Scenario: Batch operations
- **WHEN** the DAO performs a batch operation (GetByIDs, LinkBatch, DeleteByIDs) with a large list
- **THEN** the operation completes correctly without exceeding the SQLite parameter limit (batches ≤ 500 rows)

#### Scenario: Orphan cleanup
- **WHEN** the DAO deletes orphaned entities/facts
- **THEN** EntityType and the entities/facts referenced by other records are not deleted
