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
//! The NER layer (change `ingestion-ner`) builds on the chunking pipeline:
//! [`NerProvider`] implementations extract entities and facts from chunk
//! content — [`RegexNer`] applies the domain's prepared regex rules and
//! [`LlmNer`] renders the system/user prompts, calls a
//! chat-completions endpoint and caches responses in the lazily-created
//! `llm_ner_cache` table — and [`CompositeNer`] runs the configured stages
//! in order, enriches the results and applies the per-domain
//! `auto_publish_threshold` filter (design D7). Extracted entities are then
//! deduplicated persistently by the [`Resolver`] over the `entities` table
//! (Jaro-Winkler blocking index, design D9).
//!
//! The pipeline layer (change `ingestion-pipeline`) turns the parsed and
//! chunked documents into the knowledge base: the [`Ingester`] runs the
//! per-document pipeline (content-hash dedup → chunk → batched embeddings →
//! NER → one SQLite transaction → post-commit vector writes) and the
//! [`Runner`] orchestrates the configured sources — multi-source
//! `ingest_all`, incremental sync, deleted-file pruning, orphan cleanup and
//! cross-domain entity linking. Run statistics are reported as
//! [`ProgressStats`] per source and [`SummaryStats`] per run; an orphan
//! sweep reports [`OrphanCleanupStats`]. Vector writes go through the narrow
//! [`VectorSink`] seam (blanket-implemented over every vectors engine) so
//! tests can record or fail writes without an index engine.
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
//! carries no NER results — the NER layer attaches them through its own
//! structure keyed by chunk index, keeping the chunk a pure chunking
//! artifact.
//!
//! Every public module item is re-exported at the crate root
//! (`ingestion::MarkdownSource`, `ingestion::Registry`, …), so downstream
//! crates only need the root namespace.

pub mod chunkers;
pub mod entities;
pub mod error;
pub mod ingester;
pub mod ner;
pub mod parsers;
pub mod progress;
pub mod runner;
pub mod sources;
pub mod types;

pub use chunkers::json::JsonChunker;
pub use chunkers::markdown::MarkdownChunker;
pub use chunkers::mediawiki::MediawikiChunker;
pub use entities::{
    ResolvedEntity, Resolver, bigrams, canonical_proto, cluster_batch, jaro_winkler,
    normalize_name, scope_entity_metadata,
};
pub use error::IngestionError;
pub use ingester::{Ingester, VectorSink};
pub use ner::{
    CompositeNer, LlmNer, LlmNerCache, NerEntity, NerFact, NerPrompts, NerProvider, NerResult,
    RegexNer, TemplateHashes, build_cache_key, generate_json_schema, load_ner_prompts,
    parse_llm_response,
};
pub use parsers::json::JsonParser;
pub use parsers::markdown::MarkdownParser;
pub use parsers::mediawiki::MediawikiParser;
pub use parsers::unstructured::UnstructuredParser;
pub use parsers::webpage::WebpageParser;
pub use progress::{ProgressStats, ProgressTracker};
pub use runner::{OrphanCleanupStats, Runner, RunnerParams, SummaryStats};
pub use sources::{
    JsonSource, MarkdownSource, MediawikiSource, Registry, UnstructuredSource, WebpageSource,
};
pub use types::{Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};

#[cfg(test)]
mod root_api {
    //! Compile-time check that the full public API is reachable from the
    //! crate root (task 1.11 acceptance criterion: all five Source
    //! implementations available from the root; task 2.9 extends the pin
    //! with the NER layer and the entity resolver; task 3.9 extends it with
    //! the pipeline layer).

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::{Map, Value};

    use super::*;

    #[test]
    fn root_namespace_exposes_the_full_api() {
        // The five Source composites (the task 1.11 acceptance criterion).
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

        // NER: all three providers implement the object-safe trait.
        fn assert_provider<T: NerProvider>() {}
        assert_provider::<RegexNer>();
        assert_provider::<LlmNer>();
        assert_provider::<CompositeNer>();

        // The NER result types and the cheap constructors.
        let _ner_entity: Option<NerEntity> = None;
        let _ner_fact: Option<NerFact> = None;
        let _ner_result: Option<NerResult> = None;
        let _regex_ner = RegexNer::new(&[]);
        let _composite = CompositeNer::new(Vec::new(), &[]);
        let _resolver = Resolver::new(0.8);
        let _resolved: Option<ResolvedEntity> = None;
        let _prompts: Option<NerPrompts> = None;
        let _hashes: Option<TemplateHashes> = None;
        let _cache: Option<LlmNerCache<'_>> = None;

        // The stage factory and the prompt loader (name reachability from
        // the root; their signatures are pinned by the call sites in the
        // pipeline crates).
        let _build = CompositeNer::build_from_stages;
        let _load_prompts = load_ner_prompts;

        // The pure NER helpers, exercised end to end from the root namespace.
        let key = build_cache_key("srv", "model", 0.5, 1024, "sys", "usr", "content");
        assert_eq!(key.len(), 64);
        let parsed = parse_llm_response("{}").expect("an empty response parses");
        assert!(parsed.entities.is_empty() && parsed.facts.is_empty());
        let domain = config::DomainConfig {
            name: "demo".to_owned(),
            version: "1".to_owned(),
            description: String::new(),
            entities: Vec::new(),
            relations: Vec::new(),
            extraction: Default::default(),
            confidence: Default::default(),
        };
        let _schema = generate_json_schema(&domain);

        // The entity-resolution primitives.
        let _normalized = normalize_name("  Foo   bar ");
        let _grams = bigrams("foobar");
        let _similarity = jaro_winkler("foo", "foobar");
        let entity = NerEntity {
            name: "Ivan Petrov".to_owned(),
            entity_type: "person".to_owned(),
            description: String::new(),
            confidence: 1.0,
            domain: String::new(),
            metadata: Map::new(),
        };
        let clusters = cluster_batch(std::slice::from_ref(&entity), 0.8);
        let canonical = canonical_proto(clusters.first().expect("one cluster"));
        assert_eq!(canonical.name, entity.name);
        let raw = Map::from_iter([("url".to_owned(), Value::String("https://x".to_owned()))]);
        let scoped = scope_entity_metadata(&entity.name, &raw);
        assert!(scoped.is_empty());

        // The pipeline layer (task 3.9): the per-document ingester, the
        // multi-source runner and their stat types, all from the root.
        let _ingester: Option<Ingester<'_>> = None;
        let _runner: Option<Runner<'_>> = None;
        let _params: Option<RunnerParams<'_>> = None;
        let _summary: Option<SummaryStats> = None;
        let _orphan: Option<OrphanCleanupStats> = None;
        // The vector write seam is object-safe (the runner holds
        // `&dyn VectorIndex` adapters, tests hold recording stubs).
        fn assert_sink<T: VectorSink + ?Sized>() {}
        assert_sink::<dyn VectorSink>();
    }
}
