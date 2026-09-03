//! Unified command surface for DAOs: the same method runs over a plain
//! connection or an in-flight transaction.
//!
//! Per design decision D2 the unified handle is an
//! enum (`ConnectionOrTx`) rather than a trait object — dispatch is cheap and
//! type-safe; the trait itself is sealed so only the executor types below can
//! implement it.
//!
//! `rusqlite::Transaction<'conn>` has no inherent query methods of its own —
//! it derefs to `Connection` — so all three impls delegate to the same
//! connection-backed helpers (DRY).

use rusqlite::{Connection, Params, Row, Transaction};
use std::ops::Deref;

use crate::error::DbError;

mod sealed {
    //! Seals [`super::DbExecutor`]: only types inside this crate may implement it.
    use rusqlite::{Connection, Transaction};

    pub trait Sealed {}

    impl Sealed for Connection {}
    impl Sealed for Transaction<'_> {}
    impl Sealed for super::ConnectionOrTx<'_> {}
}

/// The minimal command surface a DAO needs, satisfied by [`Connection`],
/// [`Transaction`] and [`ConnectionOrTx`] (oracle analogue: `DBTX`).
///
/// All methods take `&self`: both rusqlite types run their commands behind a
/// shared connection lock, so no exclusive borrow is required.
pub trait DbExecutor: sealed::Sealed {
    /// Execute one SQL statement (INSERT/UPDATE/DELETE/DDL); returns the
    /// number of rows changed.
    fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize, DbError>;

    /// Run a statement and collect the rows mapped by `f` (DAO queries in this
    /// project are always paged/small; materializing is the KISS choice and
    /// keeps [`rusqlite::Statement`] lifetimes private).
    fn query<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>, DbError>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T, rusqlite::Error>;

    /// Run a statement that returns exactly one row and map it with `f`.
    fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, DbError>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T, rusqlite::Error>;
}

/// A non-owning handle to either the pooled connection or an in-flight
/// transaction, so DAO methods can accept one type in both cases
/// (oracle analogue: `DBTX`).
#[derive(Debug, Clone, Copy)]
pub enum ConnectionOrTx<'a> {
    /// the pooled connection, used outside a transaction.
    Connection(&'a Connection),
    /// An active transaction started by [`Db::exec_tx`](crate::Db::exec_tx).
    Transaction(&'a Transaction<'a>),
}

/// Execute one statement on a connection; returns rows changed.
fn execute_on(conn: &Connection, sql: &str, params: impl Params) -> Result<usize, DbError> {
    conn.execute(sql, params).map_err(DbError::from)
}

/// Run a statement, collect rows mapped by `f`.
fn query_on<T, P, F>(conn: &Connection, sql: &str, params: P, f: F) -> Result<Vec<T>, DbError>
where
    P: Params,
    F: FnMut(&Row<'_>) -> Result<T, rusqlite::Error>,
{
    let mut stmt = conn.prepare(sql).map_err(DbError::from)?;
    let rows = stmt.query_map(params, f).map_err(DbError::from)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(DbError::from)?);
    }
    Ok(out)
}

/// Run a single-row statement and map it.
fn query_row_on<T, P, F>(conn: &Connection, sql: &str, params: P, f: F) -> Result<T, DbError>
where
    P: Params,
    F: FnOnce(&Row<'_>) -> Result<T, rusqlite::Error>,
{
    conn.query_row(sql, params, f).map_err(DbError::from)
}

impl DbExecutor for Connection {
    fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize, DbError> {
        execute_on(self, sql, params)
    }

    fn query<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>, DbError>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        query_on(self, sql, params, f)
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, DbError>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        query_row_on(self, sql, params, f)
    }
}

impl DbExecutor for Transaction<'_> {
    fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize, DbError> {
        execute_on(self.deref(), sql, params)
    }

    fn query<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>, DbError>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        query_on(self.deref(), sql, params, f)
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, DbError>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        query_row_on(self.deref(), sql, params, f)
    }
}

impl DbExecutor for ConnectionOrTx<'_> {
    fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize, DbError> {
        match *self {
            Self::Connection(c) => execute_on(c, sql, params),
            Self::Transaction(t) => execute_on(t.deref(), sql, params),
        }
    }

    fn query<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>, DbError>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        match *self {
            Self::Connection(c) => query_on(c, sql, params, f),
            Self::Transaction(t) => query_on(t.deref(), sql, params, f),
        }
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, DbError>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T, rusqlite::Error>,
    {
        match *self {
            Self::Connection(c) => query_row_on(c, sql, params, f),
            Self::Transaction(t) => query_row_on(t.deref(), sql, params, f),
        }
    }
}
