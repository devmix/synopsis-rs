//! Surface-name alias memory over the `entity_aliases` table (change
//! multilingual-entity-resolution, design D2/D3).
//!
//! **Design:**
//! - `insert_or_ignore` is the sole write path and is idempotent:
//!   `INSERT OR IGNORE` against BOTH uniqueness rules — a repeated
//!   `(entity_id, alias)` row and an alias already bound to a DIFFERENT
//!   entity (the UNIQUE alias index) are silently skipped. A missing
//!   `entity_id` is a programming error and surfaces as a
//!   [`DbError::Sqlite`] FK violation (`INSERT OR IGNORE` does not swallow
//!   FK violations; `foreign_keys=ON` is a D8 pragma).
//! - `aliases_of` orders by alias (deterministic, not arbitrary row order).
//! - `alias_map` hydrates the resolver's name→entity_id lookup map (design
//!   D3): repeated surface forms then skip all similarity work.
//!
//! Deletion of an entity cascades to its aliases per the schema FK
//! (no explicit cleanup method needed).

use std::collections::HashMap;

use rusqlite::params;

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Surface-name aliases of entities (`entity_aliases` table).
///
/// Every surface name that has resolved to an entity is recorded here, so
/// repeated surface forms resolve by lookup instead of similarity work
/// (multilingual-entity-resolution design D3).
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`].
///
/// # Examples
///
/// ```no_run
/// # use db::{ConnectionOrTx, Db, DbError, EntityAliasDao};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// db.with_conn(|conn| -> Result<(), DbError> {
///     let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
///     aliases.insert_or_ignore(1, "the city")?;
///     assert_eq!(aliases.aliases_of(1)?, vec!["the city".to_string()]);
///     Ok(())
/// })??;
/// # Ok(())
/// # }
/// ```
pub struct EntityAliasDao<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> EntityAliasDao<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Record `alias` as a surface name of `entity_id` (idempotent).
    ///
    /// `INSERT OR IGNORE` against BOTH uniqueness rules: a repeated
    /// `(entity_id, alias)` row and an alias already bound to a DIFFERENT
    /// entity (the UNIQUE alias index) are silently skipped. A missing
    /// `entity_id` surfaces as a [`DbError::Sqlite`] FK violation (a
    /// programming error — callers always pass a live id).
    pub fn insert_or_ignore(&self, entity_id: i64, alias: &str) -> Result<(), DbError> {
        self.exec.execute(
            "INSERT OR IGNORE INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            params![entity_id, alias],
        )?;
        Ok(())
    }

    /// All aliases of one entity, ordered by alias (deterministic).
    pub fn aliases_of(&self, entity_id: i64) -> Result<Vec<String>, DbError> {
        self.exec.query(
            "SELECT alias FROM entity_aliases WHERE entity_id = ? ORDER BY alias",
            [entity_id],
            |row| row.get(0),
        )
    }

    /// The full alias→entity_id map for resolver hydration (design D3):
    /// every recorded surface name mapped to the entity it resolves to.
    /// Well-defined because an alias names exactly one entity globally
    /// (the UNIQUE alias index).
    pub fn alias_map(&self) -> Result<HashMap<String, i64>, DbError> {
        let rows = self.exec.query(
            "SELECT alias, entity_id FROM entity_aliases ORDER BY alias",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }
}
