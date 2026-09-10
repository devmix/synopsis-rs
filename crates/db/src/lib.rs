//! SQLite storage layer: connection, migrations, transactions, DAOs and FTS5
//! queries.
//!
//! Design: a `r2d2` connection pool over the sync SQLite driver
//! (db-module D1, re-decided 2026-08-20: read-heavy workload — WAL +
//! concurrent readers, a write transaction never blocks readers), native
//! transaction semantics (D2), one squashed v5 init migration with
//! `PRAGMA user_version` as the sole schema-state authority (D3, ADR 0001),
//! and the D8 PRAGMAs applied to every pooled connection.
//!
//! Entry points:
//! - [`Db::open`] — open/create the database, apply the D8 PRAGMAs to every
//!   pooled connection and run the embedded migrations once;
//! - [`Db::with_conn`] — checkout + closure: the way reads and single
//!   writes reach the database (concurrent under WAL);
//! - [`Db::exec_tx`] — closure transactions: commit on `Ok`, rollback on
//!   `Err` or panic; nested on the same thread →
//!   `DbError::NestedTransaction`;
//! - [`DbExecutor`] — the command surface DAOs use over a connection or a
//!   transaction (`ConnectionOrTx` unifies both);
//! - [`DbError`] — the crate's error type.
//!
//! Test support: [`test_util`] (in-memory and read-only fixture databases).

pub mod app_kv;
pub mod chunk;
pub mod chunk_entity;
pub mod connection;
pub mod document;
pub mod entity;
pub mod entity_alias;
pub mod entity_link;
pub mod entity_source;
pub mod error;
pub mod executor;
pub mod fact;
pub mod fact_source;
pub mod gc;
pub mod queue_task;
pub mod test_util;
pub mod utils;

pub use app_kv::AppKv;
pub use chunk::{Chunk, ChunkDao, FtsHit};
pub use chunk_entity::ChunkEntityDao;
pub use connection::Db;
pub use document::{Document, DocumentDao, DocumentFilter};
pub use entity::{Entity, EntityDao, EntityFilter};
pub use entity_alias::EntityAliasDao;
pub use entity_link::{EntityLink, EntityLinkDao};
pub use entity_source::EntitySourceDao;
pub use error::DbError;
pub use executor::{ConnectionOrTx, DbExecutor};
pub use fact::{Fact, FactDao, FactFilter};
pub use fact_source::{FactSource, FactSourceDao};
pub use gc::GcDao;
pub use queue_task::{
    DocDeletePayload, DocIndexPayload, EntityLinkPayload, QueueTask, QueueTaskDao, QueueTaskError,
    QueueTaskType, ReIndexOp, UnknownTaskType,
};
