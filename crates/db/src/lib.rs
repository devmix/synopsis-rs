//! SQLite storage layer: connection, migrations, transactions, DAOs and FTS5
//! queries.
//!
//! Oracle mapping: `../synopsis/internal/database` (design.md D1). The module
//! is re-architected for Rust per the migration principles of 2026-08-19: a
//! single shared connection behind `Arc<Mutex>` (db-module D1), native
//! transaction semantics (D2), one squashed v5 init migration with
//! `PRAGMA user_version` as the sole schema-state authority (D3, ADR 0001),
//! and D8 PRAGMA parity with the Go oracle.
//!
//! Entry points:
//! - [`Db::open`] — open/create the database, apply the D8 PRAGMAs and run
//!   the embedded migrations;
//! - [`Db::exec_tx`] — closure transactions: commit on `Ok`, rollback on
//!   `Err` or panic;
//! - [`DbExecutor`] — the command surface DAOs use over a connection or a
//!   transaction (`ConnectionOrTx` unifies both);
//! - [`DbError`] — the crate's error type.
//!
//! Test support: [`test_util`] (in-memory and read-only fixture databases).

pub mod app_kv;
pub mod connection;
pub mod document;
pub mod error;
pub mod executor;
pub mod test_util;
pub mod utils;

pub use app_kv::AppKv;
pub use connection::Db;
pub use document::{Document, DocumentDao, DocumentFilter};
pub use error::DbError;
pub use executor::{ConnectionOrTx, DbExecutor};

// DAO modules are re-exported at the crate root as their db-module tasks land
// (`app_kv` and `document` now; the chunk/entity/fact/relation DAOs follow).
