//! Error type for the `db` crate.
//!
//! Library error per workspace convention: `thiserror` with one variant per
//! failure class. Conversion from the upstream drivers is provided via
//! `From`, so DAO code can use `?` freely.

use std::path::PathBuf;

use thiserror::Error;

/// All error conditions surfaced by this crate.
#[derive(Debug, Error)]
pub enum DbError {
    /// A filesystem operation failed (e.g. creating the database's parent
    /// directory before `sqlite3_open`).
    #[error("io error at path {path}: {source}")]
    Io {
        /// The path the operation targeted.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A SQLite operation failed.
    #[error("sqlite error: {source}")]
    Sqlite {
        /// The underlying rusqlite error.
        #[source]
        source: rusqlite::Error,
    },
    /// Loading or applying the embedded migrations failed.
    #[error("migration error: {source}")]
    Migration {
        /// The underlying rusqlite_migration error.
        #[source]
        source: rusqlite_migration::Error,
    },
    /// A nested `Db::exec_tx` was attempted on the same thread (design D10).
    ///
    /// With a connection pool a nested call would silently start an
    /// INDEPENDENT transaction on another connection — a semantic trap worse
    /// than a deadlock — so it is rejected explicitly instead.
    #[error("nested transaction: this thread is already inside Db::exec_tx")]
    NestedTransaction,
    /// Checking a connection out of the pool timed out (design D12). The
    /// per-connection SQLite `busy_timeout` (5000 ms, D8) is a separate,
    /// lower-level mechanism and surfaces as [`DbError::Sqlite`].
    #[error("pool checkout timed out: {source}")]
    PoolTimeout {
        /// The underlying r2d2 error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl From<rusqlite::Error> for DbError {
    fn from(source: rusqlite::Error) -> Self {
        DbError::Sqlite { source }
    }
}

impl From<rusqlite_migration::Error> for DbError {
    fn from(source: rusqlite_migration::Error) -> Self {
        DbError::Migration { source }
    }
}

impl From<r2d2::Error> for DbError {
    fn from(source: r2d2::Error) -> Self {
        // r2d2 0.8's `Error` is a struct carrying the optional underlying
        // cause as a message; in practice `Pool::get` only fails on a
        // checkout timeout.
        DbError::PoolTimeout {
            source: Box::new(source),
        }
    }
}
