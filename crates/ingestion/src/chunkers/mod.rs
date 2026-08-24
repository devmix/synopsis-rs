//! Document chunkers (oracle: `internal/ingestion/chunkers/`).
//!
//! Each format module implements [`crate::Chunker`] with its configuration
//! injected at construction time from the config crate (the oracle read the
//! same knobs from `internal/config`). The source registry (task 1.6) composes
//! a parser with its chunker into a [`crate::Source`].

pub mod json;
pub mod markdown;
