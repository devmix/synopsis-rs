//! Serve-side bootstrap and startup checks (design D3/D4).
//!
//! [`bootstrap`] assembles the shared application state (config, domain
//! ontologies, databases, embedding provider) used by the `serve` and `sync`
//! subcommands; [`health`] runs the log-only startup health check; [`ingest`]
//! wraps the ingestion runner runs with one logging site per operation;
//! [`scheduler`] runs the periodic `orphan_cleanup` job (design D6).

pub mod bootstrap;
pub mod health;
pub mod ingest;
pub mod scheduler;
pub mod watcher;
