//! `GET /health` (design D5): status, version, sync-state placeholder and
//! knowledge-base counters, on the same listener as the MCP transport.
//!
//! The Go oracle's payload (status/uptime/metrics/components,
//! `../synopsis/internal/mcp/server.go`) is deliberately NOT copied: design
//! D5 re-decides the structure for Rust — status, version, sync-state
//! placeholder and KB row counters via the db DAOs (the oracle's request
//! metrics and component map have no Rust counterpart in this design).

use axum::extract::State;
use axum::response::IntoResponse;
use axum::{Json, http::StatusCode};
use db::{
    ChunkDao, ConnectionOrTx, Db, DocumentDao, DocumentFilter, EntityDao, EntityFilter, FactDao,
};
use serde::Serialize;

/// `GET /health` handler state: the db handle + service version.
#[derive(Clone)]
pub struct HealthState {
    db: Db,
    version: String,
}

impl HealthState {
    /// Build the state for a server reporting `version`.
    pub fn new(db: Db, version: String) -> Self {
        Self { db, version }
    }
}

/// Knowledge-base row counters (design D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KbCounters {
    /// `documents` rows.
    pub documents: i64,
    /// `chunks` rows.
    pub chunks: i64,
    /// `entities` rows.
    pub entities: i64,
    /// `facts` rows.
    pub facts: i64,
}

/// `GET /health` response (design D5).
#[derive(Debug, Clone, Serialize)]
pub struct HealthStatus {
    /// `"ok"` when every counter was read, `"degraded"` otherwise.
    pub status: String,
    /// Service version (config `server.version`).
    pub version: String,
    /// Sync-state placeholder: no background sync exists in this change yet.
    pub sync_state: String,
    /// Knowledge-base row counters.
    pub counters: KbCounters,
}

/// Read the counters and build the status. Sync on purpose: the axum handler
/// runs it in `spawn_blocking` (the driver is synchronous).
pub fn check(db: &Db, version: &str) -> HealthStatus {
    match counts(db) {
        Ok(counters) => HealthStatus {
            status: "ok".to_owned(),
            version: version.to_owned(),
            sync_state: "idle".to_owned(),
            counters,
        },
        Err(_) => degraded(version),
    }
}

/// A degraded status with zero counters (the counter read failed).
fn degraded(version: &str) -> HealthStatus {
    HealthStatus {
        status: "degraded".to_owned(),
        version: version.to_owned(),
        sync_state: "idle".to_owned(),
        counters: KbCounters {
            documents: 0,
            chunks: 0,
            entities: 0,
            facts: 0,
        },
    }
}

fn counts(db: &Db) -> Result<KbCounters, db::DbError> {
    db.with_conn(|conn| -> Result<KbCounters, db::DbError> {
        let exec = ConnectionOrTx::Connection(conn);
        Ok(KbCounters {
            documents: DocumentDao::new(exec).count(&DocumentFilter::default())?,
            chunks: ChunkDao::new(exec).count()?,
            entities: EntityDao::new(exec).count(&EntityFilter::default())?,
            facts: FactDao::new(exec).count()?,
        })
    })?
}

/// `GET /health` axum handler: always 200 with a status field (the oracle
/// never 500s on health; a failed counter read degrades the status).
pub async fn handler(State(state): State<HealthState>) -> impl IntoResponse {
    let db = state.db.clone();
    let version = state.version.clone();
    let status = match tokio::task::spawn_blocking(move || check(&db, &version)).await {
        Ok(status) => status,
        // The worker task died (cancellation): `version` moved into the
        // closure, so rebuild the degraded status from the handler state.
        Err(_) => degraded(&state.version),
    };
    (StatusCode::OK, Json(status))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use db::test_util;

    /// A seeded in-memory KB: 1 document, 2 chunks, 3 entities, 4 facts.
    fn seeded_db() -> Db {
        let db = test_util::in_memory_db();
        let seeded = db.with_conn(|conn| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Connection(conn);
            let doc = DocumentDao::new(exec).create(
                "markdown",
                "/docs/hr.md",
                Some(r#"{"domain": "hr"}"#),
                None,
            )?;
            let chunks = ChunkDao::new(exec);
            chunks.create(doc, "chunk one", 0, None, None)?;
            chunks.create(doc, "chunk two", 1, None, None)?;
            let entities = EntityDao::new(exec);
            for name in ["Alice", "Bob", "Acme"] {
                entities.create("employee", name, "hr", None, None, None)?;
            }
            let facts = FactDao::new(exec);
            for i in 0..4 {
                facts.create(
                    None,
                    &format!("predicate_{i}"),
                    None,
                    "hr",
                    None,
                    None,
                    None,
                )?;
            }
            Ok(())
        });
        seeded.expect("pool checkout").expect("seed rows");
        db
    }

    #[test]
    fn health_ok_on_seeded_db() {
        let status = check(&seeded_db(), "test-version");
        assert_eq!(status.status, "ok");
        assert_eq!(status.version, "test-version");
        assert_eq!(status.sync_state, "idle");
        assert_eq!(
            status.counters,
            KbCounters {
                documents: 1,
                chunks: 2,
                entities: 3,
                facts: 4
            }
        );
    }

    #[test]
    fn health_json_shape_keys_and_types() {
        let status = check(&seeded_db(), "test-version");
        let json = serde_json::to_value(&status).unwrap();
        // Exactly the four frozen top-level keys (design D5).
        assert_eq!(json.as_object().unwrap().len(), 4);
        assert!(json["status"].is_string());
        assert!(json["version"].is_string());
        assert!(json["sync_state"].is_string());
        for key in ["documents", "chunks", "entities", "facts"] {
            assert!(json["counters"][key].is_i64(), "{key} must be an integer");
        }
    }
}
