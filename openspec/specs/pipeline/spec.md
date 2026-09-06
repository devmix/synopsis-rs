# pipeline Specification

## Purpose

Orchestration of "source → knowledge": the Ingester takes each document through chunk → embeddings → NER → transactional write with deduplication by hash; the Runner executes the configured sources (full run, incremental sync, prune, orphan cleanup, cross-domain linking).

## Requirements

### Requirement: Ingestion progress

The pipeline publishes run statistics: files processed, chunks and embeddings created, entities extracted, facts and fact sources created, documents created/updated/skipped, errors, run time. Progress is displayed by an indicator (indicatif).

#### Scenario: Statistics after a run
- **WHEN** ingestion of a source completes
- **THEN** the returned statistics reflect the counters of all stages and the number of errors

### Requirement: Document deduplication by hash

Each document is identified by its path; the SHA-256 of the content is compared with the stored one — an unchanged document is skipped entirely (the updated counter does not grow), a changed one is updated with a full cleanup of the document's old data before the new data is written.

#### Scenario: Unchanged document
- **WHEN** a document with the same path and hash is already indexed
- **THEN** it is skipped without a database write and lands in skipped

#### Scenario: Changed document
- **WHEN** the document's hash differs from the stored one
- **THEN** the document's old chunks/relations/facts are deleted cascadingly, and the new data is written atomically in a single transaction

### Requirement: Document processing pipeline

A document goes through the stages: chunk → embeddings in batches (batch_size, default 100; a mismatch in the number of vectors is an error) → NER over the chunks (if a provider is configured and NER is not disabled) → a single SQLite transaction: document, chunks, entity resolution + chunk_entities relations, synthetic fact entities, facts. An error of an individual document increments the error counter and does not abort the run.

#### Scenario: Batched embeddings
- **WHEN** there are more chunks than batch_size
- **THEN** they are processed in sequential batches; a mismatch between the number of vectors and texts ends the document with an error

#### Scenario: Single-document error
- **WHEN** processing of a document fails
- **THEN** the error is counted in the statistics, and the remaining documents continue to be processed

### Requirement: Facts at ingestion

For each fact: the subject and the object are resolved as synthetic entities (lookup_or_create) and linked to the chunk; a cross-domain fact is dropped with a warning; the fact is created with create_or_ignore with metadata; a source with a quote is stored for the fact — a rune-aware window around the first occurrence of the subject's or object's name (±60 runes, fallback 120, trimmed at a line boundary, "..." suffix); the weights of the affected facts are recomputed.

#### Scenario: Cross-domain fact
- **WHEN** the subject and the object of a fact belong to different domains
- **THEN** the fact is not created, and a warning is recorded

#### Scenario: Fact source quote
- **WHEN** an entity name is found in a chunk's text
- **THEN** the quote is cut around the first occurrence, trimmed at a line boundary, with "..." when truncated

### Requirement: Vectors outside the transaction

Vectors are written to the ANN index after the SQLite transaction commits (the chunk in the DB is the source of truth); the mismatch is reconciled by the orphan cleanup: vectors without a live chunk are deleted.

#### Scenario: Orphan-vector reconciliation
- **WHEN** data cleanup is performed
- **THEN** vectors whose chunks do not exist in the DB are removed from the index

### Requirement: Backup and rebuild

Before indexing, a WAL snapshot of the DB is created via VACUUM INTO into the backups directory (a snapshot failure is a warning, not a failure; for an in-memory DB the snapshot is skipped). A rebuild deletes all documents under a source root in a single transaction before the files are parsed.

#### Scenario: Source rebuild
- **WHEN** ingestion is started with rebuild
- **THEN** all previously indexed documents of this root are deleted in a single transaction, and the source is then indexed again

### Requirement: Multi-source Runner

The Runner executes the pipeline over the configured sources: ingest_all (sequentially, source errors are collected), ingest_source, sync_source/ingest_source_by_path (entry points of incremental sync by file path), prune_deleted (documents with vanished files are deleted cascadingly), cleanup_orphaned_data (orphan entities without entity_sources, except infrastructure types; orphan facts without fact_sources, except approved ones; orphan documents). All mutating operations are serialized. The source type, when absent from the configuration, is determined by a path heuristic (wiki → mediawiki, webpage → webpages, otherwise unstructured); the domains from the source configuration are stamped into the documents' metadata.

#### Scenario: Per-file incremental sync
- **WHEN** sync_source is called for a changed file
- **THEN** the source whose catalog contains the path is found, and incremental ingestion is performed

#### Scenario: Pruning deleted files
- **WHEN** an indexed file vanishes from disk
- **THEN** the document and all its data are deleted cascadingly in a single transaction

### Requirement: Post-processing: linking

After ingestion the Runner builds cross-domain entity relations with the graph layer, if the cross_domain_links configuration is present; the incrementality window is taken from app_kv (the timestamp of the last run) and written back; linking errors land in the final statistics without aborting it.

#### Scenario: First linking run
- **WHEN** the timestamp of the previous run is absent
- **THEN** relations are built over all entities, and the current timestamp is stored

### Requirement: Pipeline parity

The pipeline is verified end-to-end on recorded scenarios: deduplication, rebuild, facts with quotes, prune, orphan cleanup — on an in-memory DB with mock embeddings and regex NER.

#### Scenario: End-to-end run over recorded scenarios
- **WHEN** the e2e scenarios (ingest → repeat ingest → change → delete → prune → cleanup) are run through the Rust pipeline
- **THEN** the DB state and statistics match the fixed results

### Requirement: Embedding and persistence of search_text
The per-document ingestion pipeline SHALL compute each chunk's embedding from `search_text` (not `text`) and SHALL persist both `chunk_text` (the invariant-preserving body) and `search_text` to the `chunks` table. It SHALL also serialize each chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) to the `chunks.metadata_json` column (a chunk with an empty bag stores `NULL`); the document-level `extra` bag is still serialized to `documents.metadata_json` as before. Entity extraction (NER) is unchanged: it SHALL receive the chunk's metadata bag (the same keys as before) and run on `text`.

#### Scenario: Embedding input is search_text
- **WHEN** the pipeline embeds a batch of chunks
- **THEN** the embedding provider receives each chunk's `search_text` (not its `text`)

#### Scenario: Both fields persisted
- **WHEN** the pipeline writes a chunk to the database
- **THEN** both `chunk_text` and `search_text` are stored, and the chunk's own metadata bag is stored in `metadata_json` (a chunk with no chunk-specific metadata stores `NULL`)

#### Scenario: NER input unchanged
- **WHEN** the pipeline runs entity extraction on a chunk
- **THEN** NER receives `text` and the chunk's metadata bag (the same keys it received before this change), not `search_text`

### Requirement: Worker job logging

The document worker logs every job's outcome through structured logging:
info on success (path + op), warn on failure with the attempt count and
backoff, error when the retry cap is reached; failures to record success,
unknown ops, and orphan-cleanup failures are logged at warn/error. After
each cycle that processed one or more jobs, the worker logs a queue-state
summary (counts by status). Runner per-document warnings use the same
structured logging.

#### Scenario: Successful job
- **WHEN** a job's document is processed successfully
- **THEN** an info event carries the document path and the op

#### Scenario: Failed job below the cap
- **WHEN** a job fails and has retry budget left
- **THEN** a warn event carries the path, the attempt number, and the backoff

#### Scenario: Cycle summary
- **WHEN** a worker cycle processes one or more jobs
- **THEN** an info event carries the processed count and the queue counts by status
