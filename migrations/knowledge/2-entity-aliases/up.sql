-- Migration 2-entity-aliases: the `entity_aliases` table (change
-- multilingual-entity-resolution, design D2).
--
-- Alias memory: every surface name that has resolved to an entity (merged or
-- aliased) is recorded here, so repeated surface forms resolve by lookup
-- instead of similarity work (design D3). `1-init` is shipped and never
-- edited; this is the second numbered migration, forward-only.
--
-- `PRAGMA user_version` advances 1 -> 2 via rusqlite_migration::to_latest
-- (the migration count is the counter; D3 — the sole schema-state authority,
-- no _schema_migrations table).
--
-- Constraints (data-schema spec, "entity_aliases table and transactional
-- merge"): entity_id NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
-- alias TEXT NOT NULL, UNIQUE (entity_id, alias), and a UNIQUE index on
-- alias alone — an alias names exactly one entity globally.

CREATE TABLE entity_aliases (
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    alias     TEXT NOT NULL,
    UNIQUE (entity_id, alias)
);
CREATE UNIQUE INDEX idx_entity_aliases_alias ON entity_aliases(alias);
