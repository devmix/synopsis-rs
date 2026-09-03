//! Entity storage over the `entities` table.
//!
//! **Go bug fixes (conscious deviations):**
//! - `get_or_create` is atomic: `INSERT ... ON CONFLICT (type, name, domain)
//!   DO NOTHING` + `RETURNING id` (design D5). The oracle did
//!   select-then-insert — a TOCTOU race under concurrent access.
//! - `get_by_name` matches the FULL unique key (type, name, domain); the
//!   oracle matched name+domain only, which is ambiguous when two types share
//!   a name in one domain (and the oracle's `QueryRow` would then fail with
//!   "multiple rows").
//! - `get_or_create` therefore also matches the full triple: the oracle
//!   returned an existing entity of a DIFFERENT type for the same name+domain;
//!   the Rust version creates the requested type (the unique key is the
//!   contract).
//! - `delete_orphaned_entity_ids`/`delete_orphaned_by_ids` use `NOT EXISTS`
//!   instead of the oracle's `id NOT IN (SELECT subject_entity_id FROM facts
//!   UNION ...)`: both fact FK columns are nullable, and `NOT IN` against a
//!   list containing NULL matches NOTHING under SQL three-valued logic — one
//!   fact with a NULL endpoint would silently disable the whole cleanup (Go
//!   bug; regression test below).
//! - `delete_orphaned_by_ids`/`get_by_ids` batch the `IN` list in chunks of
//!   [`config::ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
//!   placeholder list (potential 32766 bound violation).
//!
//! **Other deviations (house conventions, as in `document.rs`/`chunk.rs`):**
//! - `update`/`update_name`/`delete` return `bool` (`false` = no such id)
//!   instead of a "not found" error;
//! - `get_by_name_fold`/`list_by_name_fold` normalize the input with
//!   [`crate::utils::normalize`] (trim + collapse + lowercase) and compare
//!   with `lower(trim(name))` — case- AND surrounding-whitespace-insensitive
//!   (oracle: SQL `lower()` only);
//! - `list_paginated` unifies the oracle's `ListPaginated` and
//!   `ListPaginatedWithName` into one [`EntityFilter`] (DRY); the name filter
//!   is `LIKE` with `\`/`%`/`_` escaped; `count` takes the same filter
//!   (oracle `Count()` with no arguments = empty filter);
//! - `get_by_ids` returns a `Vec<Entity>` (Rust idiom; oracle: a map).
//!
//! Note: the task body's `Entity` field list (…`updated_at`) does not match
//! the frozen v5 schema, which has NO `updated_at` on `entities` and DOES
//! have `confidence`; the schema is the contract (same resolution as
//! `chunk.rs`).

use std::collections::HashMap;

use config::ID_BATCH_SIZE;
use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::{escape_like, normalize};

/// Shared `SELECT` list for the `entities` row queries (column order is the
/// contract of [`row_to_entity`]).
const SELECT_ENTITY: &str = "SELECT id, type, name, domain, description, confidence, metadata_json, created_at \
     FROM entities";

/// Shared `WHERE` for filtered listing and counting. Fixed shape: each
/// optional filter compares against a possibly-`NULL` parameter, so the SQL
/// is never assembled from user input and the same clause serves
/// [`EntityDao::list_paginated`] and [`EntityDao::count`] (DRY).
const FILTER_WHERE: &str = "WHERE (?1 IS NULL OR type = ?1) \
     AND (?2 IS NULL OR domain = ?2) \
     AND (?3 IS NULL OR lower(name) LIKE lower(?3) ESCAPE '\\')";

/// A knowledge-graph entity (one row of the v5 `entities` table).
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// Row id (autoincrement).
    pub id: i64,
    /// Entity type (e.g. `'PERSON'`, `'ORGANIZATION'`, `'EntityType'`).
    pub entity_type: String,
    /// Canonical name.
    pub name: String,
    /// Domain this entity belongs to (`''` = global).
    pub domain: String,
    /// Free-text description, if any.
    pub description: Option<String>,
    /// Extraction confidence, if any.
    pub confidence: Option<f64>,
    /// Metadata as a JSON string, if any.
    pub metadata_json: Option<String>,
    /// Creation timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub created_at: String,
}

/// Optional filters for [`EntityDao::list_paginated`] and
/// [`EntityDao::count`]; a `None` (or empty) member is not applied — the
/// same "empty string = no filter" semantics as the oracle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityFilter {
    /// Match entities of exactly this type.
    pub entity_type: Option<String>,
    /// Match entities in exactly this domain.
    pub domain: Option<String>,
    /// Case-insensitive substring match on the name (`LIKE`, with
    /// `%`/`_`/`\` escaped so user input is matched literally).
    pub name: Option<String>,
}

impl EntityFilter {
    /// The bound-parameter triple for [`FILTER_WHERE`]: the name filter is
    /// escaped and wrapped as a `LIKE` substring pattern.
    fn args(&self) -> (Option<String>, Option<String>, Option<String>) {
        let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
        (
            non_empty(self.entity_type.clone()),
            non_empty(self.domain.clone()),
            self.name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!("%{}%", escape_like(n))),
        )
    }
}

/// CRUD + atomic GetOrCreate + orphan cleanup + pagination over the
/// `entities` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewEntityDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, EntityDao, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
///     let id = entities.get_or_create("PERSON", "Alice", "hr", None, Some(0.9), None)?;
///     assert_eq!(entities.get_by_id(id)?.map(|e| e.id), Some(id));
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct EntityDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> EntityDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert a new entity and return its generated id. `description`,
    /// `confidence` and `metadata_json` are stored as `NULL` when `None`.
    pub fn create(
        &self,
        entity_type: &str,
        name: &str,
        domain: &str,
        description: Option<&str>,
        confidence: Option<f64>,
        metadata_json: Option<&str>,
    ) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO entities (type, name, domain, description, confidence, metadata_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
            params![
                entity_type,
                name,
                domain,
                description,
                confidence,
                metadata_json
            ],
            |row| row.get(0),
        )
    }

    /// Retrieve an entity by id, or `None` if absent.
    pub fn get_by_id(&self, id: i64) -> Result<Option<Entity>, DbError> {
        let rows = self.exec.query(
            &format!("{SELECT_ENTITY} WHERE id = ?"),
            [id],
            row_to_entity,
        )?;
        Ok(rows.into_iter().next())
    }

    /// Retrieve an entity by its FULL unique key (type, name, domain), or
    /// `None` if absent.
    pub fn get_by_name(
        &self,
        entity_type: &str,
        name: &str,
        domain: &str,
    ) -> Result<Option<Entity>, DbError> {
        let rows = self.exec.query(
            &format!("{SELECT_ENTITY} WHERE type = ?1 AND name = ?2 AND domain = ?3"),
            params![entity_type, name, domain],
            row_to_entity,
        )?;
        Ok(rows.into_iter().next())
    }

    /// Retrieve an entity by name and domain, case- and
    /// surrounding-whitespace-insensitively (input normalized with
    /// [`crate::utils::normalize`]). If several types share the same
    /// normalized name+domain, the smallest id wins (the oracle's
    /// `QueryRow` would have failed on multiple rows).
    pub fn get_by_name_fold(&self, name: &str, domain: &str) -> Result<Option<Entity>, DbError> {
        let rows = self.exec.query(
            &format!(
                "{SELECT_ENTITY} WHERE lower(trim(name)) = ?1 AND lower(trim(domain)) = ?2 ORDER BY id"
            ),
            params![normalize(name), normalize(domain)],
            row_to_entity,
        )?;
        Ok(rows.into_iter().next())
    }

    /// All entities whose name matches case- and surrounding-whitespace-
    /// insensitively, in any domain, ordered by id for determinism.
    pub fn list_by_name_fold(&self, name: &str) -> Result<Vec<Entity>, DbError> {
        self.exec.query(
            &format!("{SELECT_ENTITY} WHERE lower(trim(name)) = ? ORDER BY id"),
            [normalize(name)],
            row_to_entity,
        )
    }

    /// All entities, ordered by name.
    pub fn list(&self) -> Result<Vec<Entity>, DbError> {
        self.exec
            .query(&format!("{SELECT_ENTITY} ORDER BY name"), [], row_to_entity)
    }

    /// All entities of `entity_type`, optionally restricted to `domain`
    /// (`None` = all domains), ordered by name.
    pub fn list_by_type(
        &self,
        entity_type: &str,
        domain: Option<&str>,
    ) -> Result<Vec<Entity>, DbError> {
        self.exec.query(
            &format!(
                "{SELECT_ENTITY} WHERE type = ?1 AND (?2 IS NULL OR domain = ?2) ORDER BY name"
            ),
            params![entity_type, domain],
            row_to_entity,
        )
    }

    /// Update type, description and metadata of an existing entity. Name and
    /// domain are NOT changeable via this method (the oracle's contract; a
    /// rename goes through [`Self::update_name`]). Returns `true` if a row
    /// was updated, `false` if no entity has `id`.
    pub fn update(
        &self,
        id: i64,
        entity_type: &str,
        description: Option<&str>,
        metadata_json: Option<&str>,
    ) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE entities SET type = ?1, description = ?2, metadata_json = ?3 WHERE id = ?4",
            params![entity_type, description, metadata_json, id],
        )?;
        Ok(changed > 0)
    }

    /// Rename an existing entity (entity resolution: a longer canonical name
    /// was discovered). Returns `true` if a row was updated, `false` if no
    /// entity has `id`.
    pub fn update_name(&self, id: i64, name: &str) -> Result<bool, DbError> {
        let changed = self.exec.execute(
            "UPDATE entities SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(changed > 0)
    }

    /// Delete an entity (chunk_entities and entity_links cascade per the
    /// schema FKs). Returns `true` if a row was deleted, `false` if no
    /// entity has `id`.
    pub fn delete(&self, id: i64) -> Result<bool, DbError> {
        let changed = self
            .exec
            .execute("DELETE FROM entities WHERE id = ?", [id])?;
        Ok(changed > 0)
    }

    /// Number of entities matching `filter` (same semantics as
    /// [`Self::list_paginated`]; empty filter = total count, the oracle's
    /// `Count()`).
    pub fn count(&self, filter: &EntityFilter) -> Result<i64, DbError> {
        let (entity_type, domain, name) = filter.args();
        self.exec.query_row(
            &format!("SELECT COUNT(*) FROM entities {FILTER_WHERE}"),
            params![entity_type, domain, name],
            |row| row.get(0),
        )
    }

    /// One page of entities matching `filter`, ordered by id; returns the
    /// page and the total number of matching entities.
    pub fn list_paginated(
        &self,
        offset: i64,
        limit: i64,
        filter: &EntityFilter,
    ) -> Result<(Vec<Entity>, i64), DbError> {
        let (entity_type, domain, name) = filter.args();
        let entities = self.exec.query(
            &format!("{SELECT_ENTITY} {FILTER_WHERE} ORDER BY id LIMIT ?4 OFFSET ?5"),
            params![entity_type, domain, name, limit, offset],
            row_to_entity,
        )?;
        let total = self.exec.query_row(
            &format!("SELECT COUNT(*) FROM entities {FILTER_WHERE}"),
            params![entity_type, domain, name],
            |row| row.get(0),
        )?;
        Ok((entities, total))
    }

    /// Atomically fetch or create the entity with the unique key
    /// (type, name, domain) and return its id (design D5):
    /// `INSERT ... ON CONFLICT (type, name, domain) DO NOTHING RETURNING id`
    /// — no select-then-insert TOCTOU window (the oracle's race). The
    /// `description`/`confidence`/`metadata_json` arguments apply only when
    /// the row is actually inserted; on conflict the existing row is left
    /// untouched and its id is returned.
    ///
    /// Parameter order matches [`Self::create`] (entity_type, name, domain,
    /// description, confidence, metadata_json); the oracle's
    /// `GetOrCreate(name, type, domain, ...)` order was not carried over.
    pub fn get_or_create(
        &self,
        entity_type: &str,
        name: &str,
        domain: &str,
        description: Option<&str>,
        confidence: Option<f64>,
        metadata_json: Option<&str>,
    ) -> Result<i64, DbError> {
        let inserted = self.exec.query(
            "INSERT INTO entities (type, name, domain, description, confidence, metadata_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (type, name, domain) DO NOTHING RETURNING id",
            params![
                entity_type,
                name,
                domain,
                description,
                confidence,
                metadata_json
            ],
            |row| row.get(0),
        )?;
        if let Some(id) = inserted.into_iter().next() {
            return Ok(id);
        }
        // Conflict: the entity already exists.
        self.exec.query_row(
            "SELECT id FROM entities WHERE type = ?1 AND name = ?2 AND domain = ?3",
            params![entity_type, name, domain],
            |row| row.get(0),
        )
    }

    /// Delete all orphaned entities: no `entity_sources` link, type is not
    /// `'EntityType'`, and not referenced by any fact (the facts→entities FK
    /// has no CASCADE). Entity links cascade via the schema FK. Returns the
    /// number of rows deleted.
    pub fn delete_orphaned_entity_ids(&self) -> Result<i64, DbError> {
        let changed = self.exec.execute(
            "DELETE FROM entities WHERE id IN (
                 SELECT e.id FROM entities e
                 LEFT JOIN entity_sources es ON e.id = es.entity_id
                 WHERE es.id IS NULL
                   AND e.type != 'EntityType'
                   AND NOT EXISTS (SELECT 1 FROM facts
                                   WHERE facts.subject_entity_id = e.id
                                      OR facts.object_entity_id = e.id)
             )",
            [],
        )?;
        Ok(changed as i64)
    }

    /// Delete the subset of `ids` that are orphaned (same rules as
    /// [`Self::delete_orphaned_entity_ids`]); non-orphan candidates are left
    /// untouched. Empty `ids` → `0`. The `IN` list is batched in chunks of
    /// [`config::ID_BATCH_SIZE`] (design D9). Returns the number of rows
    /// deleted.
    pub fn delete_orphaned_by_ids(&self, ids: &[i64]) -> Result<i64, DbError> {
        let mut deleted = 0;
        for batch in ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!(
                "DELETE FROM entities WHERE id IN ({placeholders}) \
                 AND type != 'EntityType' \
                 AND NOT EXISTS (SELECT 1 FROM entity_sources WHERE entity_id = entities.id) \
                 AND NOT EXISTS (SELECT 1 FROM facts \
                                 WHERE facts.subject_entity_id = entities.id \
                                    OR facts.object_entity_id = entities.id)"
            );
            deleted += self
                .exec
                .execute(&sql, params_from_iter(batch.iter().copied()))?;
        }
        Ok(deleted as i64)
    }

    /// Retrieve several entities by id; ids that do not exist are simply
    /// absent from the result. Empty `ids` yields an empty vec.
    ///
    /// The `IN` list is batched in chunks of [`config::ID_BATCH_SIZE`] to
    /// stay far below SQLite's 32766 bound on bound parameters (design D9).
    pub fn get_by_ids(&self, ids: &[i64]) -> Result<Vec<Entity>, DbError> {
        let mut entities = Vec::new();
        for batch in ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!("{SELECT_ENTITY} WHERE id IN ({placeholders})");
            entities.extend(self.exec.query(
                &sql,
                params_from_iter(batch.iter().copied()),
                row_to_entity,
            )?);
        }
        Ok(entities)
    }

    /// Number of entities per type.
    pub fn types_by_count(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = self.exec.query(
            "SELECT type, COUNT(*) FROM entities GROUP BY type",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Number of entities per domain.
    pub fn domains_by_count(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = self.exec.query(
            "SELECT domain, COUNT(*) FROM entities GROUP BY domain",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Distinct entity types, sorted for a deterministic API.
    pub fn unique_types(&self) -> Result<Vec<String>, DbError> {
        self.exec.query(
            "SELECT DISTINCT type FROM entities ORDER BY type",
            [],
            |row| row.get(0),
        )
    }

    /// All entities with `created_at` strictly after `since` (an
    /// ISO-8601/`CURRENT_TIMESTAMP` text), ordered by `created_at`.
    pub fn list_created_since(&self, since: &str) -> Result<Vec<Entity>, DbError> {
        self.exec.query(
            &format!("{SELECT_ENTITY} WHERE created_at > ? ORDER BY created_at"),
            [since],
            row_to_entity,
        )
    }
}

/// Map an `entities` row (in [`SELECT_ENTITY`] order) to an [`Entity`].
fn row_to_entity(row: &Row<'_>) -> rusqlite::Result<Entity> {
    Ok(Entity {
        id: row.get(0)?,
        entity_type: row.get(1)?,
        name: row.get(2)?,
        domain: row.get(3)?,
        description: row.get(4)?,
        confidence: row.get(5)?,
        metadata_json: row.get(6)?,
        created_at: row.get(7)?,
    })
}
