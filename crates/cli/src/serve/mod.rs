//! Serve-side bootstrap and startup checks (design D3/D4).
//!
//! [`bootstrap`] assembles the shared application state (config, domain
//! ontologies, databases, embedding provider) used by the `serve` and `sync`
//! subcommands; [`health`] runs the log-only startup health check.

pub mod bootstrap;
pub mod health;
