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
//!   and `get_by_ids` batch their `IN` lists in chunks of
//!   [`config::ID_BATCH_SIZE`] (design D9); the oracle built one unbounded
//!   placeholder list (potential 32766 bound-parameter violation).
//! - `list_by_entity_ids` de-duplicates its input ids: the oracle returned
//!   the same fact twice in one map slice when the same id appeared twice in
//!   the input.
//! - `list_by_entity_ids` binds the two `IN` lists as full-list-then-full-list
//!   (the oracle's `append(args, args...)`) and de-duplicates facts across
//!   `IN`-list batches by fact id: a fact whose endpoints land in different
//!   batches is selected by both queries and is attached to the map exactly
//!   once. (The first port interleaved the batch parameters —
//!   `flat_map(|id| [*id, *id])` — which positionally filled the subject list
//!   with half the batch and the object list with the other half, and it
//!   double-attached cross-batch facts; both regressions were fixed in task
//!   1.17.)
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

use config::ID_BATCH_SIZE;
use rusqlite::{Row, params, params_from_iter};

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};
use crate::utils::{escape_like, normalize};

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
    /// list is batched in chunks of [`config::ID_BATCH_SIZE`] (design D9).
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
    /// in chunks of [`config::ID_BATCH_SIZE`] (design D9: 500 × 2 = 1000
    /// parameters per statement). The parameters are bound
    /// full-list-then-full-list
    /// (the oracle's `append(args, args...)`), and a fact selected by more
    /// than one batch query (endpoints in different batches) is attached to
    /// the map exactly once, de-duplicated by fact id.
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
        // Facts already attached by an earlier batch: a fact whose subject
        // and object land in different batches is selected by both queries
        // and must be attached only once.
        let mut attached: HashSet<i64> = HashSet::new();
        for batch in unique.chunks(ID_BATCH_SIZE) {
            let placeholders = vec!["?"; batch.len()].join(", ");
            let sql = format!(
                "{SELECT_FACT} WHERE (f.subject_entity_id IN ({placeholders}) \
                  OR f.object_entity_id IN ({placeholders})) AND f.status = 'approved' \
                  ORDER BY f.created_at DESC, f.id DESC"
            );
            // Two `IN` lists: the full batch for the subject list, then the
            // full batch again for the object list (positional binding).
            let facts = self.exec.query(
                &sql,
                params_from_iter(batch.iter().chain(batch.iter())),
                row_to_fact,
            )?;
            for fact in facts {
                if !attached.insert(fact.id) {
                    continue;
                }
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
    /// of [`config::ID_BATCH_SIZE`], design D9).
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
    /// is batched in chunks of [`config::ID_BATCH_SIZE`] (design D9).
    /// Returns the number of rows deleted.
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
    /// The `IN` list is batched in chunks of [`config::ID_BATCH_SIZE`] to
    /// stay far below SQLite's 32766 bound on bound parameters (design D9).
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
