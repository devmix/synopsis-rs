//! Key-value storage over the `app_kv` table.
//!
//! Oracle mapping: `../synopsis/internal/database/dao/app_kv.go` (Get/Set
//! semantics), re-architected per the 2026-08-19 migration principles.
//!
//! The table holds small pieces of application state (e.g.
//! `last_linking_run`). `set` is an upsert that refreshes `updated_at` on
//! every write; `get` returns `None` for a missing key.
//!
//! The table belongs to the CACHE database schema
//! (`migrations/cache/1-init/up.sql`, task 1.9, storage-layout-restructure);
//! on a database that does not have it yet (e.g. a knowledge database still
//! used for the pre-1.10 linker decision cache) it is created lazily at
//! runtime on first use — the same pattern as
//! `ingestion::ner::llm_cache::LlmNerCache`.
//!
//! **Conscious deviation from the oracle:** the Go `Get` swallows driver
//! errors and reports "missing key" (best-effort semantics); the Rust `get`
//! propagates them as [`DbError`]. A broken database must not look empty —
//! silently reporting "no value" would mask real failures in a process that
//! runs unattended.

use crate::error::DbError;
use crate::executor::{ConnectionOrTx, DbExecutor};

/// Key-value storage over the `app_kv` table.
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction (design D2) via [`ConnectionOrTx`] — the Rust
/// analogue of the oracle's `NewAppKV(db DBTX)`. The instance borrows from
/// the handle it is given, so the handle must outlive it.
///
/// # Examples
///
/// ```no_run
/// # use db::{AppKv, ConnectionOrTx, Db, DbError};
/// # fn example(db: &Db) -> Result<(), DbError> {
/// let value = db.with_conn(|conn| {
///     let kv = AppKv::new(ConnectionOrTx::Connection(conn));
///     kv.set("last_linking_run", "2024-01-15T12:00:00Z")?;
///     kv.get("last_linking_run")
/// })??;
/// assert_eq!(value, Some("2024-01-15T12:00:00Z".to_string()));
/// # Ok(())
/// # }
/// ```
pub struct AppKv<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> AppKv<'conn> {
    /// Bind the DAO to a shared connection or an in-flight transaction.
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Return the value stored under `key`, or `None` if the key is absent.
    ///
    /// A stored `NULL` value reads back as an empty string, matching the
    /// oracle (`sql.NullString` zero value).
    pub fn get(&self, key: &str) -> Result<Option<String>, DbError> {
        self.ensure_table()?;
        let value = self
            .exec
            .query_row("SELECT value FROM app_kv WHERE key = ?", [key], |row| {
                row.get::<_, Option<String>>(0)
                    .map(|v| v.unwrap_or_default())
            });
        match value {
            Ok(value) => Ok(Some(value)),
            Err(err) if is_no_rows(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Store `value` under `key`, creating the row or overwriting an
    /// existing one, and refresh `updated_at` (oracle upsert semantics).
    pub fn set(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.ensure_table()?;
        self.exec.execute(
            "INSERT INTO app_kv (key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
            [key, value],
        )?;
        Ok(())
    }

    /// Create the key-value table if it does not exist yet (task 1.9):
    /// `app_kv` belongs to the cache-database schema, so a database without
    /// it (e.g. a knowledge database) gets the table lazily at runtime on
    /// first use — the same pattern as `LlmNerCache::ensure_table`.
    fn ensure_table(&self) -> Result<(), DbError> {
        self.exec.execute(
            "CREATE TABLE IF NOT EXISTS app_kv \
             (key TEXT PRIMARY KEY, value TEXT, \
              updated_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            [],
        )?;
        Ok(())
    }
}

/// Whether `err` wraps the "no rows" condition of a single-row query.
fn is_no_rows(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Sqlite { source } if *source == rusqlite::Error::QueryReturnedNoRows
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::Db;
    use crate::test_util::in_memory_db;

    /// Run `f` with a non-transactional DAO bound to a pooled connection
    /// (checked out for the duration of the closure).
    fn with_kv<T>(db: &Db, f: impl FnOnce(&AppKv<'_>) -> T) -> T {
        db.with_conn(|conn| f(&AppKv::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    // (a) set + get round-trip.
    #[test]
    fn set_then_get_round_trip() {
        let db = in_memory_db();
        with_kv(&db, |kv| {
            kv.set("test_key", "test_value").unwrap();
            assert_eq!(kv.get("test_key").unwrap(), Some("test_value".to_string()));
        });
    }

    // (b) get a missing key → None.
    #[test]
    fn get_missing_key_is_none() {
        let db = in_memory_db();
        with_kv(&db, |kv| {
            assert_eq!(kv.get("nonexistent_key").unwrap(), None);
        });
    }

    // (c) set overwrites an existing value.
    #[test]
    fn set_overwrites_existing_value() {
        let db = in_memory_db();
        with_kv(&db, |kv| {
            kv.set("update_key", "initial").unwrap();
            kv.set("update_key", "updated").unwrap();
            assert_eq!(kv.get("update_key").unwrap(), Some("updated".to_string()));
        });
    }

    // An empty value is a stored value, not a missing key (oracle EmptyValue).
    #[test]
    fn set_empty_value_reads_back_as_empty_string() {
        let db = in_memory_db();
        with_kv(&db, |kv| {
            kv.set("empty_val_key", "").unwrap();
            assert_eq!(kv.get("empty_val_key").unwrap(), Some(String::new()));
        });
    }

    // (d1) set inside exec_tx commits on success.
    #[test]
    fn set_inside_transaction_commits_on_success() {
        let db = in_memory_db();
        db.exec_tx(|tx| {
            let kv = AppKv::new(ConnectionOrTx::Transaction(&*tx));
            kv.set("tx_key", "tx_value")
        })
        .expect("commit");
        with_kv(&db, |kv| {
            assert_eq!(kv.get("tx_key").unwrap(), Some("tx_value".to_string()));
        });
    }

    // (d2) set inside exec_tx rolls back when the closure errors.
    #[test]
    fn set_inside_transaction_rolls_back_on_error() {
        let db = in_memory_db();
        let err = db
            .exec_tx(|tx| -> Result<(), DbError> {
                let kv = AppKv::new(ConnectionOrTx::Transaction(&*tx));
                kv.set("tx_key", "tx_value")?;
                // A genuine SQL failure after a partial write (CHECK violation).
                tx.execute(
                    "INSERT INTO facts (predicate, status) VALUES ('p', 'bogus')",
                    [],
                )?;
                Ok(())
            })
            .expect_err("closure error must surface");
        assert!(matches!(err, DbError::Sqlite { .. }));
        with_kv(&db, |kv| {
            assert_eq!(
                kv.get("tx_key").unwrap(),
                None,
                "rolled-back set must not be visible"
            );
        });
    }
}
