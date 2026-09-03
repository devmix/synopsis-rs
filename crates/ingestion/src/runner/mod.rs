//! Multi-source ingestion runner (pipeline task 3.7, design D3/D4).
//!
//! Design D3: the [`Runner`] holds no globals — every dependency (db,
//! configs, registry, providers) is a reference passed to [`Runner::new`]
//! via [`RunnerParams`].
//!
//! Responsibilities (queue-only model, remove-direct-ingest task 1.4):
//!
//! - [`Runner::process_document_by_path`] — the worker's `index` op: the
//!   per-document pipeline for a single file (no source-tree walk), with
//!   per-source NER provider assembly and domain enrichment.
//! - [`Runner::delete_document_at`] — the worker's `delete` op (full
//!   per-document cleanup in one transaction).
//! - [`Runner::cleanup_orphaned_data`] / [`Runner::build_entity_links`] —
//!   post-batch maintenance (orphan sweep + cross-domain entity linking,
//!   `runner/cleanup.rs`); the worker's GC phase drives the sweep and the
//!   linking entry point is called directly.
//! - [`Runner::find_source_for_path`] / [`Runner::belongs_to_source`] —
//!   source containment used by the queue producer, the worker and the
//!   cleanup stage.
//!
//! The `document_jobs` queue is the only processing path:
//! [`DocumentJobQueue`](crate::job_queue::DocumentJobQueue) (producer)
//! enqueues per-file jobs and
//! [`DocumentWorker`](crate::worker::DocumentWorker) (consumer) claims them
//! and drives [`Runner::process_document_by_path`] /
//! [`Runner::delete_document_at`].
//!
//! All mutating entry points are serialized by an internal mutex: the
//! worker and a CLI operation must never write SQLite concurrently
//! (design D4).
//!
//! Design decisions:
//!
//! - Domain enrichment wraps the fused [`Source`] (parser + chunker) rather
//!   than a standalone parser interface: the [`Ingester`] parses through one
//!   `Source` trait (design D2), so the wrapper stamps
//!   [`DOMAIN_METADATA_KEY`] on parsed documents (see the private
//!   `DomainEnrichedSource` below).
//! - Path containment is component-aware (the private `is_within` helper):
//!   a raw string prefix check would treat `/data/docs2/file.md` as inside
//!   `/data/docs`.
//! - [`detect_source_type`] uses `contains("wiki")`, which also matches
//!   every mediawiki name (a separate `contains("mediawiki")` check would be
//!   dead code).
//! - Warnings use `eprintln!` (crate convention; no logger in the frozen
//!   stack).

pub mod cleanup;

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use config::DomainConfig;
use config::ontology::{GlobalConfig, SourceConfig, SourceType};
use config::preset::{IngestionConfig, LinkerConfig};
use db::{ConnectionOrTx, Db, DocumentDao, GcDao};
use embedding::EmbeddingProvider;
use serde_json::Value;
use vectors::VectorIndex;

pub use cleanup::OrphanCleanupStats;

use crate::entities::Resolver;
use crate::error::IngestionError;
use crate::ingester::{Ingester, VectorSink};
use crate::ner::{CompositeNer, NerPrompts, NerProvider};
use crate::progress::ProgressTracker;
use crate::sources::Registry;
use crate::types::{
    Chunker, Document, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source,
};

/// The `metadata.extra` key carrying the source's domain list (the
/// [`DomainEnrichedSource`] wrapper stamps it on every parsed document).
pub const DOMAIN_METADATA_KEY: &str = "domain";

/// Aggregated outcome of a multi-source run.
///
/// Per-source progress is the usual [`crate::ProgressStats`]; this struct
/// adds the cross-source bookkeeping: how many sources completed and the
/// per-source error list (a failing source never aborts the run).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryStats {
    /// Sources whose ingestion completed without a source-level error.
    pub sources_processed: usize,
    /// Documents inserted as new across all sources.
    pub documents_created: u64,
    /// Existing documents updated across all sources.
    pub documents_updated: u64,
    /// Documents skipped (unchanged content hash) across all sources.
    pub documents_skipped: u64,
    /// One entry per source-level failure (`"<source path>: <error>"`).
    pub errors: Vec<String>,
}

/// Collaborators for [`Runner::new`] (design D3: dependency injection).
pub struct RunnerParams<'a> {
    /// Shared database handle.
    pub db: &'a Db,
    /// Per-preset ingestion config (chunking, NER toggles, resolver, batching).
    pub ingest_cfg: &'a IngestionConfig,
    /// Global ontology config (sources, NER stage list); `None` when the
    /// pipeline runs without a `global.xml` (no configured sources).
    pub global: Option<&'a GlobalConfig>,
    /// Domain configs by domain name (resolved by the CLI from the
    /// `<domain>` references); NER assembly looks names up here.
    pub domains: &'a HashMap<String, DomainConfig>,
    /// Source-type registry (task 1.6): maps the `<source type>` word to its
    /// implementation.
    pub registry: &'a Registry,
    /// Embedding provider for chunk vectors.
    pub embed: &'a dyn EmbeddingProvider,
    /// Vector index engine (design D5: post-commit writes through the
    /// [`VectorSink`] blanket impl, plus the orphan reconciliation of
    /// `cleanup_orphaned_data`, task 3.8).
    pub vectors: &'a dyn VectorIndex,
    /// NER prompt templates (loaded once by the CLI, task 2.2).
    pub prompts: &'a NerPrompts,
    /// Cross-domain linker config (the root preset's `linker` section;
    /// task 3.8: the `llm` linking method honors `disabled`).
    pub linker_cfg: &'a LinkerConfig,
    /// Prompt template directory (the root preset's `paths.prompts_path`;
    /// task 3.8: the `llm` linking method loads
    /// `{prompts_path}/entity-linker/`, embedded defaults when absent).
    pub prompts_path: &'a str,
    /// LLM-NER cache database (a separate handle from the main database);
    /// `None` disables response caching.
    pub llm_cache: Option<Db>,
}

/// Multi-source ingestion orchestrator (design D3/D4).
///
/// Holds references to every collaborator plus the source index built from
/// the configured sources (absolute normalized path → config index) and the
/// enabled roots. All mutating entry points take an internal mutex.
pub struct Runner<'a> {
    db: &'a Db,
    ingest_cfg: &'a IngestionConfig,
    global: Option<&'a GlobalConfig>,
    domains: &'a HashMap<String, DomainConfig>,
    registry: &'a Registry,
    embed: &'a dyn EmbeddingProvider,
    vectors: &'a dyn VectorIndex,
    prompts: &'a NerPrompts,
    linker_cfg: &'a LinkerConfig,
    prompts_path: &'a str,
    llm_cache: Option<Db>,

    /// Configured sources keyed by absolute (lexically normalized) path;
    /// includes disabled sources (path lookup does not filter by enabled
    /// state).
    source_index: BTreeMap<String, usize>,
    /// Absolute paths of the non-disabled sources (enabled-root containment
    /// for [`Self::belongs_to_source`]).
    enabled_roots: Vec<PathBuf>,

    /// Serializes all mutating entry points.
    lock: Mutex<()>,
}

impl<'a> Runner<'a> {
    /// Wraps the injected collaborators (design D3) and builds the source
    /// index from the global config.
    ///
    /// Unresolvable source paths are skipped, never fatal.
    pub fn new(params: RunnerParams<'a>) -> Self {
        let RunnerParams {
            db,
            ingest_cfg,
            global,
            domains,
            registry,
            embed,
            vectors,
            prompts,
            linker_cfg,
            prompts_path,
            llm_cache,
        } = params;

        let mut source_index = BTreeMap::new();
        let mut enabled_roots = Vec::new();
        if let Some(global) = global {
            for (index, src) in global.sources.iter().enumerate() {
                let Ok(abs) = to_abs_path(&src.path) else {
                    continue;
                };
                source_index.insert(abs.to_string_lossy().into_owned(), index);
                if !src.disabled {
                    enabled_roots.push(abs);
                }
            }
        }

        Self {
            db,
            ingest_cfg,
            global,
            domains,
            registry,
            embed,
            vectors,
            prompts,
            linker_cfg,
            prompts_path,
            llm_cache,
            source_index,
            enabled_roots,
            lock: Mutex::new(()),
        }
    }

    /// Finds the configured source containing `path`.
    ///
    /// An exact match on the absolute normalized path wins (the watch root
    /// itself); otherwise the longest configured root that contains the path
    /// wins. Paths that no longer exist on disk are matched too (deleted
    /// files) — no filesystem access happens here.
    ///
    /// Returns `None` when the path cannot be resolved or no configured
    /// source contains it.
    pub fn find_source_for_path(&self, path: &str) -> Option<&SourceConfig> {
        let Ok(path_abs) = to_abs_path(path) else {
            return None;
        };
        let key = path_abs.to_string_lossy().into_owned();
        if let Some(&index) = self.source_index.get(&key) {
            return self.source_by_index(index);
        }
        let mut best: Option<(usize, usize)> = None; // (root length, source index)
        for (root, &index) in &self.source_index {
            let contains = is_within(&path_abs, Path::new(root));
            if contains && best.is_none_or(|(len, _)| root.len() > len) {
                best = Some((root.len(), index));
            }
        }
        best.and_then(|(_, index)| self.source_by_index(index))
    }

    /// True when `path` lies under any enabled configured source root.
    pub fn belongs_to_source(&self, path: &str) -> bool {
        let Ok(path_abs) = to_abs_path(path) else {
            return false;
        };
        self.enabled_roots
            .iter()
            .any(|root| is_within(&path_abs, root))
    }

    /// The registry [`Source`] implementation for the configured source
    /// `src` (the lookup behind every ingest entry point).
    ///
    /// The queue producer (document-jobs-queue task 1.3) walks the source
    /// tree with it without running the pipeline; the worker (task 1.4)
    /// resolves per-job sources the same way.
    ///
    /// Returns `None` when the source's type word has no registered
    /// implementation (the registry is the source of truth, design D5) —
    /// callers surface it as an explicit error.
    pub fn source_for_config(&self, src: &SourceConfig) -> Option<&dyn Source> {
        self.registry.get(&resolve_source_type(src)).ok()
    }

    /// Runs the full per-document pipeline for the single file at `path`
    /// (document-jobs-queue task 1.4, the worker's `index` op).
    ///
    /// Resolves the configured source containing `path`, reads the ONE file
    /// with [`Parser::parse_file`] (no source-tree walk), and runs
    /// [`Ingester::process_document`] on the result. If the file is
    /// genuinely gone (`NotFound`) the document row is removed (converge)
    /// and `Ok(())` is returned.
    ///
    /// # Errors
    ///
    /// [`IngestionError::NoSourceForPath`] when no configured source
    /// contains `path`, [`IngestionError::UnknownSourceType`] when the
    /// source's type word has no registered implementation, or any
    /// source-level [`IngestionError`] from the single-file read or the
    /// pipeline.
    pub fn process_document_by_path(&self, path: &str) -> Result<(), IngestionError> {
        let _guard = self.lock();
        self.process_document_by_path_locked(path)
    }

    /// Executes the `delete` op for the document at `path` (document-jobs-
    /// queue task 1.4): removes the document row and all its dependent data
    /// (chunks, provenance, scoped orphans) in one transaction.
    ///
    /// Returns `true` when a document was found and removed, `false` when
    /// no document row exists for `path` (idempotent no-op).
    ///
    /// # Errors
    ///
    /// Any [`IngestionError::Db`] failure from the cleanup transaction.
    pub fn delete_document_at(&self, path: &str) -> Result<bool, IngestionError> {
        let _guard = self.lock();
        self.delete_document_at_locked(path)
    }

    /// The unlocked core of [`Self::process_document_by_path`] (callers
    /// hold the runner mutex).
    fn process_document_by_path_locked(&self, path: &str) -> Result<(), IngestionError> {
        // The file may have been deleted since the job was enqueued.
        if matches!(
            std::fs::symlink_metadata(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound
        ) {
            // Converge: remove the document row if it exists.
            let doc = self.db.with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).get_by_path(path)
            })??;
            if let Some(doc) = doc {
                self.clear_and_delete_doc(doc.id)?;
            }
            return Ok(());
        }

        let src =
            self.find_source_for_path(path)
                .ok_or_else(|| IngestionError::NoSourceForPath {
                    path: path.to_owned(),
                })?;
        let source = self
            .source_for_config(src)
            .ok_or_else(|| IngestionError::UnknownSourceType(src.path.clone()))?;
        let enriched = DomainEnrichedSource {
            inner: source,
            domains: &src.domains,
        };
        // One file, by path — the worker never walks a source directory
        // (document-jobs-queue design Correction).
        let doc = enriched.parse_file(Path::new(path), Path::new(&src.path))?;
        let ner = self.build_ner_provider(src);
        let resolver = Resolver::new(self.ingest_cfg.resolver.similarity_threshold);
        let sink = SinkAdapter {
            index: self.vectors,
        };
        let ingester = Ingester::new(
            self.db,
            self.ingest_cfg,
            &enriched,
            self.embed,
            ner.as_deref(),
            &resolver,
            &sink,
        );
        let mut tracker = ProgressTracker::new(1, &format!("Processing {}", path));
        ingester.process_document(&doc, &mut tracker)?;
        Ok(())
    }

    /// The unlocked core of [`Self::delete_document_at`] (callers hold the
    /// runner mutex).
    fn delete_document_at_locked(&self, path: &str) -> Result<bool, IngestionError> {
        let doc = self.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn)).get_by_path(path)
        })??;
        match doc {
            Some(doc) => {
                self.clear_and_delete_doc(doc.id)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Removes a document row and all its dependent data (chunks, entity
    /// links, facts, provenance, scoped orphans) in one transaction
    /// (shared by [`Self::delete_document_at`] and the worker's
    /// converge-on-deleted-file path).
    fn clear_and_delete_doc(&self, doc_id: i64) -> Result<(), IngestionError> {
        self.db.exec_tx(|tx| -> Result<(), IngestionError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            GcDao::new(exec).full_clear_doc_by_id(doc_id)?;
            DocumentDao::new(exec).delete(doc_id)?;
            Ok(())
        })
    }

    /// The source at a `global.sources` index, or `None` (no global config).
    fn source_by_index(&self, index: usize) -> Option<&SourceConfig> {
        self.global.and_then(|g| g.sources.get(index))
    }

    /// Assembles the per-source NER provider.
    ///
    /// Returns `None` — the run proceeds without NER — when NER is disabled
    /// in the preset or the composite construction fails (degradation:
    /// warning only). Domain configs are resolved by name from the injected
    /// map; a missing domain is warned and skipped. Stage methods come from
    /// `GlobalConfig.ner.methods` (empty without a global ontology).
    fn build_ner_provider(&self, src: &SourceConfig) -> Option<Box<dyn NerProvider>> {
        if self.ingest_cfg.ner.disabled {
            return None;
        }
        let mut domain_configs = Vec::with_capacity(src.domains.len());
        for name in &src.domains {
            match self.domains.get(name) {
                Some(config) => domain_configs.push(config.clone()),
                None => eprintln!("warning: no domain config for {name:?}, skipping it for NER"),
            }
        }
        let methods = self
            .global
            .map(|g| g.ner.methods.as_slice())
            .unwrap_or_default();
        match CompositeNer::build_from_stages(
            methods,
            &domain_configs,
            &self.ingest_cfg.ner.llm,
            self.prompts.clone(),
            self.llm_cache.clone(),
        ) {
            Ok(composite) => Some(Box::new(composite)),
            Err(err) => {
                eprintln!(
                    "warning: NER provider construction failed, continuing without NER: {err}"
                );
                None
            }
        }
    }

    /// Acquires the run mutex.
    ///
    /// A poisoned lock is recovered: the guarded state is `()`, so a
    /// panicking holder cannot have corrupted anything.
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Adapts the Runner's type-erased [`VectorIndex`] handle to the Ingester's
/// narrow write seam (task 3.8): the blanket `VectorSink` impl covers
/// concrete engines, but not `dyn VectorIndex` itself (Rust has no
/// trait-object upcast across it), while the orphan reconciliation
/// (`chunk_ids` / `delete_by_chunk_ids`) needs the full trait.
struct SinkAdapter<'a> {
    /// The full vector-index engine handle.
    index: &'a dyn VectorIndex,
}

impl VectorSink for SinkAdapter<'_> {
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), IngestionError> {
        VectorIndex::insert_batch(self.index, rows).map_err(IngestionError::from)
    }
}

/// Wraps a registry source and stamps the source's domain list into every
/// parsed document's metadata ([`DOMAIN_METADATA_KEY`]).
///
/// The [`Ingester`] parses through the fused [`Source`] trait (design D2),
/// so the enrichment happens here at document level rather than by wrapping
/// a standalone parser interface: [`Parser::parse`] delegates to the inner
/// source and then stamps [`DOMAIN_METADATA_KEY`] on every document;
/// chunking and supported extensions delegate unchanged.
struct DomainEnrichedSource<'a> {
    /// The registry source implementation.
    inner: &'a dyn Source,
    /// The source's domain names (`<source><domains>`).
    domains: &'a [String],
}

impl Parser for DomainEnrichedSource<'_> {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut result = self.inner.parse(source_path);
        let domains = Value::Array(
            self.domains
                .iter()
                .map(|d| Value::String(d.clone()))
                .collect(),
        );
        for doc in &mut result.documents {
            doc.metadata
                .extra
                .insert(DOMAIN_METADATA_KEY.to_owned(), domains.clone());
        }
        result
    }

    fn parse_file(&self, path: &Path, root: &Path) -> Result<Document, IngestionError> {
        let mut doc = self.inner.parse_file(path, root)?;
        let domains = Value::Array(
            self.domains
                .iter()
                .map(|d| Value::String(d.clone()))
                .collect(),
        );
        doc.metadata
            .extra
            .insert(DOMAIN_METADATA_KEY.to_owned(), domains);
        Ok(doc)
    }

    fn supported_extensions(&self) -> &[&str] {
        self.inner.supported_extensions()
    }
}

impl Chunker for DomainEnrichedSource<'_> {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        self.inner.chunk(content, metadata)
    }
}

impl Source for DomainEnrichedSource<'_> {}

/// Infers the source type word from the path's base name.
///
/// A base containing `wiki` (which also matches every mediawiki name) →
/// `"mediawiki"`; containing `webpage` → `"webpages"`; otherwise →
/// `"unstructured"`. Case-insensitive; the base name is the last path
/// component (a trailing slash is ignored).
pub fn detect_source_type(path: &str) -> &'static str {
    let base = Path::new(path)
        .components()
        .next_back()
        .and_then(|component| match component {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            // A root-only path (`/`) has no base name: nothing to detect.
            _ => None,
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    if base.contains("wiki") {
        "mediawiki"
    } else if base.contains("webpage") {
        "webpages"
    } else {
        "unstructured"
    }
}

/// The registry word for a configured source: the explicit `type` attribute,
/// or the detected one when the attribute is empty (detected from the path).
fn resolve_source_type(src: &SourceConfig) -> String {
    match &src.source_type {
        SourceType::Unknown(word) if word.is_empty() => detect_source_type(&src.path).to_owned(),
        other => source_type_word(other),
    }
}

/// Maps a [`SourceType`] to its registry word (the `<source type>` attribute
/// vocabulary of `global.xml`).
fn source_type_word(kind: &SourceType) -> String {
    match kind {
        SourceType::Markdown => "markdown".to_owned(),
        SourceType::Webpages => "webpages".to_owned(),
        SourceType::Mediawiki => "mediawiki".to_owned(),
        SourceType::Unstructured => "unstructured".to_owned(),
        SourceType::Unknown(word) => word.clone(),
    }
}

/// Resolves `path` against the current directory and normalizes it
/// lexically (no filesystem access beyond reading the current directory).
///
/// # Errors
///
/// Propagates the OS error when the current directory cannot be read (a
/// relative `path` with no working directory).
fn to_abs_path(path: &str) -> Result<PathBuf, std::io::Error> {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(joined.components().collect())
}

/// True when `path` is `root` itself or lies strictly under it.
///
/// Component-aware: `/data/docs2` must not be treated as inside
/// `/data/docs` (a raw string prefix check would).
///
/// `pub(crate)`: the queue producer's reconcile (document-jobs-queue
/// task 1.3) filters `documents` rows by the same containment.
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    let root_len = root.components().count();
    let path_len = path.components().count();
    path_len >= root_len
        && path
            .components()
            .zip(root.components())
            .all(|(path_component, root_component)| path_component == root_component)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use config::DomainConfig;
    use config::ontology::{GlobalConfig, GlobalNerConfig, NerMethod, SourceConfig, SourceType};
    use config::preset::{IngestionConfig, LinkerConfig};
    use db::test_util::in_memory_db;
    use embedding::EmbeddingError;
    use vectors::{VectorIndex, VectorsError};

    use super::*;
    use crate::ner::load_ner_prompts;
    use crate::parsers::walk_matched_files;
    use crate::sources::Registry;
    use crate::types::{Document, DocumentChunk, DocumentMetadata, ParseResult};

    /// A minimal source that walks `.txt` files and emits one document per
    /// file (same contract as the ingester task 3.4 test source).
    struct TestSource;

    impl TestSource {
        /// Reads one `.txt` file into a test document.
        fn read_file(path: &Path) -> Result<Document, IngestionError> {
            let content = fs::read_to_string(path).map_err(|source| IngestionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(Document {
                source_path: path.to_path_buf(),
                content,
                metadata: DocumentMetadata {
                    source_type: "test".to_owned(),
                    ..Default::default()
                },
            })
        }
    }

    impl Parser for TestSource {
        fn parse(&self, source_path: &Path) -> ParseResult {
            let mut documents = Vec::new();
            let mut errors = Vec::new();
            walk_matched_files(
                source_path,
                |path| path.extension().is_some_and(|ext| ext == "txt"),
                |path| {
                    documents.push(Self::read_file(path)?);
                    Ok(())
                },
                &mut errors,
            );
            ParseResult { documents, errors }
        }

        fn parse_file(&self, path: &Path, _root: &Path) -> Result<Document, IngestionError> {
            Self::read_file(path)
        }

        fn supported_extensions(&self) -> &[&str] {
            &[".txt"]
        }
    }

    impl Chunker for TestSource {
        fn chunk(
            &self,
            content: &str,
            metadata: &DocumentMetadata,
        ) -> Result<Vec<DocumentChunk>, IngestionError> {
            let mut chunks = Vec::new();
            let mut start = 0usize;
            for (seq, line) in content.split_inclusive('\n').enumerate() {
                let end = start + line.len();
                if !line.trim().is_empty() {
                    chunks.push(DocumentChunk {
                        doc_id: None,
                        text: line.to_owned(),
                        // No section context in this test source.
                        search_text: line.to_owned(),
                        sequence_num: seq,
                        start_offset: start,
                        end_offset: end,
                        // No chunk-specific keys in this test source: the bag
                        // is the document's `extra` as-is.
                        metadata: metadata.extra.clone(),
                    });
                }
                start = end;
            }
            Ok(chunks)
        }
    }

    impl Source for TestSource {}

    /// A deterministic embedding provider: vector `i` is all `(i + 1)`.
    pub(super) struct MockEmbedding {
        dim: usize,
    }

    impl EmbeddingProvider for MockEmbedding {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok((0..texts.len())
                .map(|i| vec![(i + 1) as f32; self.dim])
                .collect())
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    /// In-memory [`VectorIndex`] stub: records every row (tests assert on
    /// it). `fail_reads` makes `chunk_ids` fail (the cleanup error path,
    /// task 3.8 test).
    pub(super) struct MemoryIndex {
        rows: Mutex<BTreeMap<u32, Vec<f32>>>,
        fail_reads: AtomicBool,
    }

    impl MemoryIndex {
        /// An empty index with working reads.
        pub(super) fn new() -> Self {
            Self {
                rows: Mutex::new(BTreeMap::new()),
                fail_reads: AtomicBool::new(false),
            }
        }

        /// Makes `chunk_ids` fail (the cleanup error path, task 3.8 test).
        pub(super) fn set_fail_reads(&self, fail: bool) {
            self.fail_reads.store(fail, Ordering::SeqCst);
        }
    }

    impl VectorIndex for MemoryIndex {
        fn insert(&self, chunk_id: u32, vector: &[f32]) -> Result<(), VectorsError> {
            self.rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(chunk_id, vector.to_vec());
            Ok(())
        }

        fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), VectorsError> {
            let mut map = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
            for (id, vector) in rows {
                map.insert(*id, vector.to_vec());
            }
            Ok(())
        }

        fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(u32, f32)>, VectorsError> {
            Ok(Vec::new())
        }

        fn delete_by_chunk_ids(&self, chunk_ids: &[u32]) -> Result<(), VectorsError> {
            let mut map = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
            for id in chunk_ids {
                map.remove(id);
            }
            Ok(())
        }

        fn chunk_ids(&self) -> Result<Vec<u32>, VectorsError> {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(VectorsError::Engine("test failure".to_owned()));
            }
            Ok(self
                .rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .keys()
                .copied()
                .collect())
        }

        fn count(&self) -> Result<u64, VectorsError> {
            Ok(self
                .rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len() as u64)
        }

        fn build_index(&self) -> Result<(), VectorsError> {
            Ok(())
        }

        fn rebuild(&self, rows: &[(u32, Vec<f32>)]) -> Result<(), VectorsError> {
            let mut map = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
            map.clear();
            for (id, vector) in rows {
                map.insert(*id, vector.clone());
            }
            Ok(())
        }
    }

    /// Test fixture: in-memory DB, default collaborators and a runner built
    /// over them. Shared with the `runner/cleanup.rs` tests (task 3.8).
    pub(super) struct Harness {
        pub(super) db: Db,
        /// The cache database (task 1.10): holds the linker decision cache
        /// and the `last_linking_run` marker, mirroring production where the
        /// runner owns a separate cache handle.
        pub(super) cache: Db,
        pub(super) cfg: IngestionConfig,
        pub(super) global: GlobalConfig,
        pub(super) domains: HashMap<String, DomainConfig>,
        pub(super) registry: Registry,
        pub(super) embed: MockEmbedding,
        pub(super) sink: Arc<MemoryIndex>,
        pub(super) prompts: crate::ner::NerPrompts,
        pub(super) linker_cfg: LinkerConfig,
    }

    impl Harness {
        pub(super) fn new() -> Self {
            let mut registry = Registry::new();
            registry
                .register("unstructured", Box::new(TestSource))
                .unwrap();
            Self {
                db: in_memory_db(),
                cache: in_memory_db(),
                cfg: IngestionConfig::default(),
                global: GlobalConfig {
                    sources: Vec::new(),
                    cross_domain_links: None,
                    ner: GlobalNerConfig {
                        methods: vec![NerMethod::Regex],
                    },
                    entities: Vec::new(),
                    relations: Vec::new(),
                    extraction: Default::default(),
                },
                domains: HashMap::new(),
                registry,
                embed: MockEmbedding { dim: 4 },
                sink: Arc::new(MemoryIndex::new()),
                prompts: load_ner_prompts("/nonexistent-ner-prompts").unwrap(),
                linker_cfg: LinkerConfig::default(),
            }
        }

        pub(super) fn runner(&self) -> Runner<'_> {
            Runner::new(RunnerParams {
                db: &self.db,
                ingest_cfg: &self.cfg,
                global: Some(&self.global),
                domains: &self.domains,
                registry: &self.registry,
                embed: &self.embed,
                vectors: self.sink.as_ref(),
                prompts: &self.prompts,
                linker_cfg: &self.linker_cfg,
                prompts_path: "/nonexistent-prompts",
                llm_cache: Some(self.cache.clone()),
            })
        }
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new(prefix: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ingestion-runner-{prefix}-{id}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(super) fn sub(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Builds a [`SourceConfig`] for the tests.
    pub(super) fn source_config(
        path: &str,
        source_type: SourceType,
        disabled: bool,
        domains: &[&str],
    ) -> SourceConfig {
        SourceConfig {
            path: path.to_owned(),
            source_type,
            disabled,
            space: String::new(),
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            dataset: String::new(),
        }
    }
}
