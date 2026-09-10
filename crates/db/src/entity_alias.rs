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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::entity::EntityDao;
    use crate::test_util::{in_memory_db, temp_file_db};

    /// Two entities of the same type/domain; returns their ids (a < b).
    fn seed_entities(db: &Db) -> (i64, i64) {
        db.with_conn(|conn| -> Result<(i64, i64), DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            let a = entities.create("PERSON", "Alice", "", None, None, None)?;
            let b = entities.create("PERSON", "Bob", "", None, None, None)?;
            Ok((a, b))
        })
        .unwrap()
        .unwrap()
    }

    // (acceptance) Fresh-DB schema: the table and the UNIQUE alias index
    // exist, both columns are NOT NULL, PRAGMA user_version advanced to 2,
    // and the constraints reject a duplicate (entity_id, alias) and a
    // duplicate alias of another entity (plain INSERTs — the DAO path is
    // deliberately idempotent).
    #[test]
    fn fresh_db_has_entity_aliases_table_and_user_version_2() {
        let db = temp_file_db();
        let (a, b) = seed_entities(&db);

        // PRAGMA user_version advanced 1 -> 2 (the migration count, D3).
        let user_version: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(
            user_version, 2,
            "PRAGMA user_version must be 2 after migration 2-entity-aliases"
        );

        let (table, index): (i64, i64) = db
            .with_conn(|conn| {
                let table = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'table' AND name = 'entity_aliases'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let index = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master \
                         WHERE type = 'index' AND name = 'idx_entity_aliases_alias'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                (table, index)
            })
            .unwrap();
        assert_eq!(table, 1, "entity_aliases table must exist");
        assert_eq!(index, 1, "idx_entity_aliases_alias (UNIQUE) must exist");

        // PRAGMA table_info columns: (name, notnull).
        let columns: Vec<(String, i64)> = db
            .with_conn(|conn| {
                conn.prepare("PRAGMA table_info(entity_aliases)")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(1)?, r.get(3)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            })
            .unwrap();
        assert_eq!(
            columns,
            vec![("entity_id".to_string(), 1), ("alias".to_string(), 1)],
            "both columns must be NOT NULL: {columns:?}"
        );

        // A duplicate (entity_id, alias) violates UNIQUE (entity_id, alias).
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
                params![a, "first name"],
            )
            .unwrap();
            let err = conn
                .execute(
                    "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
                    params![a, "first name"],
                )
                .expect_err("duplicate (entity_id, alias) must be rejected");
            assert!(
                err.to_string().contains("UNIQUE constraint failed"),
                "expected a UNIQUE violation, got {err}"
            );
        })
        .unwrap();

        // A duplicate alias of ANOTHER entity violates the UNIQUE alias
        // index (an alias names exactly one entity globally).
        db.with_conn(|conn| {
            let err = conn
                .execute(
                    "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
                    params![b, "first name"],
                )
                .expect_err("a duplicate alias of another entity must be rejected");
            assert!(
                err.to_string().contains("entity_aliases.alias"),
                "expected the UNIQUE alias index to fail, got {err}"
            );
        })
        .unwrap();

        // The FK references entities(id): foreign_key_list columns are
        // (id, seq, table, from, to, on_update, on_delete, match).
        let fk: Vec<(String, String, String)> = db
            .with_conn(|conn| {
                conn.prepare("PRAGMA foreign_key_list(entity_aliases)")
                    .unwrap()
                    .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get(3)?, r.get(4)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            })
            .unwrap();
        assert_eq!(
            fk,
            vec![(
                "entities".to_string(),
                "entity_id".to_string(),
                "id".to_string()
            )],
            "entity_aliases.entity_id must reference entities(id)"
        );
    }

    // insert_or_ignore is idempotent: the same (entity_id, alias) twice
    // yields exactly one row.
    #[test]
    fn insert_or_ignore_is_idempotent() {
        let db = in_memory_db();
        let (a, _) = seed_entities(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            aliases.insert_or_ignore(a, "the city")?;
            aliases.insert_or_ignore(a, "the city")?;
            assert_eq!(aliases.aliases_of(a)?, vec!["the city".to_string()]);
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM entity_aliases", [], |r| r.get(0))?;
            assert_eq!(count, 1, "the repeated pair must not duplicate a row");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // insert_or_ignore skips an alias already bound to a DIFFERENT entity
    // (the UNIQUE alias index) without error — the first binding wins.
    #[test]
    fn insert_or_ignore_skips_alias_bound_to_other_entity() {
        let db = in_memory_db();
        let (a, b) = seed_entities(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            aliases.insert_or_ignore(a, "shared name")?;
            aliases.insert_or_ignore(b, "shared name")?; // silently skipped
            assert_eq!(
                aliases.aliases_of(a)?,
                vec!["shared name".to_string()],
                "the first binding survives"
            );
            assert!(
                aliases.aliases_of(b)?.is_empty(),
                "the second entity must not gain the alias"
            );
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM entity_aliases", [], |r| r.get(0))?;
            assert_eq!(count, 1);
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // insert_or_ignore with a missing entity_id surfaces the FK violation
    // as DbError::Sqlite (INSERT OR IGNORE does not swallow FK violations;
    // a missing id is a programming error).
    #[test]
    fn insert_or_ignore_missing_entity_fails() {
        let db = in_memory_db();

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            let err = aliases
                .insert_or_ignore(999_999, "ghost")
                .expect_err("a missing entity_id must fail");
            assert!(
                matches!(err, DbError::Sqlite { .. }),
                "expected a Sqlite FK error, got {err:?}"
            );
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM entity_aliases", [], |r| r.get(0))?;
            assert_eq!(count, 0, "no row for a nonexistent entity");
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // aliases_of: several aliases ordered by alias; unknown entity → empty.
    #[test]
    fn aliases_of_orders_by_alias() {
        let db = in_memory_db();
        let (a, _) = seed_entities(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            aliases.insert_or_ignore(a, "zebra")?;
            aliases.insert_or_ignore(a, "alpha")?;
            aliases.insert_or_ignore(a, "mike")?;
            assert_eq!(
                aliases.aliases_of(a)?,
                vec!["alpha".to_string(), "mike".to_string(), "zebra".to_string()],
                "ordered by alias"
            );
            assert!(
                aliases.aliases_of(999_999)?.is_empty(),
                "unknown entity → empty"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // alias_map: every recorded surface name mapped to its entity (resolver
    // hydration, design D3).
    #[test]
    fn alias_map_maps_every_alias_to_its_entity() {
        let db = in_memory_db();
        let (a, b) = seed_entities(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            aliases.insert_or_ignore(a, "alpha")?;
            aliases.insert_or_ignore(b, "beta")?;
            aliases.insert_or_ignore(a, "gamma")?;
            let map = aliases.alias_map()?;
            assert_eq!(map.get("alpha"), Some(&a));
            assert_eq!(map.get("beta"), Some(&b));
            assert_eq!(map.get("gamma"), Some(&a));
            assert_eq!(map.len(), 3);
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // alias_map of an empty table is an empty map.
    #[test]
    fn alias_map_empty_table() {
        let db = in_memory_db();

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            assert!(aliases.alias_map()?.is_empty());
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    // Schema FK cascade: deleting an entity removes its aliases.
    #[test]
    fn deleting_entity_cascades_its_aliases() {
        let db = in_memory_db();
        let (a, b) = seed_entities(&db);

        db.with_conn(|conn| -> Result<(), DbError> {
            let aliases = EntityAliasDao::new(ConnectionOrTx::Connection(conn));
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            aliases.insert_or_ignore(a, "alpha")?;
            aliases.insert_or_ignore(b, "beta")?;

            assert!(entities.delete(a)?);
            assert!(
                aliases.aliases_of(a)?.is_empty(),
                "the deleted entity's aliases must cascade"
            );
            assert_eq!(
                aliases.aliases_of(b)?,
                vec!["beta".to_string()],
                "the other entity's aliases survive"
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    }
}
