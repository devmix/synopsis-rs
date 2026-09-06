# pipeline Specification

## MODIFIED Requirements

### Requirement: Post-processing: linking

When the `cross-domain-links` configuration is present, the Runner SHALL build cross-domain entity relations incrementally through the event queue. During document indexing the pipeline determines which entities were CREATED or UPDATED for that document; when that set is non-empty the runner SHALL enqueue one `entity:link` event per document (identity = document id, payload = exactly those entity ids) — no event is enqueued when the set is empty. Re-indexing a document enqueues only the newly created/updated entities, never the document's full entity set. A re-enqueued `entity:link` event for the same document merges candidate ids (union) and moves to the end of the queue.

The background worker processes `entity:link` events sequentially: the candidate entities are linked against the remaining cross-domain entities with the configured methods (equals/expression/llm; idempotent inserts; LLM decision cache). Per-pair linking failures are logged and skipped and never abort the task; a task-level failure is retried with backoff and lands in `error` status, inspectable and resettable through `queue status` / `queue reset-retries`. When a document is deleted, its `entity:link` queue task is removed with it.

#### Scenario: First linking run
- **WHEN** a dataset has no entity links yet and a document is indexed that creates entities
- **THEN** an `entity:link` task is enqueued for those entities; cross-domain relations appear as later documents from other domains are indexed and their new entities are linked against them (there is no separate "build everything" bootstrap — every run is incremental over the delta)

#### Scenario: Indexing a document with new entities
- **WHEN** a document is indexed and the pipeline creates or updates N > 0 entities
- **THEN** an `entity:link` task carrying exactly those N entity ids is enqueued, and after the worker processes it, cross-domain links exist for the candidates where a method matched

#### Scenario: Re-indexing with no entity changes
- **WHEN** a document is re-indexed and no entity is created or updated
- **THEN** no `entity:link` task is enqueued

#### Scenario: Partial update links only the delta
- **WHEN** a document with 1000 linked entities is re-indexed and only 10 are created or updated
- **THEN** the enqueued task carries only those 10 ids; the other 990 are not reprocessed

#### Scenario: Linking failure
- **WHEN** processing an `entity:link` task fails
- **THEN** the failure is recorded in the task (`last_error`, backoff); per-pair errors are logged and skipped; after `max_attempts` the task is in `error` status and can be reset with `queue reset-retries`

#### Scenario: Document deletion
- **WHEN** a document is deleted
- **THEN** its `entity:link` queue task (if any) is removed from the queue
