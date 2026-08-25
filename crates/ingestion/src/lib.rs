//! Document parsing and chunking for the Synopsis ingestion pipeline.
//!
//! This crate is the Rust re-architecture of the Go oracle's
//! `internal/ingestion` package (design D1): parsers walk a source tree and
//! extract [`Document`]s, chunkers split document content into
//! [`DocumentChunk`]s, and a [`Source`] is the self-sufficient ingestion unit
//! combining one parser with its chunker.
//!
//! All five `global.xml` source formats are implemented as [`Source`]
//! composites — [`MarkdownSource`], [`JsonSource`], [`MediawikiSource`],
//! [`WebpageSource`] and [`UnstructuredSource`] — and the [`Registry`] is the
//! pipeline's entry point: it maps the `type` attribute word of a `<source>`
//! element to its implementation (design D5).
//!
//! Core contracts (oracle `types.go`, `chunkers/chunker.go`,
//! `sources/source.go`):
//!
//! - Parsing is best-effort: per-file failures are collected in
//!   [`ParseResult::errors`] and never abort the walk.
//! - [`DocumentChunk`] offsets are byte offsets into the original content and
//!   the chunk text is a pure slice of it (`content[start..end] == text`).
//! - [`Parser`], [`Chunker`] and [`Source`] are object-safe; the
//!   [`Registry`] stores implementations as `Box<dyn Source>`.
//! - User `.synignore` files (gitignore semantics) are the single exclusion
//!   mechanism for source walks; there is no built-in skip list.
//!
//! Deliberate deviation from the oracle (design D2): [`DocumentChunk`]
//! carries no NER results — the NER stage (series change 2) attaches them
//! through its own structure keyed by chunk index, keeping the chunk a pure
//! chunking artifact.
//!
//! Every public module item is re-exported at the crate root
//! (`ingestion::MarkdownSource`, `ingestion::Registry`, …), so downstream
//! crates only need the root namespace.

pub mod chunkers;
pub mod entities;
pub mod error;
pub mod ner;
pub mod parsers;
pub mod sources;
pub mod types;

pub use chunkers::json::JsonChunker;
pub use chunkers::markdown::MarkdownChunker;
pub use chunkers::mediawiki::MediawikiChunker;
pub use error::IngestionError;
pub use parsers::json::JsonParser;
pub use parsers::markdown::MarkdownParser;
pub use parsers::mediawiki::MediawikiParser;
pub use parsers::unstructured::UnstructuredParser;
pub use parsers::webpage::WebpageParser;
pub use sources::{
    JsonSource, MarkdownSource, MediawikiSource, Registry, UnstructuredSource, WebpageSource,
};
pub use types::{Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};

#[cfg(test)]
mod root_api {
    //! Compile-time check that the full public API is reachable from the
    //! crate root (task 1.11 acceptance criterion: all five Source
    //! implementations available from the root).

    use super::*;

    #[test]
    fn root_namespace_exposes_the_full_api() {
        // The five Source composites (the acceptance criterion).
        fn assert_source<T: Source>(_: Option<T>) {}
        assert_source::<MarkdownSource>(None);
        assert_source::<JsonSource>(None);
        assert_source::<MediawikiSource>(None);
        assert_source::<WebpageSource>(None);
        assert_source::<UnstructuredSource>(None);

        // The registry, core types and the error type.
        let _registry = Registry::new();
        let _document: Option<Document> = None;
        let _chunk: Option<DocumentChunk> = None;
        let _metadata: Option<DocumentMetadata> = None;
        let _result: Option<ParseResult> = None;
        let _error: Option<IngestionError> = None;

        // The format parsers (stateless unit structs).
        let _parsers = (
            MarkdownParser,
            JsonParser,
            MediawikiParser,
            WebpageParser,
            UnstructuredParser,
        );

        // The chunkers need a config to construct; a trait check suffices.
        fn assert_chunker<T: Chunker>() {}
        assert_chunker::<MarkdownChunker>();
        assert_chunker::<JsonChunker>();
        assert_chunker::<MediawikiChunker>();
    }
}
