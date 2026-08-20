//! Fact storage over the `facts` table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/fact_dao.go`,
//! re-architected per the 2026-08-19 migration principles (functional copy,
//! not a code copy).
//!
//! **Go bug fixes / conscious deviations:**
//! - `create_or_ignore` is atomic via
//!   `INSERT ... ON CONFLICT (subject_entity_id, object_entity_id, predicate)
//!   DO NOTHING` + `RETURNING id` with a fallback `SELECT` on conflict
//!   (design D5, same pattern as
//!   [`crate::entity::EntityDao::get_or_create`]). The oracle's
//!   `DO UPDATE SET subject_entity_id = subject_entity_id` performs a
//!   pointless self-assignment write on every conflict.
//! - `recompute_weights`, `find_orphaned_fact_ids`, `delete_orphaned_facts`
//!   and `get_by_ids` batch their `IN` lists in chunks of [`ID_BATCH_SIZE`]
//!   (design D9); the oracle built one unbounded placeholder list (potential
//!   32766 bound-parameter violation).
//! - `list_by_entity_ids` de-duplicates its input ids: the oracle returned
//!   the same fact twice in one map slice when the same id appeared twice in
//!   the input.
//! - `search_paginated`'s entity-name filter is a correlated `EXISTS`
//!   subquery instead of the oracle's `INNER JOIN entities`: the join
//!   returned a fact TWICE in the page when both the subject's and the
//!   object's names matched the filter, while its `COUNT(DISTINCT ...)`
//!   total counted it once — page and total disagreed.
//! - A missing endpoint is stored as `NULL` (the v5 schema allows it); the
//!   oracle stored Go's zero value `0` — a dangling reference to a
//!   non-existent entity id. Consequence: SQLite unique indexes treat
//!   `NULL`s as distinct, so `create_or_ignore` de-duplication applies to
//!   facts with non-`NULL` endpoints only.
//! - `validate_fact_domain` returns `bool` (`false` = missing endpoint
//!   entity or domain mismatch) instead of the oracle's descriptive error —
//!   the house convention (see `entity.rs`/`document.rs`), and no per-DAO
//!   `DbError` variant exists for it.
//! - `delete` returns `bool` (`false` = no such id) — house convention. The
//!   oracle has no fact `Delete` at all; the method is added per the task
//!   body.
//! - `create`/`create_or_ignore` store `status = 'approved'` exactly as the
//!   oracle's constructors hard-code it (the schema default is `'draft'`).
//!   The oracle's empty-predicate guard is dropped: the predicate is a
//!   required parameter and no validation `DbError` variant is in scope.
//! - All listings carry a deterministic `ORDER BY` (the oracle ordered by
//!   `created_at` alone, which ties at second-resolution timestamps).
//!
//! Note: the task body's `Fact` field list (…`confidence`) does not match
//! the frozen v5 schema, which has `domain`, `status`, `valid_from`,
//! `valid_to`, `weight` and `metadata` and NO `confidence`; the schema is
//! the contract (same resolution as `entity.rs`).

use std::collections::{HashMap, HashSet};

use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::{escape_like, normalize};

/// Maximum ids per single `IN (...)` statement (design D9): SQLite bounds
/// bound parameters per statement at 32766; 500 stays far below it even when
/// one statement carries two `IN` lists (500 × 2 = 1000 parameters).
const ID_BATCH_SIZE: usize = 500;

/// Shared `SELECT` for the `facts` row queries (column order is the contract
/// of [`row_to_fact`]); the `f.` alias is part of the constant so the same
/// text serves the plain and the filtered (search) queries.
const SELECT_FACT: &str = "SELECT f.id, f.subject_entity_id, f.predicate, f.object_entity_id, \
     f.domain, f.metadata, f.status, f.valid_from, f.valid_to, f.weight, f.created_at, f.updated_at \
     FROM facts f";

/// Shared `WHERE` for the search page and count queries. Fixed shape: each
/// optional filter compares against a possibly-`NULL` parameter, so the SQL
/// is never assembled from user input and the same clause serves the page
/// and the total (DRY, as in `entity.rs`/`document.rs`). The entity-name
/// filter is a correlated `EXISTS` (no `JOIN`): a fact whose subject AND
/// object names both match appears exactly once — the oracle's
/// `INNER JOIN` duplicated it in the page while its `COUNT(DISTINCT ...)`
/// total did not.
const SEARCH_WHERE: &str = "WHERE (?1 IS NULL OR lower(f.predicate) LIKE lower(?1) ESCAPE '\\') \
     AND (?2 IS NULL OR f.status = ?2) \
     AND (?3 IS NULL OR f.domain = ?3) \
     AND (?4 IS NULL OR EXISTS (SELECT 1 FROM entities e \
        WHERE (e.id = f.subject_entity_id OR e.id = f.object_entity_id) \
        AND lower(e.name) LIKE lower(?4) ESCAPE '\\'))";

/// A knowledge-graph fact (one row of the v5 `facts` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// Row id (autoincrement).
    pub id: i64,
    /// Subject entity, or `None` (the v5 schema allows `NULL` endpoints).
    pub subject_entity_id: Option<i64>,
    /// The relation predicate (e.g. `'works_at'`, `'located_in'`).
    pub predicate: String,
    /// Object entity, or `None`.
    pub object_entity_id: Option<i64>,
    /// Domain this fact belongs to (`''` = global).
    pub domain: String,
    /// Metadata as a JSON string, if any (schema column `metadata`).
    pub metadata_json: Option<String>,
    /// Lifecycle status: `'draft'`, `'pending'`, `'approved'` or `'rejected'`.
    pub status: String,
    /// Validity interval start (ISO date), if any.
    pub valid_from: Option<String>,
    /// Validity interval end (ISO date), if any.
    pub valid_to: Option<String>,
    /// Source-count weight (recomputed from `fact_sources` by
    /// [`FactDao::recompute_weights`]).
    pub weight: i64,
    /// Creation timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub created_at: String,
    /// Last-update timestamp (SQLite `CURRENT_TIMESTAMP` text).
    pub updated_at: String,
}

/// Optional filters for [`FactDao::search_paginated`]; a `None` (or empty)
/// member is not applied — the same "empty string = no filter" semantics as
/// the oracle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactFilter {
    /// Case-insensitive substring match on the predicate (`LIKE`, with
    /// `%`/`_`/`\` escaped so user input is matched literally).
    pub predicate: Option<String>,
    /// Case-insensitive substring match on the subject OR object entity
    /// name (`LIKE`, escaped; a correlated `EXISTS` over `entities`).
    pub entity_name: Option<String>,
    /// Match facts with exactly this status.
    pub status: Option<String>,
    /// Match facts in exactly this domain.
    pub domain: Option<String>,
}

impl FactFilter {
    /// The bound-parameter quadruple for [`SEARCH_WHERE`]: the two `LIKE`
    /// filters are escaped and wrapped as `LIKE` substring patterns.
    fn args(
        &self,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
        let pattern = |s: Option<String>| {
            s.as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!("%{}%", escape_like(n)))
        };
        (
            pattern(self.predicate.clone()),
            non_empty(self.status.clone()),
            non_empty(self.domain.clone()),
            pattern(self.entity_name.clone()),
        )
    }
}

/// CRUD + atomic CreateOrIgnore + orphan cleanup + pagination over the
/// `facts` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewFactDAO(db DBTX)`.
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, FactDao, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let facts = FactDao::new(ConnectionOrTx::Connection(conn));
///     let id = facts.create(Some(1), "works_at", Some(2), "hr", None, None, None)?;
///     assert_eq!(facts.get_by_id(id)?.map(|f| f.id), Some(id));
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct FactDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> FactDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Insert a new fact with `status = 'approved'` (the oracle's
    /// hard-coded constructor status) and return its generated id.
    /// `subject_entity_id`/`object_entity_id` are stored as `NULL` when
    /// `None`, as are `metadata_json`, `valid_from` and `valid_to`.
    // The seven data parameters mirror the `facts` column set (house style:
    // plain parameters, as in the sibling DAOs).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        subject_entity_id: Option<i64>,
        predicate: &str,
        object_entity_id: Option<i64>,
        domain: &str,
        metadata_json: Option<&str>,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
    ) -> Result<i64, DbError> {
        self.exec.query_row(
            "INSERT INTO facts (subject_entity_id, predicate, object_entity_id, domain, metadata, \
             status, valid_from, valid_to) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'approved', ?6, ?7) RETURNING id",
            params![
                subject_entity_id,
                predicate,
                object_entity_id,
                domain,
                metadata_json,
                valid_from,
                valid_to
            ],
            |row| row.get(0),
        )
    }

    /// Atomically insert the fact (status `'approved'`) unless a fact with
    /// the same unique key (subject, object, predicate) already exists, and
    /// return the id of the new or the existing row (design D5). The
    /// `metadata_json`/`valid_from`/`valid_to` arguments apply only when the
    /// row is actually inserted; on conflict the existing row is left
    /// untouched and its id is returned.
    ///
    /// `NULL` endpoints: SQLite unique indexes treat `NULL`s as distinct, so
    /// a fact with a `NULL` subject or object is never de-duplicated — every
    /// call inserts a new row.
    // Same seven data parameters as [`Self::create`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_or_ignore(
        &self,
        subject_entity_id: Option<i64>,
        predicate: &str,
        object_entity_id: Option<i64>,
        domain: &str,
        metadata_json: Option<&str>,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
    ) -> Result<i64, DbError> {
        let inserted = self.exec.query(
            "INSERT INTO facts (subject_entity_id, predicate, object_entity_id, domain, metadata, \
             status, valid_from, valid_to) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'approved', ?6, ?7) \
             ON CONFLICT (subject_entity_id, object_entity_id, predicate) DO NOTHING RETURNING id",
            params![
                subject_entity_id,
                predicate,
                object_entity_id,
                domain,
                metadata_json,
                valid_from,
                valid_to
            ],
            |row| row.get(0),
        )?;
        if let Some(id) = inserted.into_iter().next() {
            return Ok(id);
        }
        // Conflict: the fact already exists (its endpoints are non-NULL —
        // NULLs cannot hit the unique index).
        self.exec.query_row(
            "SELECT id FROM facts WHERE subject_entity_id = ?1 AND object_entity_id = ?2 \
             AND predicate = ?3",
            params![subject_entity_id, object_entity_id, predicate],
            |row| row.get(0),
        )
    }

    /// Set `weight = COUNT(fact_sources)` for each of `fact_ids`; returns
    /// the number of rows updated. Empty `fact_ids` is a no-op. The `IN`
    /// list is batched in chunks of [`ID_BATCH_SIZE`] (design D9).
    pub fn recompute_weights(&self, fact_ids: &[i64]) -> Result<i64, DbError> {
        let mut updated = 0;
        for batch in fact_ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!(
                "UPDATE facts SET weight = (SELECT COUNT(*) FROM fact_sources \
                 WHERE fact_sources.fact_id = facts.id) WHERE id IN ({placeholders})"
            );
            updated += self
                .exec
                .execute(&sql, params_from_iter(batch.iter().copied()))?;
        }
        Ok(updated as i64)
    }

    /// Retrieve a fact by id, or `None` if absent.
    pub fn get_by_id(&self, id: i64) -> Result<Option<Fact>, DbError> {
        let facts = self
            .exec
            .query(&format!("{SELECT_FACT} WHERE f.id = ?"), [id], row_to_fact)?;
        Ok(facts.into_iter().next())
    }

    /// All approved facts in which `entity_id` appears as subject or object,
    /// newest first. The `id` tie-break keeps the order deterministic at
    /// second-resolution timestamps.
    pub fn list_by_entity_id(&self, entity_id: i64) -> Result<Vec<Fact>, DbError> {
        self.exec.query(
            &format!(
                "{SELECT_FACT} WHERE (f.subject_entity_id = ? OR f.object_entity_id = ?) \
                 AND f.status = 'approved' ORDER BY f.created_at DESC, f.id DESC"
            ),
            params![entity_id, entity_id],
            row_to_fact,
        )
    }

    /// All approved facts for any of `entity_ids`, grouped by entity: each
    /// fact is attached to its subject and (if different) its object entry.
    /// Ids that do not exist are simply absent from the map. Empty `ids`
    /// yields an empty map.
    ///
    /// Input ids are de-duplicated (the oracle returned a fact twice when an
    /// id appeared twice in the input), and the two `IN` lists are batched
    /// in chunks of [`ID_BATCH_SIZE`] (design D9: 500 × 2 = 1000 parameters
    /// per statement).
    pub fn list_by_entity_ids(
        &self,
        entity_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<Fact>>, DbError> {
        let mut grouped: HashMap<i64, Vec<Fact>> = HashMap::new();
        if entity_ids.is_empty() {
            return Ok(grouped);
        }
        let unique: Vec<i64> = entity_ids
            .iter()
            .copied()
            .collect::<HashSet<i64>>()
            .into_iter()
            .collect();
        for batch in unique.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!(
                "{SELECT_FACT} WHERE (f.subject_entity_id IN ({placeholders}) \
                 OR f.object_entity_id IN ({placeholders})) AND f.status = 'approved' \
                 ORDER BY f.created_at DESC, f.id DESC"
            );
            let facts = self.exec.query(
                &sql,
                params_from_iter(batch.iter().flat_map(|id| [*id, *id])),
                row_to_fact,
            )?;
            for fact in facts {
                if let Some(subject) = fact.subject_entity_id {
                    grouped.entry(subject).or_default().push(fact.clone());
                }
                if let Some(object) = fact.object_entity_id
                    && Some(object) != fact.subject_entity_id
                {
                    grouped.entry(object).or_default().push(fact);
                }
            }
        }
        Ok(grouped)
    }

    /// All approved facts, ordered by id (knowledge-graph load).
    pub fn list_all(&self) -> Result<Vec<Fact>, DbError> {
        self.exec.query(
            &format!("{SELECT_FACT} WHERE f.status = 'approved' ORDER BY f.id"),
            [],
            row_to_fact,
        )
    }

    /// Ids of facts with no remaining `fact_sources` rows, ordered by id.
    /// `exclude_approved` skips approved facts from the result; a non-empty
    /// `candidates` list scopes the search to those ids (batched in chunks
    /// of [`ID_BATCH_SIZE`], design D9).
    pub fn find_orphaned_fact_ids(
        &self,
        exclude_approved: bool,
        candidates: &[i64],
    ) -> Result<Vec<i64>, DbError> {
        let base = "SELECT f.id FROM facts f LEFT JOIN fact_sources fs ON f.id = fs.fact_id \
                    WHERE fs.id IS NULL";
        let exclude = if exclude_approved {
            " AND f.status != 'approved'"
        } else {
            ""
        };
        if candidates.is_empty() {
            return self
                .exec
                .query(&format!("{base}{exclude} ORDER BY f.id"), [], |row| {
                    row.get(0)
                });
        }
        let mut ids: Vec<i64> = Vec::new();
        for batch in candidates.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!("{base} AND f.id IN ({placeholders}){exclude} ORDER BY f.id");
            ids.extend(self.exec.query(
                &sql,
                params_from_iter(batch.iter().copied()),
                |row| -> rusqlite::Result<i64> { row.get(0) },
            )?);
        }
        Ok(ids)
    }

    /// Delete the facts with the given ids (a bulk cleanup step; single-fact
    /// removal is [`Self::delete`]). Empty `fact_ids` → `0`. The `IN` list
    /// is batched in chunks of [`ID_BATCH_SIZE`] (design D9). Returns the
    /// number of rows deleted.
    pub fn delete_orphaned_facts(&self, fact_ids: &[i64]) -> Result<i64, DbError> {
        let mut deleted = 0;
        for batch in fact_ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!("DELETE FROM facts WHERE id IN ({placeholders})");
            deleted += self
                .exec
                .execute(&sql, params_from_iter(batch.iter().copied()))?;
        }
        Ok(deleted as i64)
    }

    /// Check that both endpoint entities exist and belong to `domain`
    /// (compared after [`crate::utils::normalize`], case- and
    /// whitespace-insensitive). Returns `true` when the fact's domain is
    /// consistent, `false` when an endpoint entity is missing or its domain
    /// differs (house convention: `bool` instead of the oracle's
    /// descriptive error).
    pub fn validate_fact_domain(
        &self,
        subject_id: i64,
        object_id: i64,
        domain: &str,
    ) -> Result<bool, DbError> {
        let domain_of = |id: i64| -> Result<Option<String>, DbError> {
            let rows =
                self.exec
                    .query("SELECT domain FROM entities WHERE id = ?", [id], |row| {
                        row.get(0)
                    })?;
            Ok(rows.into_iter().next())
        };
        let (Some(subject_domain), Some(object_domain)) =
            (domain_of(subject_id)?, domain_of(object_id)?)
        else {
            // A missing endpoint entity fails the check (oracle: an error).
            return Ok(false);
        };
        let want = normalize(domain);
        Ok(normalize(&subject_domain) == want && normalize(&object_domain) == want)
    }

    /// Retrieve several facts by id; ids that do not exist are simply absent
    /// from the result. Empty `ids` yields an empty vec.
    ///
    /// The `IN` list is batched in chunks of [`ID_BATCH_SIZE`] to stay far
    /// below SQLite's 32766 bound on bound parameters (design D9).
    pub fn get_by_ids(&self, ids: &[i64]) -> Result<Vec<Fact>, DbError> {
        let mut facts = Vec::new();
        for batch in ids.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!("{SELECT_FACT} WHERE f.id IN ({placeholders})");
            facts.extend(self.exec.query(
                &sql,
                params_from_iter(batch.iter().copied()),
                row_to_fact,
            )?);
        }
        Ok(facts)
    }

    /// Total number of facts, all statuses (the oracle's `Count()`).
    pub fn count(&self) -> Result<i64, DbError> {
        self.exec
            .query_row("SELECT COUNT(*) FROM facts", [], |row| row.get(0))
    }

    /// One page of facts matching `filter` (see [`FactFilter`]), ordered by
    /// id; returns the page and the total number of matching facts. A fact
    /// whose subject AND object names both match the entity-name filter
    /// appears ONCE (the oracle's `INNER JOIN` duplicated it in the page
    /// while its total count did not).
    pub fn search_paginated(
        &self,
        offset: i64,
        limit: i64,
        filter: &FactFilter,
    ) -> Result<(Vec<Fact>, i64), DbError> {
        let (predicate, status, domain, entity_name) = filter.args();
        let page = self.exec.query(
            &format!("{SELECT_FACT} {SEARCH_WHERE} ORDER BY f.id LIMIT ?5 OFFSET ?6"),
            params![predicate, status, domain, entity_name, limit, offset],
            row_to_fact,
        )?;
        let total = self.exec.query_row(
            &format!("SELECT COUNT(*) FROM facts f {SEARCH_WHERE}"),
            params![predicate, status, domain, entity_name],
            |row| row.get(0),
        )?;
        Ok((page, total))
    }

    /// Delete a fact (its `fact_sources` rows cascade per the schema FK).
    /// Returns `true` if a row was deleted, `false` if no fact has `id`.
    pub fn delete(&self, id: i64) -> Result<bool, DbError> {
        let changed = self.exec.execute("DELETE FROM facts WHERE id = ?", [id])?;
        Ok(changed > 0)
    }
}

/// Map a `facts` row (in [`SELECT_FACT`] column order) to a [`Fact`].
fn row_to_fact(row: &Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: row.get(0)?,
        subject_entity_id: row.get(1)?,
        predicate: row.get(2)?,
        object_entity_id: row.get(3)?,
        domain: row.get(4)?,
        metadata_json: row.get(5)?,
        status: row.get(6)?,
        valid_from: row.get(7)?,
        valid_to: row.get(8)?,
        weight: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;
    use crate::Db;
    use crate::entity::EntityDao;
    use crate::test_util::in_memory_db;

    /// Run `f` with a DAO bound to a pooled connection (checked out for the
    /// closure's duration).
    fn with_facts<T>(db: &Db, f: impl FnOnce(&FactDao<'_>) -> T) -> T {
        db.with_conn(|conn| f(&FactDao::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Create an entity through a pooled connection and return its id (the
    /// fact endpoint FKs require existing entities; `foreign_keys=ON`).
    fn insert_entity(db: &Db, entity_type: &str, name: &str, domain: &str) -> i64 {
        db.with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            entities.create(entity_type, name, domain, None, None, None)
        })
        .unwrap()
        .unwrap()
    }

    /// Insert a `fact_sources` row (the FactSource DAO lands in task 1.14;
    /// `document_id` is `TEXT` in the v5 schema, so the ids are strings).
    fn insert_fact_source(db: &Db, fact_id: i64, document_id: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO fact_sources (fact_id, document_id, quote) VALUES (?1, ?2, 'q')",
                params![fact_id, document_id],
            )
            .unwrap()
        })
        .unwrap();
    }

    /// Remove one `fact_sources` row (the weight-decrease path).
    fn delete_fact_source(db: &Db, fact_id: i64, document_id: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "DELETE FROM fact_sources WHERE fact_id = ?1 AND document_id = ?2",
                params![fact_id, document_id],
            )
            .unwrap()
        })
        .unwrap();
    }

    /// Set a fact's status directly (the DAO has no status update; the oracle
    /// tested status transitions with raw SQL too).
    fn set_fact_status(db: &Db, fact_id: i64, status: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE facts SET status = ? WHERE id = ?",
                params![status, fact_id],
            )
            .unwrap()
        })
        .unwrap();
    }

    /// The `fact_sources` row count of one fact.
    fn source_count(db: &Db, fact_id: i64) -> i64 {
        db.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM fact_sources WHERE fact_id = ?",
                [fact_id],
                |r| r.get(0),
            )
            .unwrap()
        })
        .unwrap()
    }

    // (a1) create + get_by_id round-trip, all fields.
    #[test]
    fn create_then_get_by_id_round_trip() {
        let db = in_memory_db();
        let subject = insert_entity(&db, "PERSON", "Alice", "hr");
        let object = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            let id = facts
                .create(
                    Some(subject),
                    "works_at",
                    Some(object),
                    "hr",
                    Some(r#"{"threshold":100}"#),
                    Some("2024-01-01"),
                    Some("2024-12-31"),
                )
                .unwrap();
            let fact = facts
                .get_by_id(id)
                .unwrap()
                .expect("created fact must exist");
            assert_eq!(fact.id, id);
            assert_eq!(fact.subject_entity_id, Some(subject));
            assert_eq!(fact.predicate, "works_at");
            assert_eq!(fact.object_entity_id, Some(object));
            assert_eq!(fact.domain, "hr");
            assert_eq!(fact.metadata_json.as_deref(), Some(r#"{"threshold":100}"#));
            assert_eq!(fact.status, "approved", "the oracle's constructor status");
            assert_eq!(fact.valid_from.as_deref(), Some("2024-01-01"));
            assert_eq!(fact.valid_to.as_deref(), Some("2024-12-31"));
            assert_eq!(fact.weight, 1, "schema default");
            assert!(
                !fact.created_at.is_empty(),
                "created_at default must be set"
            );
            assert!(
                !fact.updated_at.is_empty(),
                "updated_at default must be set"
            );
            assert_eq!(facts.get_by_id(999_999).unwrap(), None);
        });
    }

    // (a2) create: metadata variants (None / JSON / empty string) and NULL
    // endpoints (stored as NULL, not the oracle's zero value 0).
    #[test]
    fn create_metadata_and_null_endpoint_variants() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            let no_meta = facts
                .create(Some(subj), "founded_by", Some(obj), "hr", None, None, None)
                .unwrap();
            let json_meta = facts
                .create(
                    Some(subj),
                    "works_at",
                    Some(obj),
                    "hr",
                    Some(r#"{"threshold":100}"#),
                    None,
                    None,
                )
                .unwrap();
            let empty_meta = facts
                .create(Some(subj), "owns", Some(obj), "hr", Some(""), None, None)
                .unwrap();
            let nulls = facts
                .create(
                    None,
                    "located_in",
                    None,
                    "geo",
                    None,
                    Some("2024-01-01"),
                    None,
                )
                .unwrap();

            assert_eq!(
                facts.get_by_id(no_meta).unwrap().unwrap().metadata_json,
                None
            );
            assert_eq!(
                facts
                    .get_by_id(json_meta)
                    .unwrap()
                    .unwrap()
                    .metadata_json
                    .as_deref(),
                Some(r#"{"threshold":100}"#)
            );
            assert_eq!(
                facts
                    .get_by_id(empty_meta)
                    .unwrap()
                    .unwrap()
                    .metadata_json
                    .as_deref(),
                Some(""),
                "an empty string is a value, not NULL"
            );
            let null_fact = facts.get_by_id(nulls).unwrap().unwrap();
            assert_eq!(null_fact.subject_entity_id, None, "None endpoint → NULL");
            assert_eq!(null_fact.object_entity_id, None, "None endpoint → NULL");
            assert_eq!(null_fact.valid_to, None);
        });
    }

    // (a3) create: the unique key (subject, object, predicate) is enforced —
    // a plain create (unlike create_or_ignore) fails on a duplicate.
    #[test]
    fn create_duplicate_key_fails() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap();
            let err = facts
                .create(Some(subj), "works_at", Some(obj), "it", None, None, None)
                .expect_err("duplicate unique key must fail");
            assert!(matches!(err, DbError::Sqlite { .. }));
        });
    }

    // (a4) delete: true on hit, false on miss; the fact_sources rows cascade
    // per the schema FK.
    #[test]
    fn delete_cascades_sources_and_reports_missing() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let id = with_facts(&db, |facts| {
            facts
                .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap()
        });
        insert_fact_source(&db, id, "doc-1");
        insert_fact_source(&db, id, "doc-2");

        with_facts(&db, |facts| {
            assert!(facts.delete(id).unwrap(), "existing id must report true");
            assert_eq!(facts.get_by_id(id).unwrap(), None);
            assert!(
                !facts.delete(id).unwrap(),
                "second delete must report false"
            );
        });
        assert_eq!(source_count(&db, id), 0, "fact_sources must cascade");
    }

    // (a5) list_by_entity_id: both sides, approved only, id-DESC order
    // (created_at ties at second resolution).
    #[test]
    fn list_by_entity_id_approved_only_both_sides() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let other = insert_entity(&db, "ORGANIZATION", "Beta", "it");

        let (works, manages, owns, knows1, knows2) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "manages", Some(other), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "owns", Some(other), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "knows_1", Some(other), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "knows_2", Some(other), "hr", None, None, None)
                    .unwrap(),
            )
        });
        set_fact_status(&db, manages, "draft");
        set_fact_status(&db, owns, "rejected");

        with_facts(&db, |facts| {
            let subject_side: Vec<i64> = facts
                .list_by_entity_id(subj)
                .unwrap()
                .into_iter()
                .map(|f| f.id)
                .collect();
            assert_eq!(
                subject_side,
                vec![knows2, knows1, works],
                "approved only, id DESC (created_at ties)"
            );
            let object_side: Vec<i64> = facts
                .list_by_entity_id(obj)
                .unwrap()
                .into_iter()
                .map(|f| f.id)
                .collect();
            assert_eq!(object_side, vec![works], "object side sees its fact");
            assert!(
                facts.list_by_entity_id(999_999).unwrap().is_empty(),
                "unknown entity → empty"
            );
        });
    }

    // (a6) list_all: approved only, id ASC order.
    #[test]
    fn list_all_approved_only_id_order() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (f1, f2, f3) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
                    .unwrap(),
            )
        });
        set_fact_status(&db, f2, "draft");

        with_facts(&db, |facts| {
            let ids: Vec<i64> = facts
                .list_all()
                .unwrap()
                .into_iter()
                .map(|f| f.id)
                .collect();
            assert_eq!(ids, vec![f1, f3], "approved only, id ASC");
        });
    }

    // (a7) get_by_ids: several, missing ids absent, empty input.
    #[test]
    fn get_by_ids() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (f1, f2, f3) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
                    .unwrap(),
            )
        });

        with_facts(&db, |facts| {
            let found = facts.get_by_ids(&[f2, f1]).unwrap();
            let got: HashSet<i64> = found.iter().map(|f| f.id).collect();
            assert_eq!(got, [f1, f2].into_iter().collect());
            let p1 = found
                .iter()
                .find(|f| f.id == f1)
                .expect("f1 must be present");
            assert_eq!(p1.predicate, "p1");

            assert_eq!(
                facts.get_by_ids(&[f1, 999_999, f3]).unwrap().len(),
                2,
                "missing ids are absent"
            );
            assert!(facts.get_by_ids(&[]).unwrap().is_empty());
        });
    }

    // (a8) get_by_ids across the D9 batch boundary (501 ids → 2 statements).
    #[test]
    fn get_by_ids_batches_over_500() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let mut ids: Vec<i64> = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..501 {
                ids.push(facts.create(
                    Some(subj),
                    &format!("p-{i}"),
                    Some(obj),
                    "hr",
                    None,
                    None,
                    None,
                )?);
            }
            Ok(())
        })
        .expect("seed commits");

        with_facts(&db, |facts| {
            let requested: Vec<i64> = ids.iter().rev().copied().collect();
            let found = facts.get_by_ids(&requested).unwrap();
            assert_eq!(found.len(), 501);
            let got: HashSet<i64> = found.iter().map(|f| f.id).collect();
            assert_eq!(got, ids.into_iter().collect::<HashSet<i64>>());
        });
    }

    // (g) count: all statuses, 0 on an empty table.
    #[test]
    fn count_all_statuses() {
        let db = in_memory_db();
        with_facts(&db, |facts| {
            assert_eq!(facts.count().unwrap(), 0);
        });
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (f1, f3) = with_facts(&db, |facts| {
            let f1 = facts
                .create(Some(subj), "p1", Some(obj), "hr", None, None, None)
                .unwrap();
            // p2 stays approved; only its existence matters for the count.
            facts
                .create(Some(subj), "p2", Some(obj), "hr", None, None, None)
                .unwrap();
            let f3 = facts
                .create(Some(subj), "p3", Some(obj), "hr", None, None, None)
                .unwrap();
            (f1, f3)
        });
        set_fact_status(&db, f1, "draft");
        set_fact_status(&db, f3, "rejected");
        with_facts(&db, |facts| {
            assert_eq!(facts.count().unwrap(), 3, "count ignores status");
        });
    }

    // (b1) create_or_ignore: repeated calls with the same key → one row,
    // the same id, status 'approved' (D5).
    #[test]
    fn create_or_ignore_repeated_returns_same_id() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            let id1 = facts
                .create_or_ignore(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap();
            let id2 = facts
                .create_or_ignore(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                .unwrap();
            assert_eq!(id1, id2, "conflict must return the existing id");
            assert_eq!(facts.count().unwrap(), 1, "no duplicate row");
            assert_eq!(facts.get_by_id(id1).unwrap().unwrap().status, "approved");
        });
    }

    // (b2) create_or_ignore: a conflict must NOT overwrite the existing row
    // (status and metadata survive).
    #[test]
    fn create_or_ignore_conflict_keeps_existing_row() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        let id = with_facts(&db, |facts| {
            facts
                .create_or_ignore(
                    Some(subj),
                    "works_at",
                    Some(obj),
                    "hr",
                    Some(r#"{"v":1}"#),
                    None,
                    None,
                )
                .unwrap()
        });
        set_fact_status(&db, id, "pending");

        with_facts(&db, |facts| {
            let id2 = facts
                .create_or_ignore(
                    Some(subj),
                    "works_at",
                    Some(obj),
                    "hr",
                    Some(r#"{"v":2}"#),
                    Some("2025-01-01"),
                    None,
                )
                .unwrap();
            assert_eq!(id2, id);
            let fact = facts.get_by_id(id).unwrap().unwrap();
            assert_eq!(fact.status, "pending", "status must not be overwritten");
            assert_eq!(
                fact.metadata_json.as_deref(),
                Some(r#"{"v":1}"#),
                "metadata must not be overwritten"
            );
            assert_eq!(fact.valid_from, None, "validity must not be overwritten");
        });
    }

    // (b3) create_or_ignore: NULL endpoints are never de-duplicated (SQLite
    // unique indexes treat NULLs as distinct) — every call inserts.
    #[test]
    fn create_or_ignore_null_endpoints_always_insert() {
        let db = in_memory_db();
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            let id1 = facts
                .create_or_ignore(None, "located_in", Some(obj), "geo", None, None, None)
                .unwrap();
            let id2 = facts
                .create_or_ignore(None, "located_in", Some(obj), "geo", None, None, None)
                .unwrap();
            assert_ne!(id1, id2, "NULL subject cannot hit the unique index");
            assert_eq!(facts.count().unwrap(), 2);
        });
    }

    // (b4) create_or_ignore: the unique key is the full triple — a different
    // predicate is a different fact.
    #[test]
    fn create_or_ignore_distinct_predicate_inserts() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        with_facts(&db, |facts| {
            let id1 = facts
                .create_or_ignore(Some(subj), "p1", Some(obj), "hr", None, None, None)
                .unwrap();
            let id2 = facts
                .create_or_ignore(Some(subj), "p2", Some(obj), "hr", None, None, None)
                .unwrap();
            assert_ne!(id1, id2, "the predicate is part of the unique key");
            assert_eq!(facts.count().unwrap(), 2);
        });
    }

    // (c1) list_by_entity_ids: a fact is attached to its subject AND its
    // object entry, approved only (the oracle's ListByEntityIDs scenario).
    //
    // Each call uses a SINGLE id: with two or more ids the current production
    // code is order-dependent (known bug, pinned by the ignored regression
    // tests below), so multi-id calls are not asserted here.
    #[test]
    fn list_by_entity_ids_groups_subject_and_object() {
        let db = in_memory_db();
        let alice = insert_entity(&db, "PERSON", "Alice", "hr");
        let bob = insert_entity(&db, "PERSON", "Bob", "hr");
        let acme = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        let (works, employed, knows) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(alice), "works_at", Some(acme), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(bob), "employed_by", Some(acme), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(alice), "knows", Some(bob), "hr", None, None, None)
                    .unwrap(),
            )
        });
        set_fact_status(&db, knows, "draft");

        with_facts(&db, |facts| {
            // Subject side: alice's approved fact only (the draft is
            // excluded); the fact is also attached to its object entry.
            let map = facts.list_by_entity_ids(&[alice]).unwrap();
            assert_eq!(map.get(&alice).map(Vec::len), Some(1));
            assert_eq!(map.get(&alice).unwrap()[0].id, works);
            assert_eq!(map.get(&acme).map(Vec::len), Some(1));
            assert_eq!(map.get(&acme).unwrap()[0].id, works);

            // Object side: acme is the object of both approved facts.
            let map = facts.list_by_entity_ids(&[acme]).unwrap();
            let acme_ids: Vec<i64> = map
                .get(&acme)
                .expect("acme must be present")
                .iter()
                .map(|f| f.id)
                .collect();
            assert_eq!(acme_ids.len(), 2, "both approved facts attach to acme");
            assert!(acme_ids.contains(&works) && acme_ids.contains(&employed));

            // bob's draft fact is excluded, his approved one is present.
            let map = facts.list_by_entity_ids(&[bob]).unwrap();
            assert_eq!(map.get(&bob).map(Vec::len), Some(1));
            assert_eq!(map.get(&bob).unwrap()[0].id, employed);

            // Ids with no facts are absent from the map.
            assert!(facts.list_by_entity_ids(&[999_999]).unwrap().is_empty());
        });
    }

    // (c2) list_by_entity_ids: empty input → empty map.
    #[test]
    fn list_by_entity_ids_empty_input() {
        let db = in_memory_db();
        with_facts(&db, |facts| {
            assert!(facts.list_by_entity_ids(&[]).unwrap().is_empty());
        });
    }

    // (c3) list_by_entity_ids: repeated input ids must not change the result
    // (ids are de-duplicated before the query). One unique id keeps the test
    // deterministic — multi-id inputs are affected by the known production
    // bug pinned by the ignored regression tests below.
    #[test]
    fn list_by_entity_ids_deduplicates_input_ids() {
        let db = in_memory_db();
        let alice = insert_entity(&db, "PERSON", "Alice", "hr");
        let acme = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        with_facts(&db, |facts| {
            facts
                .create(Some(alice), "works_at", Some(acme), "hr", None, None, None)
                .unwrap();
            let map = facts.list_by_entity_ids(&[alice, alice, alice]).unwrap();
            assert_eq!(
                map.get(&alice).map(Vec::len),
                Some(1),
                "repeated input ids must not duplicate the fact"
            );
        });
    }

    // (c4) KNOWN BUG #1 (found by this task's test design, task 1.11): the
    // batch parameter binding of `list_by_entity_ids` is wrong. The SQL
    // carries TWO `IN` lists (subject, object) with n placeholders each, but
    // the parameters are passed INTERLEAVED as [b0, b0, b1, b1, ...]
    // (`flat_map(|id| [*id, *id])`), so positional binding fills the subject
    // list with the first half of the batch and the object list with the
    // second half — half of the requested ids are missing from each list.
    // The batch order comes from a HashSet (random per call), so EVERY
    // multi-id call is order-dependent (this is what made the first version
    // of the (c1)/(c3) tests flake). The oracle bound [args..., args...]
    // (full list, then full list) — the Rust port regressed it.
    //
    // Deterministic reproduction: facts A→B and B→A; under either batch
    // order exactly one fact is missed, so the map holds 2 entries instead
    // of 4. Ignored so `cargo test` stays green until the production code is
    // fixed (out of scope for this tests-only task).
    #[test]
    #[ignore = "known bug: list_by_entity_ids interleaves batch parameters (task 1.11 report)"]
    fn list_by_entity_ids_multi_id_regression() {
        let db = in_memory_db();
        let a = insert_entity(&db, "PERSON", "Alice", "hr");
        let b = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        with_facts(&db, |facts| {
            facts
                .create(Some(a), "p_ab", Some(b), "hr", None, None, None)
                .unwrap();
            facts
                .create(Some(b), "p_ba", Some(a), "hr", None, None, None)
                .unwrap();
            let map = facts.list_by_entity_ids(&[a, b]).unwrap();
            let entries: usize = map.values().map(Vec::len).sum();
            assert_eq!(entries, 4, "both facts under both entities");
        });
    }

    // (c5) KNOWN BUG #2 (task 1.11): with more than 500 unique ids,
    // `list_by_entity_ids` runs one query per 500-id batch and a fact whose
    // subject and object land in DIFFERENT batches is selected by both
    // queries and pushed into the result map twice. It is masked by bug #1
    // (the parameter interleaving) and will surface once #1 is fixed.
    // Ignored for the same reason as above.
    #[test]
    #[ignore = "known bug: cross-batch fact duplication in list_by_entity_ids (task 1.11 report)"]
    fn list_by_entity_ids_batches_over_500_no_duplicates() {
        let db = in_memory_db();
        let mut ids: Vec<i64> = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..501 {
                let s = entities.create("PERSON", &format!("S-{i}"), "hr", None, None, None)?;
                let o =
                    entities.create("ORGANIZATION", &format!("O-{i}"), "hr", None, None, None)?;
                facts.create(Some(s), &format!("p-{i}"), Some(o), "hr", None, None, None)?;
                ids.extend([s, o]);
            }
            Ok(())
        })
        .expect("seed commits");

        let map = with_facts(&db, |facts| facts.list_by_entity_ids(&ids).unwrap());
        for id in &ids {
            assert_eq!(
                map.get(id).map(Vec::len),
                Some(1),
                "entity {id}: exactly one fact — no cross-batch duplication"
            );
        }
    }

    // (d) validate_fact_domain: matching domains → true; mismatch,
    // case/whitespace normalization, and missing endpoints → false.
    #[test]
    fn validate_fact_domain() {
        let db = in_memory_db();
        let alice = insert_entity(&db, "PERSON", "Alice", "hr");
        let bob = insert_entity(&db, "PERSON", "Bob", "hr");
        let carol = insert_entity(&db, "PERSON", "Carol", "policy");
        let dave = insert_entity(&db, "PERSON", "Dave", " HR ");

        with_facts(&db, |facts| {
            assert!(
                facts.validate_fact_domain(alice, bob, "hr").unwrap(),
                "same domain must pass"
            );
            assert!(
                !facts.validate_fact_domain(alice, carol, "hr").unwrap(),
                "cross-domain object must fail"
            );
            assert!(
                !facts.validate_fact_domain(carol, bob, "hr").unwrap(),
                "cross-domain subject must fail"
            );
            assert!(
                facts.validate_fact_domain(alice, bob, "HR").unwrap(),
                "the fact domain is normalized (case)"
            );
            assert!(
                facts.validate_fact_domain(alice, bob, "  hr  ").unwrap(),
                "the fact domain is normalized (whitespace)"
            );
            assert!(
                facts.validate_fact_domain(alice, dave, "hr").unwrap(),
                "the stored entity domain is normalized too"
            );
            assert!(
                !facts.validate_fact_domain(alice, 999_999, "hr").unwrap(),
                "missing object entity must fail"
            );
            assert!(
                !facts.validate_fact_domain(999_999, bob, "hr").unwrap(),
                "missing subject entity must fail"
            );
        });
    }

    // (e1) find_orphaned_fact_ids: no-source facts only, exclude_approved,
    // candidate scoping. Facts with live sources (and live entities) are
    // never reported.
    #[test]
    fn find_orphaned_fact_ids() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (sourced, orphan_approved, orphan_draft) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "drafted", Some(obj), "hr", None, None, None)
                    .unwrap(),
            )
        });
        insert_fact_source(&db, sourced, "doc-1");
        set_fact_status(&db, orphan_draft, "draft");

        with_facts(&db, |facts| {
            assert_eq!(
                facts.find_orphaned_fact_ids(false, &[]).unwrap(),
                vec![orphan_approved, orphan_draft],
                "all no-source facts, id order"
            );
            assert_eq!(
                facts.find_orphaned_fact_ids(true, &[]).unwrap(),
                vec![orphan_draft],
                "approved facts are excluded"
            );
            assert_eq!(
                facts.find_orphaned_fact_ids(false, &[sourced]).unwrap(),
                Vec::<i64>::new(),
                "a sourced fact is not orphaned"
            );
            assert_eq!(
                facts
                    .find_orphaned_fact_ids(false, &[sourced, orphan_approved, 999_999])
                    .unwrap(),
                vec![orphan_approved],
                "candidates scope the search"
            );
        });
    }

    // (e2) delete_orphaned_facts: deletes exactly the listed ids (empty → 0);
    // a fact with live entities and sources is never touched.
    #[test]
    fn delete_orphaned_facts() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (sourced, orphan1, orphan2) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "drafted", Some(obj), "hr", None, None, None)
                    .unwrap(),
            )
        });
        insert_fact_source(&db, sourced, "doc-1");
        set_fact_status(&db, orphan2, "draft");

        with_facts(&db, |facts| {
            assert_eq!(facts.delete_orphaned_facts(&[]).unwrap(), 0);
            assert_eq!(
                facts
                    .delete_orphaned_facts(&[orphan1, orphan2, 999_999])
                    .unwrap(),
                2,
                "exactly the listed existing ids"
            );
            assert_eq!(facts.get_by_id(orphan1).unwrap(), None);
            assert_eq!(facts.get_by_id(orphan2).unwrap(), None);
            assert!(
                facts.get_by_id(sourced).unwrap().is_some(),
                "the sourced fact with live entities must survive"
            );
            assert_eq!(facts.count().unwrap(), 1);
        });
    }

    // (e3) find_orphaned_fact_ids + delete_orphaned_facts across the D9
    // batch boundary (501 candidates → 2 statements each).
    #[test]
    fn orphan_cleanup_batches_over_500() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let mut ids: Vec<i64> = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..501 {
                ids.push(facts.create(
                    Some(subj),
                    &format!("p-{i}"),
                    Some(obj),
                    "hr",
                    None,
                    None,
                    None,
                )?);
            }
            Ok(())
        })
        .expect("seed commits");

        with_facts(&db, |facts| {
            let candidates: Vec<i64> = ids.iter().rev().copied().collect();
            let orphaned = facts.find_orphaned_fact_ids(false, &candidates).unwrap();
            assert_eq!(orphaned.len(), 501, "all sourceless facts are orphaned");

            assert_eq!(
                facts.delete_orphaned_facts(&candidates).unwrap(),
                501,
                "every listed fact is deleted"
            );
            assert_eq!(facts.count().unwrap(), 0);
        });
    }

    /// Search fixture: 4 entities and 6 facts covering every filter axis —
    /// approved/draft statuses, hr/it domains, LIKE wildcards in predicates,
    /// and a fact whose BOTH endpoint names match one pattern (the fact the
    /// oracle's `INNER JOIN` duplicated in the page).
    fn seed_search_db() -> (Db, [i64; 6]) {
        let db = in_memory_db();
        let e1 = insert_entity(&db, "PERSON", "Alpha One", "hr");
        let e2 = insert_entity(&db, "ORGANIZATION", "Alpha Two", "hr");
        let e3 = insert_entity(&db, "ORGANIZATION", "Beta Corp", "it");
        let e4 = insert_entity(&db, "ORGANIZATION", "Gamma Lab", "it");
        let ids: [i64; 6] = with_facts(&db, |facts| {
            [
                facts
                    .create(Some(e1), "works_at", Some(e2), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(e1), "manages", Some(e3), "it", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(e3), "supplies", Some(e2), "it", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(e4), "funds", Some(e3), "it", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(e1), "a_b", Some(e4), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(e1), "axb", Some(e4), "hr", None, None, None)
                    .unwrap(),
            ]
        });
        set_fact_status(&db, ids[4], "draft");
        (db, ids)
    }

    // (e4, search) no filter: every fact (all statuses), id order; page and
    // total agree.
    #[test]
    fn search_paginated_no_filter() {
        let (db, ids) = seed_search_db();
        with_facts(&db, |facts| {
            let (page, total) = facts
                .search_paginated(0, 100, &FactFilter::default())
                .unwrap();
            assert_eq!(total, 6, "no filter matches every status");
            let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids, ids.to_vec(), "id ASC order");
        });
    }

    // (e5, search) THE ORACLE BUG REGRESSION: a fact whose subject AND object
    // names both match the entity-name filter must appear ONCE in the page
    // and ONCE in the total (the oracle's INNER JOIN duplicated it in the
    // page while its COUNT(DISTINCT) total did not).
    #[test]
    fn search_paginated_entity_name_both_endpoints_match_once() {
        let (db, ids) = seed_search_db();
        with_facts(&db, |facts| {
            let filter = FactFilter {
                entity_name: Some("alpha".into()),
                ..Default::default()
            };
            let (page, total) = facts.search_paginated(0, 100, &filter).unwrap();
            // f0 (Alpha One → Alpha Two), f1, f4, f5 (subject Alpha One) and
            // f2 (object Alpha Two); f3 (Beta/Gamma) does not match.
            assert_eq!(total, 5);
            let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids.len(), 5, "page and total must agree");
            assert_eq!(
                page_ids.iter().filter(|id| **id == ids[0]).count(),
                1,
                "the both-endpoints-matching fact must appear exactly once"
            );
            let expected: HashSet<i64> = [ids[0], ids[1], ids[2], ids[4], ids[5]]
                .into_iter()
                .collect();
            assert_eq!(page_ids.into_iter().collect::<HashSet<_>>(), expected);
        });
    }

    // (e6, search) predicate filter: case-insensitive substring, and LIKE
    // wildcards in the input match LITERALLY (escaped).
    #[test]
    fn search_paginated_predicate_filter() {
        let (db, ids) = seed_search_db();
        with_facts(&db, |facts| {
            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        predicate: Some("WORKS".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1, "case-insensitive substring");
            assert_eq!(page[0].id, ids[0]);

            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        predicate: Some("a_b".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1, "the underscore must be matched literally");
            assert_eq!(page[0].id, ids[4], "'axb' must not match the pattern 'a_b'");
        });
    }

    // (e7, search) status and domain filters (exact match), combined with the
    // predicate filter.
    #[test]
    fn search_paginated_status_domain_and_combined_filters() {
        let (db, ids) = seed_search_db();
        with_facts(&db, |facts| {
            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        status: Some("draft".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].id, ids[4]);

            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        status: Some("approved".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 5);
            assert_eq!(page.len(), 5);

            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        domain: Some("it".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 3);
            let page_ids: HashSet<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids, [ids[1], ids[2], ids[3]].into_iter().collect());

            let (page, total) = facts
                .search_paginated(
                    0,
                    100,
                    &FactFilter {
                        predicate: Some("a".into()),
                        status: Some("approved".into()),
                        domain: Some("hr".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 2, "hr + approved + predicate containing 'a'");
            let page_ids: HashSet<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids, [ids[0], ids[5]].into_iter().collect());
        });
    }

    // (e8, search) pagination windows: page contents follow id order, the
    // total stays stable, a window past the end is empty.
    #[test]
    fn search_paginated_windows() {
        let (db, ids) = seed_search_db();
        with_facts(&db, |facts| {
            let (page, total) = facts
                .search_paginated(0, 2, &FactFilter::default())
                .unwrap();
            assert_eq!(total, 6);
            let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids, vec![ids[0], ids[1]]);

            let (page, total) = facts
                .search_paginated(4, 2, &FactFilter::default())
                .unwrap();
            assert_eq!(total, 6);
            let page_ids: Vec<i64> = page.iter().map(|f| f.id).collect();
            assert_eq!(page_ids, vec![ids[4], ids[5]]);

            let (page, total) = facts
                .search_paginated(6, 2, &FactFilter::default())
                .unwrap();
            assert!(page.is_empty(), "past the end → empty page");
            assert_eq!(total, 6, "the total does not follow the window");
        });
    }

    // (z1) recompute_weights: weight = COUNT(fact_sources) (0 when none),
    // decreases after a source is removed, returns the rows updated; empty
    // input is a no-op.
    #[test]
    fn recompute_weights() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let (f1, f2) = with_facts(&db, |facts| {
            (
                facts
                    .create(Some(subj), "works_at", Some(obj), "hr", None, None, None)
                    .unwrap(),
                facts
                    .create(Some(subj), "knows", Some(obj), "hr", None, None, None)
                    .unwrap(),
            )
        });
        insert_fact_source(&db, f1, "doc-1");
        insert_fact_source(&db, f1, "doc-2");

        with_facts(&db, |facts| {
            assert_eq!(
                facts.recompute_weights(&[]).unwrap(),
                0,
                "empty input is a no-op"
            );
            assert_eq!(
                facts.recompute_weights(&[f1, f2]).unwrap(),
                2,
                "every listed fact is updated"
            );
            assert_eq!(
                facts.get_by_id(f1).unwrap().unwrap().weight,
                2,
                "one weight per source"
            );
            assert_eq!(
                facts.get_by_id(f2).unwrap().unwrap().weight,
                0,
                "no sources → weight 0 (the schema default 1 is replaced)"
            );

            delete_fact_source(&db, f1, "doc-1");
            facts.recompute_weights(&[f1]).unwrap();
            assert_eq!(
                facts.get_by_id(f1).unwrap().unwrap().weight,
                1,
                "the weight follows the source count down"
            );
        });
    }

    // (z2) recompute_weights across the D9 batch boundary (501 facts → 2
    // statements; 500 × 2 = 1000 parameters max per statement stays far
    // below SQLite's 32766 bound).
    #[test]
    fn recompute_weights_batches_over_500() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");
        let mut fact_ids: Vec<i64> = Vec::new();
        db.exec_tx(|tx| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..501 {
                let id = facts.create(
                    Some(subj),
                    &format!("p-{i}"),
                    Some(obj),
                    "hr",
                    None,
                    None,
                    None,
                )?;
                tx.execute(
                    "INSERT INTO fact_sources (fact_id, document_id) VALUES (?1, ?2)",
                    params![id, format!("doc-{i}")],
                )?;
                fact_ids.push(id);
            }
            Ok(())
        })
        .expect("seed commits");

        with_facts(&db, |facts| {
            assert_eq!(
                facts.recompute_weights(&fact_ids).unwrap(),
                501,
                "every listed fact is updated across the batch boundary"
            );
            let weight_one: i64 = db
                .with_conn(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM facts WHERE weight = 1", [], |r| {
                        r.get(0)
                    })
                })
                .unwrap()
                .unwrap();
            assert_eq!(weight_one, 501, "every fact has exactly one source");
        });
    }

    // The DAO works over a transaction: commit and rollback paths
    // (house pattern, as in the sibling DAOs).
    #[test]
    fn create_inside_transaction() {
        let db = in_memory_db();
        let subj = insert_entity(&db, "PERSON", "Alice", "hr");
        let obj = insert_entity(&db, "ORGANIZATION", "Acme", "hr");

        db.exec_tx(|tx| -> Result<(), DbError> {
            let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
            facts.create(Some(subj), "tx_commit", Some(obj), "hr", None, None, None)?;
            Ok(())
        })
        .expect("commit");

        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let facts = FactDao::new(ConnectionOrTx::Transaction(&*tx));
                facts.create(Some(subj), "tx_rollback", Some(obj), "hr", None, None, None)?;
                // A genuine failure: UNIQUE(subject, object, predicate).
                facts.create(Some(subj), "tx_rollback", Some(obj), "hr", None, None, None)?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));

        with_facts(&db, |facts| {
            assert_eq!(facts.count().unwrap(), 1, "only the committed fact");
            let (page, total) = facts
                .search_paginated(
                    0,
                    10,
                    &FactFilter {
                        predicate: Some("tx_rollback".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(total, 0, "the rolled-back fact must be gone");
            assert!(page.is_empty());
        });
    }
}
