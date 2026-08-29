//! Serve-side bootstrap and startup checks (design D3/D4).
//!
//! [`bootstrap`] assembles the shared application state (config, domain
//! ontologies, databases, embedding provider) used by the `serve` and `sync`
//! subcommands; [`health`] runs the log-only startup health check; [`ingest`]
//! wraps the ingestion runner runs with one logging site per operation;
//! [`server`] mounts the MCP router and drives the owner-thread serve loop
//! with graceful shutdown (design D4/D7).

pub mod bootstrap;
pub mod health;
pub mod ingest;
pub mod server;
pub mod watcher;
