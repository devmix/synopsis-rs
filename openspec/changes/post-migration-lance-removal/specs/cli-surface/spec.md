# cli-surface Specification (delta)

## MODIFIED Requirements

### Requirement: Подкоманда db

A subcommand for dataset database maintenance. It SHALL provide two actions:
`db stats` — prints dataset and knowledge-DB statistics (document, chunk, entity,
entity_link, fact and queue-job counts), read-only, no modification and no
confirmation; `db clear` — prints the same statistics, asks for confirmation
(`Confirm deletion? [y/N]` on stdin) and on `y`/`Y` deletes the entire dataset
state directory (`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db`
and the vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`,
ignoring a missing directory). This atomically removes both the SQLite DB and the
vectors in one call. The command is one-shot and does not load the embedding model /
ONNX (it opens only the dataset-bound DB to print statistics, then closes it before
deletion). After `db clear` a `serve` restart is required so the startup reconcile
re-enqueues the files and the background worker re-embeds the documents and
recreates the DB + vector index.

#### Scenario: Stats
- **WHEN** `synopsis db stats` is invoked
- **THEN** counts for documents/chunks/entities/entity_links/facts/queue are printed;
  the DB is not modified

#### Scenario: Clear with confirmation
- **WHEN** `synopsis db clear` is invoked and the answer is `y`
- **THEN** the whole dataset state directory (knowledge.db + vectors/) is removed
  from disk; a cleanup summary is printed

#### Scenario: Clear aborted
- **WHEN** `synopsis db clear` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no deletion happens; the command exits without modifying the DB

#### Scenario: Stats shown before prompt
- **WHEN** `synopsis db clear` is invoked
- **THEN** before the confirmation prompt, counts for documents/chunks/entities/
  entity_links/facts/queue are printed
