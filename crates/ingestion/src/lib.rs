//! Document parsing and chunking for the Synopsis ingestion pipeline.
//!
//! This crate is the Rust re-architecture of the Go oracle's
//! `internal/ingestion` package (design D1): parsers walk a source tree and
//! extract [`Document`]s, chunkers split document content into
//! [`DocumentChunk`]s, and a [`Source`] is the self-sufficient ingestion unit
//! combining one parser with its chunker.
//!
//! Core contracts (oracle `types.go`, `chunkers/chunker.go`,
//! `sources/source.go`):
//!
//! - Parsing is best-effort: per-file failures are collected in
//!   [`ParseResult::errors`] and never abort the walk.
//! - [`DocumentChunk`] offsets are byte offsets into the original content and
//!   the chunk text is a pure slice of it (`content[start..end] == text`).
//! - [`Parser`], [`Chunker`] and [`Source`] are object-safe; the source
//!   registry (task 1.6) stores implementations as `Box<dyn Source>`.
//!
//! Deliberate deviation from the oracle (design D2): [`DocumentChunk`]
//! carries no NER results — the NER stage (series change 2) attaches them
//! through its own structure keyed by chunk index, keeping the chunk a pure
//! chunking artifact.

pub mod error;
pub mod types;

pub use error::IngestionError;
pub use types::{Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};
