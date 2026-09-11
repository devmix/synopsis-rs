# cli-surface Specification

## MODIFIED Requirements

### Requirement: db subcommand

A subcommand for dataset database maintenance. It SHALL provide three
actions: `db stats` — prints dataset and knowledge-DB statistics (document,
chunk, entity, entity_link, fact and queue-job counts) as an aligned
key-value block rendered by the unified console layer, read-only, no
modification and no confirmation; `db clear` — prints the same statistics,
asks for confirmation (`Confirm deletion? [y/N]` on stdin) and on `y`/`Y`
deletes the entire dataset state directory
(`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db` and the
vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`,
ignoring a missing directory). This atomically removes both the SQLite DB
and the vectors in one call. `db merge-entities <id> --into <id>` — merges
one entity into another: verifies that both ids exist and share the same
`type` and `domain` (otherwise a clear error, no modification), asks for
confirmation (`Confirm merge? [y/N]` on stdin), and on `y`/`Y` performs the
transactional merge (see the data-schema spec: facts, chunk links, sources,
and links re-pointed; both names recorded as aliases of the surviving
entity; the duplicate row deleted) and prints a summary of re-pointed row
counts and the surviving name; any non-`y` input aborts without changes.
Both `db clear` and `db merge-entities` are one-shot and do not load the
embedding model / ONNX (they open only the dataset-bound DB, then close it
before any deletion or write). After `db clear` a `serve` restart is required
so the startup reconcile re-enqueues the files and the background worker
re-embeds the documents and recreates the DB + vector index.

> **Contract decision (2026-09-09):** the `db stats` / `db clear` statistics
> block changed from fixed-width dash-separated lines to an aligned key-value
> block rendered by the unified console layer. The set of printed counts and
> the confirmation prompt are unchanged.

#### Scenario: Stats
- **WHEN** `synopsis db stats` is invoked
- **THEN** counts for documents/chunks/entities/entity_links/facts/queue are printed; the DB is not modified

#### Scenario: Clear with confirmation
- **WHEN** `synopsis db clear` is invoked and the answer is `y`
- **THEN** the whole dataset state directory (knowledge.db + vectors/) is removed from disk; a cleanup summary is printed

#### Scenario: Clear aborted
- **WHEN** `synopsis db clear` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no deletion happens; the command exits without modifying the DB

#### Scenario: Stats shown before prompt
- **WHEN** `synopsis db clear` is invoked
- **THEN** before the confirmation prompt, counts for documents/chunks/entities/entity_links/facts/queue are printed

#### Scenario: Merge with confirmation
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked for two existing entities of the same type and domain and the answer is `y`
- **THEN** the transactional merge is applied, a summary (re-pointed fact/chunk/source/link counts, the surviving name, the recorded aliases) is printed, and the duplicate row no longer exists

#### Scenario: Merge precondition failure
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked with a nonexistent id, or for entities whose types or domains differ
- **THEN** a clear error is printed and the DB is not modified

#### Scenario: Merge aborted
- **WHEN** `synopsis db merge-entities <id> --into <id>` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no merge happens; the command exits without modifying the DB
