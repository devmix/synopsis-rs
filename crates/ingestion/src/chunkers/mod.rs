//! Document chunkers.
//!
//! Each format module implements [`crate::Chunker`] with its configuration
//! injected at construction time from the config crate. The source registry
//! (task 1.6) composes a parser with its chunker into a [`crate::Source`].

pub mod json;
pub mod markdown;
pub mod mediawiki;
