# pipeline Specification

## MODIFIED Requirements

### Requirement: Vectors outside the transaction

Vectors are written to the ANN index after the SQLite transaction commits (the chunk in the DB is the source of truth); the mismatch is reconciled in **both** directions. Orphan vectors (index ids whose chunk no longer exists) are deleted by the orphan cleanup. Missing vectors (chunk rows whose vector is absent — the loss window after an unclean shutdown such as `SIGKILL`, which bypasses the graceful shutdown save) are restored by a targeted re-embed: at serve startup the runner detects chunk rows without a corresponding vector and enqueues one `doc:index` per affected document carrying the `ReEmbed` op, so the worker re-embeds the document's existing chunk rows (no re-parse, re-chunk, or NER) before the index serves traffic; when every chunk row has a vector the self-heal is a no-op.

#### Scenario: Orphan-vector reconciliation
- **WHEN** data cleanup is performed
- **THEN** vectors whose chunks do not exist in the DB are removed from the index

#### Scenario: Missing-vector self-heal at startup
- **WHEN** the server starts and some chunk rows have no corresponding vector (the chunk rows are intact)
- **THEN** a `doc:index` task carrying the `ReEmbed` op is enqueued for each affected document, and after the startup drain the affected chunks are re-embedded and have vectors again (without re-parsing, re-chunking, or re-running NER)

#### Scenario: Self-heal is a no-op when complete
- **WHEN** the server starts and every chunk row has a corresponding vector
- **THEN** no `doc:index` task is enqueued by the self-heal

## ADDED Requirements

### Requirement: Per-cycle vector persistence

After a worker cycle that indexed or deleted one or more documents, the vector engine's RAM layer SHALL be persisted to disk (`build_index`), bounding the unclean-shutdown (`SIGKILL`) loss window to the in-progress batch. A cycle that processed no document work performs no persistence (the empty-RAM `build_index` guard is a no-op). A persistence failure is logged and does not abort the cycle.

#### Scenario: Save after a work cycle
- **WHEN** a worker cycle processes one or more `doc:index` / `doc:delete` tasks
- **THEN** the RAM layer is persisted, and a fresh open of the same directory restores those vectors

#### Scenario: No persistence on an idle cycle
- **WHEN** a worker cycle processes no tasks
- **THEN** no persistence is performed

#### Scenario: Persistence failure is non-fatal
- **WHEN** the post-cycle persistence fails
- **THEN** the failure is logged and the cycle completes normally
