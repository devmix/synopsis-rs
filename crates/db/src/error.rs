//! Error type for the `db` crate.
//!
//! Library error per workspace convention: `thiserror` with one variant per
//! failure class. Conversion from the two upstream drivers is provided via
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
    /// The shared connection's mutex was poisoned because a previous holder
    /// panicked while holding it (design D1).
    #[error("connection mutex poisoned: a previous holder panicked while holding it")]
    Poisoned,
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
