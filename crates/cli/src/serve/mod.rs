//! Serve-side bootstrap and startup checks (design D3/D4).
//!
//! [`bootstrap`] assembles the shared application state (config, domain
//! ontologies, databases, embedding provider) used by the `serve` subcommand;
//! [`health`] runs the log-only startup health check; [`server`] mounts the
//! MCP router and drives the owner-thread serve loop with graceful shutdown
//! (design D4/D7).

pub mod bootstrap;
pub mod health;
pub mod server;
pub mod watcher;
