//! Entity storage over the `entities` table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/entity_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
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
//!   [`ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
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

use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::{escape_like, normalize};

/// Maximum ids per single `IN (...)` statement (design D9): SQLite bounds
/// bound parameters per statement at 32766; 500 stays far below it.
const ID_BATCH_SIZE: usize = 500;

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
    /// [`ID_BATCH_SIZE`] (design D9). Returns the number of rows deleted.
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
    /// The `IN` list is batched in chunks of [`ID_BATCH_SIZE`] to stay far
    /// below SQLite's 32766 bound on bound parameters (design D9).
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::Db;
    use crate::test_util::{in_memory_db, temp_file_db};

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_entities<T>(db: &Db, f: impl FnOnce(&EntityDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&EntityDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Insert a document row and return its id (needed for `entity_sources`).
    fn insert_document(db: &Db, path: &str) -> i64 {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO documents (source_type, original_path) VALUES ('markdown', ?)",
                [path],
            )
            .unwrap();
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap()
    }

    /// Insert a fact row directly (the Fact DAO lands in task 1.6).
    fn insert_fact(db: &Db, subject: i64, predicate: &str, object: i64) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO facts (subject_entity_id, predicate, object_entity_id) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![subject, predicate, object],
            )
            .unwrap()
        })
        .unwrap();
    }

    /// Insert a fact with a NULL subject (allowed by the v5 schema) — the
    /// regression trigger for the oracle's `NOT IN` bug.
    fn insert_null_subject_fact(db: &Db, predicate: &str, object: i64) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO facts (predicate, object_entity_id) VALUES (?1, ?2)",
                rusqlite::params![predicate, object],
            )
            .unwrap()
        })
        .unwrap();
    }

    /// Link an entity to a document via `entity_sources`.
    fn insert_entity_source(db: &Db, entity_id: i64, document_id: i64) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO entity_sources (entity_id, document_id) VALUES (?1, ?2)",
                rusqlite::params![entity_id, document_id],
            )
            .unwrap()
        })
        .unwrap();
    }

    // (a) create + get_by_id round-trip, all fields.
    #[test]
    fn create_then_get_by_id_round_trip() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create(
                    "PERSON",
                    "Alice Smith",
                    "hr",
                    Some("HR lead"),
                    Some(0.92),
                    Some(r#"{"source":"llm"}"#),
                )
                .unwrap();
            let ent = entities
                .get_by_id(id)
                .unwrap()
                .expect("created entity must exist");
            assert_eq!(ent.id, id);
            assert_eq!(ent.entity_type, "PERSON");
            assert_eq!(ent.name, "Alice Smith");
            assert_eq!(ent.domain, "hr");
            assert_eq!(ent.description.as_deref(), Some("HR lead"));
            assert_eq!(ent.confidence, Some(0.92));
            assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"source":"llm"}"#));
            assert!(!ent.created_at.is_empty(), "created_at default must be set");
            assert_eq!(entities.get_by_id(999_999).unwrap(), None);
        });
    }

    // (b1) get_by_name: full unique key (type, name, domain).
    #[test]
    fn get_by_name_full_key() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            let found = entities
                .get_by_name("PERSON", "Alice", "hr")
                .unwrap()
                .expect("exact key must match");
            assert_eq!(found.id, id);

            // Same name+domain, different type → no match (full key).
            entities
                .create("ORGANIZATION", "Alice", "hr", None, None, None)
                .unwrap();
            assert_eq!(
                entities
                    .get_by_name("ORGANIZATION", "Alice", "policy")
                    .unwrap(),
                None,
                "wrong domain must not match"
            );
            assert_eq!(
                entities.get_by_name("POLICY", "Alice", "hr").unwrap(),
                None,
                "wrong type must not match"
            );
        });
    }

    // (b2) get_by_name_fold: case + whitespace normalization.
    #[test]
    fn get_by_name_fold_normalizes_case_and_whitespace() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create("PERSON", "John Smith", "hr", None, None, None)
                .unwrap();
            for query in ["John Smith", "john smith", "JOHN SMITH", "  john   smith  "] {
                let found = entities
                    .get_by_name_fold(query, "hr")
                    .unwrap()
                    .unwrap_or_else(|| panic!("query {query:?} must match"));
                assert_eq!(found.id, id, "query {query:?}");
            }
            assert_eq!(
                entities.get_by_name_fold("John Smith", "policy").unwrap(),
                None,
                "wrong domain must not match"
            );
            assert_eq!(
                entities.get_by_name_fold("Nobody", "hr").unwrap(),
                None,
                "unknown name must not match"
            );
        });
    }

    // (b3) list_by_name_fold: all domains, ordered by id.
    #[test]
    fn list_by_name_fold_all_domains_ordered_by_id() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id_hr = entities
                .create("PERSON", "Jane Doe", "hr", None, None, None)
                .unwrap();
            let id_policy = entities
                .create("PERSON", "Jane Doe", "policy", None, None, None)
                .unwrap();

            for query in ["Jane Doe", "jane doe", "JANE DOE"] {
                let found = entities.list_by_name_fold(query).unwrap();
                assert_eq!(
                    found.iter().map(|e| e.id).collect::<Vec<_>>(),
                    vec![id_hr, id_policy],
                    "query {query:?}: both domains, id order"
                );
            }
            assert!(
                entities.list_by_name_fold("Nobody").unwrap().is_empty(),
                "unknown name → empty"
            );
        });
    }

    // (a2) update: type/description/metadata; missing id → false.
    #[test]
    fn update() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            assert!(
                entities
                    .update(id, "EMPLOYEE", Some("updated"), Some(r#"{"k":1}"#))
                    .unwrap()
            );
            let ent = entities.get_by_id(id).unwrap().unwrap();
            assert_eq!(ent.entity_type, "EMPLOYEE");
            assert_eq!(ent.description.as_deref(), Some("updated"));
            assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"k":1}"#));
            // Name and domain are NOT changeable via update (oracle contract).
            assert_eq!(ent.name, "Alice");
            assert_eq!(ent.domain, "hr");
            assert!(
                !entities.update(999_999, "X", None, None).unwrap(),
                "missing id must report false"
            );
        });
    }

    // (a3) update_name: rename; missing id → false.
    #[test]
    fn update_name() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create("ORGANIZATION", "Apple", "", None, None, None)
                .unwrap();
            assert!(entities.update_name(id, "Apple Inc.").unwrap());
            assert_eq!(entities.get_by_id(id).unwrap().unwrap().name, "Apple Inc.");
            assert!(
                !entities.update_name(999_999, "Ghost").unwrap(),
                "missing id must report false"
            );
        });
    }

    // (a4) delete; repeat → false; get → None.
    #[test]
    fn delete() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            assert!(entities.delete(id).unwrap());
            assert_eq!(entities.get_by_id(id).unwrap(), None);
            assert!(
                !entities.delete(id).unwrap(),
                "second delete must report false"
            );
        });
    }

    // (g1) count: total and with filters.
    #[test]
    fn count_respects_filters() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Bob", "hr", None, None, None)
                .unwrap();
            entities
                .create("ORGANIZATION", "Acme", "it", None, None, None)
                .unwrap();
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 3);
            assert_eq!(
                entities
                    .count(&EntityFilter {
                        entity_type: Some("PERSON".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                2
            );
            assert_eq!(
                entities
                    .count(&EntityFilter {
                        domain: Some("it".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                1
            );
            assert_eq!(
                entities
                    .count(&EntityFilter {
                        name: Some("acme".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                1
            );
        });
    }

    // (g2) list orders by name.
    #[test]
    fn list_orders_by_name() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Zed", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Bob", "hr", None, None, None)
                .unwrap();
            let names: Vec<String> = entities
                .list()
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            assert_eq!(names, vec!["Alice", "Bob", "Zed"]);
        });
    }

    // (g3) list_by_type with and without the domain filter.
    #[test]
    fn list_by_type() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Bob", "it", None, None, None)
                .unwrap();
            entities
                .create("ORGANIZATION", "Acme", "hr", None, None, None)
                .unwrap();

            let all_persons = entities.list_by_type("PERSON", None).unwrap();
            assert_eq!(all_persons.len(), 2);

            let hr_persons = entities.list_by_type("PERSON", Some("hr")).unwrap();
            assert_eq!(hr_persons.len(), 1);
            assert_eq!(hr_persons[0].name, "Alice");

            assert!(
                entities.list_by_type("POLICY", None).unwrap().is_empty(),
                "unknown type → empty"
            );
        });
    }

    // (d1) list_paginated with all three filters.
    #[test]
    fn list_paginated_filters() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Bob", "hr", None, None, None)
                .unwrap();
            entities
                .create("ORGANIZATION", "Acme Corp", "it", None, None, None)
                .unwrap();

            let (page, total) = entities
                .list_paginated(
                    0,
                    10,
                    &EntityFilter {
                        entity_type: Some("PERSON".into()),
                        domain: Some("hr".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 2);
            assert_eq!(page.len(), 2);

            let (page, total) = entities
                .list_paginated(
                    0,
                    10,
                    &EntityFilter {
                        name: Some("acme".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].name, "Acme Corp");

            let (page, total) = entities
                .list_paginated(0, 10, &EntityFilter::default())
                .unwrap();
            assert_eq!(total, 3);
            assert_eq!(page.len(), 3);
        });
    }

    // (d2) list_paginated: LIKE wildcards in the name filter match literally.
    #[test]
    fn list_paginated_name_filter_escapes_like_wildcards() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Report_2024", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Report 2024", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "100%_off", "hr", None, None, None)
                .unwrap();

            let (page, total) = entities
                .list_paginated(
                    0,
                    10,
                    &EntityFilter {
                        name: Some("Report_2024".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1, "escaped underscore must be literal");
            assert_eq!(page[0].name, "Report_2024");

            let (page, total) = entities
                .list_paginated(
                    0,
                    10,
                    &EntityFilter {
                        name: Some("100%_off".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].name, "100%_off");
        });
    }

    // (d3) list_paginated: page windows over id order.
    #[test]
    fn list_paginated_pages_by_id() {
        let db = in_memory_db();
        let mut ids = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..5 {
                ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
            }
            Ok(())
        })
        .expect("seed transaction commits");

        with_entities(&db, |entities| {
            let (page, total) = entities
                .list_paginated(1, 2, &EntityFilter::default())
                .unwrap();
            assert_eq!(total, 5);
            assert_eq!(page.len(), 2);
            assert_eq!(page[0].id, ids[1]);
            assert_eq!(page[1].id, ids[2]);

            let (page, total) = entities
                .list_paginated(10, 2, &EntityFilter::default())
                .unwrap();
            assert!(page.is_empty());
            assert_eq!(total, 5);
        });
    }

    // (c1) get_or_create: repeated calls with the same key → one row, one id.
    #[test]
    fn get_or_create_repeated_returns_same_id() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let id = entities
                .get_or_create(
                    "PERSON",
                    "Test Entity",
                    "",
                    Some("desc"),
                    Some(0.88),
                    Some(r#"{"key":"value"}"#),
                )
                .unwrap();
            let id2 = entities
                .get_or_create("PERSON", "Test Entity", "", None, Some(0.50), None)
                .unwrap();
            assert_eq!(id, id2, "second call must return the existing id");
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 1);

            // The first call's fields are kept; the second call's are ignored.
            let ent = entities.get_by_id(id).unwrap().unwrap();
            assert_eq!(ent.confidence, Some(0.88));
            assert_eq!(ent.metadata_json.as_deref(), Some(r#"{"key":"value"}"#));
            assert_eq!(ent.description.as_deref(), Some("desc"));
        });
    }

    // (c2) get_or_create: same name+domain, DIFFERENT type → second row
    // (the full unique key is the contract; the oracle would have returned
    // the other type's row — documented deviation).
    #[test]
    fn get_or_create_different_type_creates_second_row() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let person = entities
                .get_or_create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            let org = entities
                .get_or_create("ORGANIZATION", "Alice", "hr", None, None, None)
                .unwrap();
            assert_ne!(person, org, "different types are different entities");
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 2);
        });
    }

    // (c3) get_or_create: concurrent calls with the same key → one row, all
    // callers get the same id (D5 atomicity; SQLite serializes the writes
    // across pool connections, the ON CONFLICT clause is the guarantee).
    //
    // File-backed WAL database (task 1.16): on the shared-cache `:memory:`
    // database the concurrent writers flake with SQLITE_LOCKED_SHAREDCACHE
    // (extended code 262) — shared-cache table locks bypass the busy
    // handler, a known SQLite limitation. The file-backed configuration is
    // the production one, where busy_timeout=5000 (D8) serializes writers.
    #[test]
    fn get_or_create_is_atomic_under_concurrency() {
        let db = temp_file_db();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                db.with_conn(|conn| {
                    let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
                    entities
                        .get_or_create("PERSON", "Raced Entity", "", None, None, None)
                        .unwrap()
                })
                .unwrap()
            }));
        }
        let ids: Vec<i64> = handles
            .into_iter()
            .map(|h| h.join().expect("thread must not panic"))
            .collect();
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "all callers must get the same id, got {ids:?}"
        );
        with_entities(&db, |entities| {
            assert_eq!(
                entities.count(&EntityFilter::default()).unwrap(),
                1,
                "exactly one row must exist"
            );
        });
    }

    // (f1) get_by_ids: several, missing absent, empty input.
    #[test]
    fn get_by_ids() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            let e1 = entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            let e2 = entities
                .create("ORGANIZATION", "Acme", "hr", None, None, None)
                .unwrap();
            let e3 = entities
                .create("LOCATION", "New York", "geo", None, None, None)
                .unwrap();

            let found = entities.get_by_ids(&[e2, e1]).unwrap();
            let got: HashSet<i64> = found.iter().map(|e| e.id).collect();
            assert_eq!(got, [e1, e2].into_iter().collect());
            let alice = found.iter().find(|e| e.id == e1).unwrap();
            assert_eq!(alice.name, "Alice");
            assert_eq!(alice.entity_type, "PERSON");

            let mixed = entities.get_by_ids(&[e1, 999_999, e3]).unwrap();
            assert_eq!(mixed.len(), 2, "missing ids are absent");

            assert!(entities.get_by_ids(&[]).unwrap().is_empty());
        });
    }

    // (f2) get_by_ids across the D9 batch boundary (3 × 500).
    #[test]
    fn get_by_ids_batches_over_500() {
        let db = in_memory_db();
        let mut ids = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..1200 {
                ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
            }
            Ok(())
        })
        .expect("seed transaction commits");

        with_entities(&db, |entities| {
            let requested: Vec<i64> = ids.iter().rev().copied().collect();
            let found = entities.get_by_ids(&requested).unwrap();
            assert_eq!(found.len(), 1200);
            let got: HashSet<i64> = found.iter().map(|e| e.id).collect();
            assert_eq!(got, ids.into_iter().collect::<HashSet<i64>>());
        });
    }

    // (e1) delete_orphaned_entity_ids: orphans deleted; source-linked,
    // fact-referenced and EntityType entities survive.
    #[test]
    fn delete_orphaned_entity_ids() {
        let db = in_memory_db();
        let doc = insert_document(&db, "/docs/hr.md");

        let (orphan, linked, fact_subj, fact_obj, type_id) = with_entities(&db, |entities| {
            (
                entities
                    .create("PERSON", "Charlie", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("PERSON", "Alice", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("PERSON", "Bob", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("ORGANIZATION", "Acme", "it", None, None, None)
                    .unwrap(),
                entities
                    .create("EntityType", "PERSON", "", None, None, None)
                    .unwrap(),
            )
        });

        insert_entity_source(&db, linked, doc);
        insert_fact(&db, fact_subj, "works_at", fact_obj);

        with_entities(&db, |entities| {
            let deleted = entities.delete_orphaned_entity_ids().unwrap();
            assert_eq!(deleted, 1, "only the orphan must be deleted");

            assert_eq!(entities.get_by_id(orphan).unwrap(), None);
            for survivor in [linked, fact_subj, fact_obj, type_id] {
                assert!(
                    entities.get_by_id(survivor).unwrap().is_some(),
                    "entity {survivor} must survive"
                );
            }
        });
    }

    // (e1b) Go-bug regression: a fact with a NULL subject must not disable
    // the whole cleanup (the oracle's `NOT IN` would match nothing).
    #[test]
    fn delete_orphaned_entity_ids_with_null_fact_endpoint() {
        let db = in_memory_db();
        let (orphan, referenced) = with_entities(&db, |entities| {
            (
                entities
                    .create("PERSON", "Charlie", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("ORGANIZATION", "Acme", "it", None, None, None)
                    .unwrap(),
            )
        });
        // A fact with a NULL subject still references its object.
        insert_null_subject_fact(&db, "located_in", referenced);

        with_entities(&db, |entities| {
            let deleted = entities.delete_orphaned_entity_ids().unwrap();
            assert_eq!(deleted, 1, "the orphan must still be deleted");
            assert_eq!(entities.get_by_id(orphan).unwrap(), None);
            assert!(
                entities.get_by_id(referenced).unwrap().is_some(),
                "the fact-referenced entity must survive"
            );
        });
    }

    // (e2) delete_orphaned_by_ids: only the orphan candidates are deleted.
    #[test]
    fn delete_orphaned_by_ids() {
        let db = in_memory_db();
        let doc = insert_document(&db, "/docs/hr.md");

        let (orphan, linked, type_id) = with_entities(&db, |entities| {
            (
                entities
                    .create("PERSON", "Charlie", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("PERSON", "Alice", "hr", None, None, None)
                    .unwrap(),
                entities
                    .create("EntityType", "PERSON", "", None, None, None)
                    .unwrap(),
            )
        });
        insert_entity_source(&db, linked, doc);

        with_entities(&db, |entities| {
            // Empty candidate list → 0, nothing touched.
            assert_eq!(entities.delete_orphaned_by_ids(&[]).unwrap(), 0);

            let deleted = entities
                .delete_orphaned_by_ids(&[orphan, linked, type_id])
                .unwrap();
            assert_eq!(deleted, 1, "only the orphan candidate must be deleted");
            assert_eq!(entities.get_by_id(orphan).unwrap(), None);
            assert!(entities.get_by_id(linked).unwrap().is_some());
            assert!(entities.get_by_id(type_id).unwrap().is_some());
        });
    }

    // (e3) delete_orphaned_by_ids across the D9 batch boundary.
    #[test]
    fn delete_orphaned_by_ids_batches_over_500() {
        let db = in_memory_db();
        let mut ids = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..1200 {
                ids.push(entities.create("PERSON", &format!("E-{i}"), "hr", None, None, None)?);
            }
            Ok(())
        })
        .expect("seed transaction commits");

        with_entities(&db, |entities| {
            let candidates: Vec<i64> = ids.iter().rev().copied().collect();
            let deleted = entities.delete_orphaned_by_ids(&candidates).unwrap();
            assert_eq!(deleted, 1200, "all orphan candidates must be deleted");
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 0);
        });
    }

    // (g4) types_by_count / domains_by_count / unique_types.
    #[test]
    fn counts_and_unique_types() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            entities
                .create("PERSON", "Alice", "hr", None, None, None)
                .unwrap();
            entities
                .create("PERSON", "Bob", "hr", None, None, None)
                .unwrap();
            entities
                .create("ORGANIZATION", "Acme", "it", None, None, None)
                .unwrap();

            let by_type = entities.types_by_count().unwrap();
            assert_eq!(by_type.get("PERSON"), Some(&2));
            assert_eq!(by_type.get("ORGANIZATION"), Some(&1));
            assert_eq!(by_type.len(), 2);

            let by_domain = entities.domains_by_count().unwrap();
            assert_eq!(by_domain.get("hr"), Some(&2));
            assert_eq!(by_domain.get("it"), Some(&1));
            assert_eq!(by_domain.len(), 2);

            assert_eq!(
                entities.unique_types().unwrap(),
                vec!["ORGANIZATION", "PERSON"],
                "distinct types, sorted"
            );
        });
    }

    // (g4b) empty table: all aggregations report empty, not an error.
    #[test]
    fn aggregations_on_empty_table() {
        let db = in_memory_db();
        with_entities(&db, |entities| {
            assert!(entities.types_by_count().unwrap().is_empty());
            assert!(entities.domains_by_count().unwrap().is_empty());
            assert!(entities.unique_types().unwrap().is_empty());
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 0);
        });
    }

    // (h) list_created_since: strictly after, ordered by created_at.
    #[test]
    fn list_created_since_strictly_after() {
        let db = in_memory_db();
        let alice = db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO entities (type, name, domain, created_at) \
                 VALUES ('PERSON', 'Alice', 'hr', '2024-01-01 10:00:00')",
                    [],
                )
                .unwrap();
                conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                    .unwrap()
            })
            .unwrap();
        let bob = db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO entities (type, name, domain, created_at) \
                 VALUES ('PERSON', 'Bob', 'hr', '2024-01-15 12:00:00')",
                    [],
                )
                .unwrap();
                conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                    .unwrap()
            })
            .unwrap();
        let acme = db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO entities (type, name, domain, created_at) \
                 VALUES ('ORGANIZATION', 'Acme', 'it', '2024-02-01 08:00:00')",
                    [],
                )
                .unwrap();
                conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                    .unwrap()
            })
            .unwrap();

        with_entities(&db, |entities| {
            for (since, want_ids) in [
                ("2023-12-31 23:59:59", &[alice, bob, acme][..]),
                ("2024-01-10 00:00:00", &[bob, acme][..]),
                ("2024-03-01 00:00:00", &[][..]),
            ] {
                let found: Vec<i64> = entities
                    .list_created_since(since)
                    .unwrap()
                    .into_iter()
                    .map(|e| e.id)
                    .collect();
                assert_eq!(found, want_ids, "since {since}");
            }
            // Strictly greater: a timestamp equal to `since` is excluded.
            let found: Vec<i64> = entities
                .list_created_since("2024-01-15 12:00:00")
                .unwrap()
                .into_iter()
                .map(|e| e.id)
                .collect();
            assert_eq!(found, vec![acme], "equal timestamp must be excluded");
        });
    }

    // The DAO works over a transaction: commit and rollback paths.
    #[test]
    fn create_inside_transaction() {
        let db = in_memory_db();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            entities.create("PERSON", "/tx.md", "hr", None, None, None)?;
            Ok(())
        })
        .expect("commit");

        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
                entities.create("PERSON", "tx-rollback", "hr", None, None, None)?;
                // A genuine failure: UNIQUE(type, name, domain).
                entities.create("PERSON", "tx-rollback", "hr", None, None, None)?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        with_entities(&db, |entities| {
            assert_eq!(entities.count(&EntityFilter::default()).unwrap(), 1);
            assert_eq!(
                entities.get_by_name("PERSON", "tx-rollback", "hr").unwrap(),
                None
            );
        });
    }
}
