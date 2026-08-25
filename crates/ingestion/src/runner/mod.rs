//! Multi-source ingestion runner (pipeline task 3.7, design D3/D4).
//!
//! Oracle reference: `internal/ingestion/runner/runner.go`. The oracle's
//! `Runner` is a stateful service object assembled by the CLI; this module
//! ports its behavior with the design D3 re-architecture: the [`Runner`]
//! holds no globals — every dependency (db, configs, registry, providers) is
//! a reference passed to [`Runner::new`] via [`RunnerParams`].
//!
//! Responsibilities (oracle parity):
//!
//! - [`Runner::ingest_all`] — processes the enabled configured sources
//!   sequentially; a failing source is collected into
//!   [`SummaryStats::errors`] and the run continues. The post-run orphan
//!   cleanup + entity-linking tail (`runner/cleanup.rs`) runs under the same
//!   lock; its failures are collected into [`SummaryStats::errors`] too.
//! - [`Runner::ingest_source`] — single-source entry point (explicit type or
//!   detected), with per-source NER provider assembly and domain enrichment.
//! - [`Runner::sync_source`] / [`Runner::ingest_source_by_path`] —
//!   incremental (rebuild = false) entry points for the file watcher and the
//!   CLI.
//! - [`Runner::belongs_to_source`] — containment predicate used by the prune
//!   stage (`runner/cleanup.rs`).
//!
//! All mutating entry points are serialized by an internal mutex (oracle
//! `r.mu`): a future file watcher and a CLI sync must never write SQLite
//! concurrently (design D4).
//!
//! Deliberate deviations from the oracle (behavior is ported, not
//! transcribed — migration principles):
//!
//! - Domain enrichment wraps the fused [`Source`] (parser + chunker) rather
//!   than a standalone parser interface: the Rust [`Ingester`] parses through
//!   one `Source` trait (design D2), so the wrapper stamps
//!   [`DOMAIN_METADATA_KEY`] on parsed documents (see the private
//!   `DomainEnrichedSource` below).
//! - Path containment is component-aware (the private `is_within` helper):
//!   the oracle's raw string prefix check would treat `/data/docs2/file.md`
//!   as inside `/data/docs`.
//! - `detectSourceType`'s `contains("mediawiki")` branch is subsumed by
//!   `contains("wiki")` (every mediawiki name contains "wiki"); the oracle's
//!   second check was dead code.
//! - Warnings use `eprintln!` (crate convention, no logger in the frozen
//!   stack), not the oracle's `log` package.

pub mod cleanup;

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use config::DomainConfig;
use config::ontology::{GlobalConfig, SourceConfig, SourceType};
use config::preset::{IngestionConfig, LinkerConfig};
use db::Db;
use embedding::EmbeddingProvider;
use serde_json::Value;
use vectors::VectorIndex;

pub use cleanup::OrphanCleanupStats;

use crate::entities::Resolver;
use crate::error::IngestionError;
use crate::ingester::{Ingester, VectorSink};
use crate::ner::{CompositeNer, NerPrompts, NerProvider};
use crate::progress::ProgressStats;
use crate::sources::Registry;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata, ParseResult, Parser, Source};

/// The `metadata.extra` key carrying the source's domain list (port of the
/// oracle's `domainEnrichedParser`, which set `metadata["domain"] = src.Domain`).
pub const DOMAIN_METADATA_KEY: &str = "domain";

/// Aggregated outcome of a multi-source run (oracle `SummaryStats`).
///
/// Per-source progress is the usual [`ProgressStats`]; this struct adds the
/// cross-source bookkeeping: how many sources completed and the per-source
/// error list (a failing source never aborts the run).
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

/// Collaborators for [`Runner::new`] (design D3: injection instead of the
/// oracle's assembled service object).
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
    /// [`VectorSink`](crate::ingester::VectorSink) blanket impl, plus the
    /// orphan reconciliation of `cleanup_orphaned_data`, task 3.8).
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
    /// LLM-NER cache database (a separate handle per the oracle); `None`
    /// disables response caching.
    pub llm_cache: Option<Db>,
}

/// Multi-source ingestion orchestrator (design D3/D4).
///
/// Holds references to every collaborator plus the source index built from
/// the configured sources (absolute normalized path → config index) and the
/// enabled roots. All mutating entry points take an internal mutex (oracle
/// `r.mu`).
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
    /// includes disabled sources (path lookup does not filter, oracle parity).
    source_index: BTreeMap<String, usize>,
    /// Absolute paths of the non-disabled sources (prune containment).
    enabled_roots: Vec<PathBuf>,

    /// Serializes all mutating entry points (oracle `r.mu`).
    lock: Mutex<()>,
}

impl<'a> Runner<'a> {
    /// Wraps the injected collaborators (design D3) and builds the source
    /// index from the global config (oracle `NewRunner` bookkeeping).
    ///
    /// Unresolvable source paths are skipped (oracle `continue`), never
    /// fatal.
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

    /// Processes every enabled configured source sequentially (oracle
    /// `IngestAll`).
    ///
    /// A source-level failure is collected into [`SummaryStats::errors`] and
    /// the run continues with the next source (oracle parity). `rebuild` is
    /// forwarded to every source (design D6 rebuild-clear).
    ///
    /// The post-run tail (oracle `IngestAll`): [`cleanup_orphaned_data`](Self::cleanup_orphaned_data)
    /// followed by [`build_entity_links`](Self::build_entity_links). Both
    /// failures are collected into [`SummaryStats::errors`] and never fatal
    /// (design D8).
    pub fn ingest_all(&self, rebuild: bool) -> SummaryStats {
        let _guard = self.lock();
        let mut stats = SummaryStats::default();
        let sources = self
            .global
            .map(|g| g.sources.as_slice())
            .unwrap_or_default();
        for src in sources {
            if src.disabled {
                eprintln!("skipping disabled source: {}", src.path);
                continue;
            }
            match self.ingest_source_locked(src, rebuild) {
                Ok(progress) => {
                    stats.sources_processed += 1;
                    stats.documents_created += progress.documents_created;
                    stats.documents_updated += progress.documents_updated;
                    stats.documents_skipped += progress.documents_skipped;
                }
                Err(err) => {
                    eprintln!("ingest source {}: {err}", src.path);
                    stats.errors.push(format!("{}: {err}", src.path));
                }
            }
        }

        // Post-run tail (oracle `IngestAll`): orphan cleanup, then cross-
        // domain entity linking. Already holding the run mutex — the locked
        // cores are called directly.
        match self.cleanup_orphaned_data_locked() {
            Ok(cleanup) => eprintln!(
                "orphan cleanup completed: entities={}, facts={}, documents={}, vectors={}",
                cleanup.entities_deleted,
                cleanup.facts_deleted,
                cleanup.documents_deleted,
                cleanup.vectors_deleted,
            ),
            Err(err) => {
                eprintln!("cleanup orphaned data: {err}");
                stats.errors.push(format!("cleanup orphaned data: {err}"));
            }
        }
        match self.build_entity_links_locked() {
            // Non-fatal per-link failures propagate into the summary
            // (oracle parity: `stats.Errors = append(..., linkResult.Errors...)`).
            Ok(link) => stats.errors.extend(link.errors),
            Err(err) => {
                eprintln!("build entity links: {err}");
                stats.errors.push(format!("build entity links: {err}"));
            }
        }
        stats
    }

    /// Runs the full pipeline over one configured source (oracle
    /// `IngestSource`).
    ///
    /// Resolves the source type (explicit, or detected from the path when the
    /// `type` attribute is absent), looks up the registry implementation,
    /// wraps it with the domain enrichment, assembles the per-source NER
    /// provider and runs the [`Ingester`] over `src.path`.
    ///
    /// # Errors
    ///
    /// [`IngestionError::UnknownSourceType`] for an unregistered type word,
    /// or any source-level [`IngestionError`] from the ingest run.
    pub fn ingest_source(
        &self,
        src: &SourceConfig,
        rebuild: bool,
    ) -> Result<ProgressStats, IngestionError> {
        let _guard = self.lock();
        self.ingest_source_locked(src, rebuild)
    }

    /// Re-indexes the configured source containing `changed_path`
    /// incrementally (oracle `SyncSource`, the file-watcher entry point).
    ///
    /// # Errors
    ///
    /// [`IngestionError::NoSourceForPath`] when no configured source root
    /// contains the path (oracle `no configured source contains %s`), or any
    /// source-level [`IngestionError`] from the ingest run.
    pub fn sync_source(&self, changed_path: &str) -> Result<ProgressStats, IngestionError> {
        let _guard = self.lock();
        self.sync_source_locked(changed_path)
    }

    /// Runs an incremental sync for the source whose configured directory
    /// matches `path` exactly (absolute, normalized); any other path falls
    /// back to [`Self::sync_source`] (oracle `IngestSourceByPath`).
    ///
    /// # Errors
    ///
    /// [`IngestionError::Io`] when `path` cannot be resolved,
    /// [`IngestionError::NoSourceForPath`] when no source matches (fallback),
    /// or any source-level [`IngestionError`] from the ingest run.
    pub fn ingest_source_by_path(&self, path: &str) -> Result<ProgressStats, IngestionError> {
        let _guard = self.lock();
        let key = to_abs_path(path)
            .map(|abs| abs.to_string_lossy().into_owned())
            .map_err(|source| IngestionError::Io {
                path: PathBuf::from(path),
                source,
            })?;
        if let Some(src) = self
            .source_index
            .get(&key)
            .and_then(|&index| self.source_by_index(index))
        {
            return self.ingest_source_locked(src, false);
        }
        self.sync_source_locked(path)
    }

    /// Finds the configured source containing `path` (oracle
    /// `findSourceForPath` / `SourceForPath`).
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

    /// True when `path` lies under any enabled configured source root
    /// (oracle `belongsToSource`; the prune stage, task 3.8, uses this).
    pub fn belongs_to_source(&self, path: &str) -> bool {
        let Ok(path_abs) = to_abs_path(path) else {
            return false;
        };
        self.enabled_roots
            .iter()
            .any(|root| is_within(&path_abs, root))
    }

    /// The unlocked core of [`Self::ingest_source`] (callers hold the lock).
    fn ingest_source_locked(
        &self,
        src: &SourceConfig,
        rebuild: bool,
    ) -> Result<ProgressStats, IngestionError> {
        let source_type = resolve_source_type(src);
        let source = self.registry.get(&source_type)?;
        let enriched = DomainEnrichedSource {
            inner: source,
            domains: &src.domains,
        };
        let ner = self.build_ner_provider(src);
        // Per-run resolver: cheap to build, stateful only for the run
        // (oracle `NewIngester` built it inside from the same threshold).
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
        ingester.ingest(Path::new(&src.path), rebuild)
    }

    /// The unlocked core of [`Self::sync_source`] (callers hold the lock).
    fn sync_source_locked(&self, changed_path: &str) -> Result<ProgressStats, IngestionError> {
        let Some(src) = self.find_source_for_path(changed_path) else {
            return Err(IngestionError::NoSourceForPath {
                path: changed_path.to_owned(),
            });
        };
        self.ingest_source_locked(src, false)
    }

    /// The source at a `global.sources` index, or `None` (no global config).
    fn source_by_index(&self, index: usize) -> Option<&SourceConfig> {
        self.global.and_then(|g| g.sources.get(index))
    }

    /// Assembles the per-source NER provider (oracle `buildNERProvider`).
    ///
    /// Returns `None` — the run proceeds without NER — when NER is disabled
    /// in the preset or the composite construction fails (degradation, oracle
    /// parity: warning only). Domain configs are resolved by name from the
    /// injected map; a missing domain is warned and skipped. Stage methods
    /// come from `GlobalConfig.ner.methods` (empty without a global
    /// ontology).
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

    /// Acquires the run mutex (oracle `r.mu.Lock`).
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
/// parsed document's metadata (port of the oracle's `domainEnrichedParser`,
/// which set `metadata["domain"] = src.Domain`).
///
/// The Rust [`Ingester`] parses through the fused [`Source`] trait (design
/// D2), so the enrichment happens here at document level rather than by
/// wrapping a standalone parser interface: [`Parser::parse`] delegates to the
/// inner source and then stamps [`DOMAIN_METADATA_KEY`] on every document;
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

/// Infers the source type word from the path's base name (oracle
/// `detectSourceType`).
///
/// A base containing `wiki` (which also matches `mediawiki` — the oracle's
/// second check was dead) → `"mediawiki"`; containing `webpage` →
/// `"webpages"`; otherwise → `"unstructured"`. Case-insensitive; the base
/// name is the last path component (a trailing slash is ignored).
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
/// or the detected one when the attribute is absent (oracle parity: an empty
/// `type` is detected from the path).
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
/// lexically (oracle `filepath.Abs` — no filesystem access beyond reading
/// the current directory).
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
/// Component-aware (deliberate fix over the oracle's raw string prefix:
/// `/data/docs2` must not be treated as inside `/data/docs`).
fn is_within(path: &Path, root: &Path) -> bool {
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
    use db::{ConnectionOrTx, DocumentDao};
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

    impl Parser for TestSource {
        fn parse(&self, source_path: &Path) -> ParseResult {
            let mut documents = Vec::new();
            let mut errors = Vec::new();
            walk_matched_files(
                source_path,
                |path| path.extension().is_some_and(|ext| ext == "txt"),
                |path| {
                    let content =
                        fs::read_to_string(path).map_err(|source| IngestionError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    documents.push(Document {
                        source_path: path.to_path_buf(),
                        content,
                        metadata: DocumentMetadata {
                            source_type: "test".to_owned(),
                            ..Default::default()
                        },
                    });
                    Ok(())
                },
                &mut errors,
            );
            ParseResult { documents, errors }
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
                        sequence_num: seq,
                        start_offset: start,
                        end_offset: end,
                        metadata: metadata.clone(),
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
                llm_cache: None,
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

    /// Builds a minimal domain config (no entities, no rules).
    pub(super) fn domain_config(name: &str) -> DomainConfig {
        DomainConfig {
            name: name.to_owned(),
            version: "1".to_owned(),
            description: String::new(),
            entities: Vec::new(),
            relations: Vec::new(),
            extraction: Default::default(),
            confidence: Default::default(),
        }
    }

    #[test]
    fn ingest_all_collects_source_errors_and_continues() {
        let root = TempDir::new("all");
        let good = root.sub("good");
        let bad = root.sub("bad");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("a.txt"), "alpha\nbeta\n").unwrap();
        // `bad` stays missing: the ingest run fails with an Io error.

        let mut harness = Harness::new();
        harness.global.sources = vec![
            source_config(
                bad.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
            source_config(
                good.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
        ];
        let runner = harness.runner();

        let stats = runner.ingest_all(false);

        assert_eq!(stats.sources_processed, 1, "{stats:?}");
        assert_eq!(stats.documents_created, 1, "{stats:?}");
        assert_eq!(stats.errors.len(), 1, "{stats:?}");
        assert!(
            stats.errors[0].starts_with(bad.to_string_lossy().as_ref()),
            "{}",
            stats.errors[0]
        );

        // The good source was processed after the failure: vectors were
        // written (the run did not stop at the first error).
        assert!(!harness.sink.chunk_ids().unwrap().is_empty());
    }

    #[test]
    fn ingest_all_skips_disabled_sources() {
        let root = TempDir::new("disabled");
        let off = root.sub("off");
        let on = root.sub("on");
        fs::create_dir_all(&off).unwrap();
        fs::write(off.join("off.txt"), "off\n").unwrap();
        fs::create_dir_all(&on).unwrap();
        fs::write(on.join("on.txt"), "on\n").unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![
            source_config(
                off.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                true,
                &[],
            ),
            source_config(
                on.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
        ];
        let runner = harness.runner();

        let stats = runner.ingest_all(false);

        assert_eq!(stats.sources_processed, 1, "{stats:?}");
        assert_eq!(stats.documents_created, 1, "{stats:?}");
        assert!(stats.errors.is_empty(), "{stats:?}");

        // Only the enabled source's document is in the database.
        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].original_path,
            on.join("on.txt").to_string_lossy().into_owned()
        );
    }

    #[test]
    fn detect_source_type_matches_the_oracle_cases() {
        assert_eq!(detect_source_type("/data/my-wiki"), "mediawiki");
        assert_eq!(detect_source_type("/data/mediawiki"), "mediawiki");
        assert_eq!(detect_source_type("/data/MY-WIKI"), "mediawiki");
        assert_eq!(detect_source_type("/data/wiki/"), "mediawiki");
        assert_eq!(detect_source_type("/data/webpages"), "webpages");
        assert_eq!(detect_source_type("/data/WEBPAGE-archive"), "webpages");
        assert_eq!(detect_source_type("/data/docs"), "unstructured");
        assert_eq!(detect_source_type("/data"), "unstructured");
    }

    #[test]
    fn find_source_for_path_exact_prefix_and_deleted_file() {
        let root = TempDir::new("find");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file.txt"), "x\n").unwrap();
        let deleted = src.join("deleted.txt");
        fs::write(&deleted, "x\n").unwrap();
        fs::remove_file(&deleted).unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        // Exact match on the watch root itself.
        let exact = runner
            .find_source_for_path(src.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(exact.path, src.to_string_lossy().into_owned());

        // A live file under the root.
        let live = runner
            .find_source_for_path(src.join("file.txt").to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(live.path, src.to_string_lossy().into_owned());

        // A deleted file still resolves (no filesystem access).
        let gone = runner
            .find_source_for_path(deleted.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(gone.path, src.to_string_lossy().into_owned());

        // No configured source contains this path.
        assert!(runner.find_source_for_path("/elsewhere/file.txt").is_none());
    }

    #[test]
    fn find_source_for_path_longest_prefix_wins_and_component_boundary() {
        let root = TempDir::new("nested");
        let outer = root.sub("docs");
        let inner = outer.join("nested");
        fs::create_dir_all(&inner).unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![
            source_config(
                outer.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
            source_config(
                inner.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
        ];
        let runner = harness.runner();

        // The nested root wins over the outer one (longest prefix).
        let file = inner.join("file.txt");
        let found = runner
            .find_source_for_path(file.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(found.path, inner.to_string_lossy().into_owned());

        // Component boundary: `<root>/docs2` is NOT inside `<root>/docs`
        // (the oracle's raw string prefix would have matched).
        let sibling = root.sub("docs2");
        assert!(
            runner
                .find_source_for_path(sibling.join("file.txt").to_string_lossy().as_ref())
                .is_none()
        );
    }

    #[test]
    fn domain_list_is_stamped_into_document_metadata() {
        let root = TempDir::new("domains");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello world\n").unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &["hr", "legal"],
        )];
        let runner = harness.runner();

        let progress = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");

        let docs = harness
            .db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap();
        assert_eq!(docs.len(), 1);
        // The stored metadata is the document's `extra` bag: exactly the
        // domain stamp.
        assert_eq!(
            docs[0].metadata_json.as_deref(),
            Some(r#"{"domain":["hr","legal"]}"#)
        );
    }

    #[test]
    fn ner_degrades_to_none_when_llm_stage_has_no_domains() {
        let root = TempDir::new("ner-llm");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness.global.ner.methods = vec![NerMethod::Llm];
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        // LlmNer::new fails (no domain configs) → the run degrades to
        // no-NER and still succeeds.
        let progress = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");
        assert_eq!(progress.entities_extracted, 0, "{progress:?}");
    }

    #[test]
    fn ner_degrades_to_none_when_stage_is_prose() {
        let root = TempDir::new("ner-prose");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness.global.ner.methods = vec![NerMethod::Prose];
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        // The `prose` stage is deferred (task 2.6): construction fails → the
        // run degrades to no-NER and still succeeds.
        let progress = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");
        assert_eq!(progress.entities_extracted, 0, "{progress:?}");
    }

    #[test]
    fn ner_skips_missing_domains_and_keeps_known_ones() {
        let root = TempDir::new("ner-domains");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness
            .domains
            .insert("known".to_owned(), domain_config("known"));
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &["known", "ghost"],
        )];
        let runner = harness.runner();

        // The ghost domain is warned and skipped; the known domain config
        // still reaches NER (no rules → no entities, but no construction
        // failure).
        let progress = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");
        assert_eq!(progress.entities_extracted, 0, "{progress:?}");
    }

    #[test]
    fn ingest_source_fails_on_unknown_type() {
        let root = TempDir::new("unknown-type");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();

        let harness = Harness::new();
        let src_cfg = source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unknown("confluence".to_owned()),
            false,
            &[],
        );
        let runner = harness.runner();

        let err = runner.ingest_source(&src_cfg, false).unwrap_err();
        assert!(
            matches!(err, IngestionError::UnknownSourceType(ref word) if word == "confluence"),
            "{err:?}"
        );
    }

    #[test]
    fn sync_source_resolves_and_ingests_incrementally() {
        let root = TempDir::new("sync");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        let progress = runner
            .sync_source(src.join("doc.txt").to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");

        // A path outside every configured source fails explicitly.
        let err = runner.sync_source("/elsewhere/file.txt").unwrap_err();
        assert!(
            matches!(err, IngestionError::NoSourceForPath { ref path } if path == "/elsewhere/file.txt"),
            "{err:?}"
        );
    }

    #[test]
    fn ingest_source_by_path_exact_match_and_fallback() {
        let root = TempDir::new("by-path");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        // Exact match on the configured root.
        let progress = runner
            .ingest_source_by_path(src.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(progress.documents_created, 1, "{progress:?}");

        // A file path is not an exact root match: the fallback (sync) finds
        // the containing source and re-runs it (unchanged → skipped).
        let progress = runner
            .ingest_source_by_path(src.join("doc.txt").to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(progress.documents_created, 0, "{progress:?}");
        assert_eq!(progress.documents_skipped, 1, "{progress:?}");

        // No source matches at all.
        let err = runner
            .ingest_source_by_path("/elsewhere/file.txt")
            .unwrap_err();
        assert!(
            matches!(err, IngestionError::NoSourceForPath { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn sequential_runs_are_serialized_and_deduplicated() {
        let root = TempDir::new("mutex");
        let src = root.sub("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("doc.txt"), "hello\n").unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![source_config(
            src.to_string_lossy().as_ref(),
            SourceType::Unstructured,
            false,
            &[],
        )];
        let runner = harness.runner();

        let first = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(first.documents_created, 1, "{first:?}");

        // A second run through the same mutex: all documents skipped
        // (content-hash dedup) — proves the lock is released between runs.
        let second = runner
            .ingest_source(&harness.global.sources[0], false)
            .unwrap();
        assert_eq!(second.documents_created, 0, "{second:?}");
        assert_eq!(second.documents_skipped, 1, "{second:?}");
    }

    #[test]
    fn belongs_to_source_uses_enabled_roots_only() {
        let root = TempDir::new("belongs");
        let on = root.sub("on");
        let off = root.sub("off");
        fs::create_dir_all(&on).unwrap();
        fs::create_dir_all(&off).unwrap();

        let mut harness = Harness::new();
        harness.global.sources = vec![
            source_config(
                on.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                false,
                &[],
            ),
            source_config(
                off.to_string_lossy().as_ref(),
                SourceType::Unstructured,
                true,
                &[],
            ),
        ];
        let runner = harness.runner();

        assert!(runner.belongs_to_source(on.join("a.txt").to_string_lossy().as_ref()));
        assert!(!runner.belongs_to_source(off.join("a.txt").to_string_lossy().as_ref()));
        assert!(!runner.belongs_to_source("/elsewhere/a.txt"));
    }
}
