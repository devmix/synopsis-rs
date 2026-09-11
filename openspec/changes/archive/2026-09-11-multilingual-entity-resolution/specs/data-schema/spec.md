# data-schema Specification

## ADDED Requirements

### Requirement: entity_aliases table and transactional merge

The knowledge DB SHALL contain an `entity_aliases` table added by a new
numbered migration (shipped migrations are never edited; `PRAGMA
user_version` remains the sole schema-state authority): columns `entity_id`
INTEGER NOT NULL referencing `entities(id)` with ON DELETE CASCADE and
`alias` TEXT NOT NULL, with a UNIQUE constraint on `(entity_id, alias)` and a
UNIQUE index on `alias` alone (an alias names exactly one entity
globally). The table stores every surface name that has resolved to an
entity (merged or aliased), so repeated surface forms resolve by lookup.

A transactional merge operation `merge_entities(into, from)` SHALL exist in
the db crate. Preconditions (violated → error, no partial change): both
entities exist; they have the same `type` and the same `domain`. Inside one
transaction the operation SHALL: re-point `facts` (both subject and object
positions) from `from` to `into`, dropping fact rows that would collide with
the existing UNIQUE (subject, object, predicate); re-point `chunk_entities`;
move `entity_sources` rows (ignoring (entity_id, document_id) collisions);
re-point `entity_links` (subject and target positions), dropping rows that
would become self-links and ignoring duplicates; record `from.name` and
`into.name` as aliases of `into` (ignoring collisions); delete the `from`
row.

#### Scenario: Fresh-DB schema
- **WHEN** a fresh knowledge DB is built from migrations
- **THEN** the `entity_aliases` table is present with the UNIQUE (entity_id, alias) constraint and the UNIQUE alias index

#### Scenario: Merge re-points dependent rows
- **WHEN** `merge_entities(into, from)` is called for two entities of the same type and domain
- **THEN** after the transaction, no row in `facts`, `chunk_entities`, `entity_sources`, or `entity_links` references the deleted entity; both names are aliases of the surviving entity; the surviving entity's id, name, and metadata are unchanged

#### Scenario: Merge precondition violation
- **WHEN** `merge_entities` is called with ids whose types or domains differ, or where one id does not exist
- **THEN** the operation returns an error and the database is unmodified (no alias rows, no re-pointed rows, no deleted entity)

#### Scenario: Alias is globally unique
- **WHEN** an alias string is already recorded for a different entity
- **THEN** it cannot be recorded for another entity (the UNIQUE alias index rejects it)
