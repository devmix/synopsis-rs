//! Integration tests for the multi-source ingestion runner (change
//! `ingestion-pipeline`, pipeline task 3.7, design D3/D4).
//!
//! Relocated from `src/runner/mod.rs` (change test-hygiene-phase-1,
//! task 1.12): the 13 unit tests that exercise [`Runner`] through the
//! test-local fixtures. `walk_matched_files` is reached through the
//! `ingestion::test_support` seam (design D3). The fixtures are local
//! copies of the ones kept inline in `src/runner/mod.rs`, which the
//! `runner/cleanup.rs` inline tests import (task 1.12 Revision 1, design
//! D4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use config::DomainConfig;
use config::ontology::{GlobalConfig, GlobalNerConfig, NerMethod, SourceConfig, SourceType};
use config::preset::{IngestionConfig, LinkerConfig};
use db::test_util::in_memory_db;
use db::{ChunkDao, ConnectionOrTx, Db, DocumentDao, EntityDao, QueueTaskDao};
use embedding::{EmbeddingError, EmbeddingProvider};
use vectors::{VectorIndex, VectorsError};

use ingestion::runner::detect_source_type;
use ingestion::worker::DocumentWorker;
use ingestion::{
    Chunker, Document, DocumentChunk, DocumentJobQueue, DocumentMetadata, IngestionError,
    NerPrompts, ParseResult, Parser, Registry, Runner, RunnerParams, Source, load_ner_prompts,
};

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
        ingestion::test_support::walk_matched_files(
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
struct MockEmbedding {
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
struct MemoryIndex {
    rows: Mutex<BTreeMap<u32, Vec<f32>>>,
    fail_reads: AtomicBool,
}

impl MemoryIndex {
    /// An empty index with working reads.
    fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            fail_reads: AtomicBool::new(false),
        }
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
/// over them. Local copy of the fixture shared with the `runner/cleanup.rs`
/// tests (task 3.8; task 1.12 Revision 1).
struct Harness {
    db: Db,
    /// The cache database (task 1.10): holds the linker decision cache
    /// and the `last_linking_run` marker, mirroring production where the
    /// runner owns a separate cache handle.
    cache: Db,
    cfg: IngestionConfig,
    global: GlobalConfig,
    aliases: HashMap<String, String>,
    domains: HashMap<String, DomainConfig>,
    registry: Registry,
    embed: MockEmbedding,
    sink: Arc<MemoryIndex>,
    prompts: NerPrompts,
    linker_cfg: LinkerConfig,
}

impl Harness {
    fn new() -> Self {
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
                aliases: Vec::new(),
            },
            aliases: HashMap::new(),
            domains: HashMap::new(),
            registry,
            embed: MockEmbedding { dim: 4 },
            sink: Arc::new(MemoryIndex::new()),
            prompts: load_ner_prompts("/nonexistent-ner-prompts").unwrap(),
            linker_cfg: LinkerConfig::default(),
        }
    }

    fn runner(&self) -> Runner<'_> {
        Runner::new(RunnerParams {
            db: &self.db,
            ingest_cfg: &self.cfg,
            global: Some(&self.global),
            aliases: &self.aliases,
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
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ingestion-runner-{prefix}-{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Builds a [`SourceConfig`] for the tests.
fn source_config(
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
fn domain_config(name: &str) -> DomainConfig {
    DomainConfig {
        name: name.to_owned(),
        version: "1".to_owned(),
        description: String::new(),
        entities: Vec::new(),
        relations: Vec::new(),
        extraction: Default::default(),
        confidence: Default::default(),
        aliases: Vec::new(),
    }
}

#[test]
fn failing_source_does_not_stop_the_run() {
    let root = TempDir::new("all");
    let good = root.sub("good");
    let bad = root.sub("bad");
    fs::create_dir_all(&good).unwrap();
    fs::write(good.join("a.txt"), "alpha\nbeta\n").unwrap();
    // `bad` stays missing: the reconcile run fails with an Io error.

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
    let queue = DocumentJobQueue::new(&harness.db);

    // The bad source (missing root) fails explicitly...
    let err = queue
        .reconcile_source(&runner, bad.to_string_lossy().as_ref())
        .unwrap_err();
    assert!(matches!(err, IngestionError::Io { .. }), "{err:?}");

    // ...and the run continues with the next source: its file is queued
    // and the worker indexes it (a failing source never aborts the run).
    let stats = queue
        .reconcile_source(&runner, good.to_string_lossy().as_ref())
        .unwrap();
    assert_eq!(stats.indexed, 1, "{stats:?}");

    let worker = DocumentWorker::new(&harness.db, &runner);
    worker.run_once(i64::MAX / 2).unwrap();

    // The good source was processed after the failure: the job is done
    // and vectors were written (the run did not stop at the first error).
    let jobs = harness
        .db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .unwrap()
        .unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].status, "done", "{jobs:?}");
    assert!(!harness.sink.chunk_ids().unwrap().is_empty());
}

#[test]
fn disabled_sources_produce_no_jobs() {
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
    let queue = DocumentJobQueue::new(&harness.db);

    // The startup reconcile loop skips disabled sources (the CLI's
    // `reconcile_enabled_sources` filters them out).
    for src in harness.global.sources.iter().filter(|src| !src.disabled) {
        let stats = queue.reconcile_source(&runner, &src.path).unwrap();
        assert_eq!(stats.indexed, 1, "{stats:?}");
    }

    let worker = DocumentWorker::new(&harness.db, &runner);
    worker.run_once(i64::MAX / 2).unwrap();

    // Zero jobs for the disabled source; the enabled one is done.
    let jobs = harness
        .db
        .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
        .unwrap()
        .unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(
        jobs[0].identity,
        on.join("on.txt").to_string_lossy().into_owned()
    );
    assert_eq!(jobs[0].status, "done", "{jobs:?}");

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
fn detect_source_type_matches_expected_cases() {
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
    // (the old raw string prefix would have matched).
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

    runner
        .process_document_by_path(src.join("doc.txt").to_string_lossy().as_ref())
        .unwrap();

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
    runner
        .process_document_by_path(src.join("doc.txt").to_string_lossy().as_ref())
        .unwrap();
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    let entities = harness
        .db
        .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert!(entities.is_empty(), "{entities:?}");
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
    runner
        .process_document_by_path(src.join("doc.txt").to_string_lossy().as_ref())
        .unwrap();
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    let entities = harness
        .db
        .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert!(entities.is_empty(), "{entities:?}");
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
    runner
        .process_document_by_path(src.join("doc.txt").to_string_lossy().as_ref())
        .unwrap();
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    let entities = harness
        .db
        .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert!(entities.is_empty(), "{entities:?}");
}

#[test]
fn process_document_by_path_indexes_exactly_one_file() {
    let root = TempDir::new("by-path-process");
    let src = root.sub("src");
    fs::create_dir_all(&src).unwrap();
    let a = src.join("a.txt");
    let b = src.join("b.txt");
    fs::write(&a, "alpha\n").unwrap();
    fs::write(&b, "beta\n").unwrap();

    let mut harness = Harness::new();
    harness.global.sources = vec![source_config(
        src.to_string_lossy().as_ref(),
        SourceType::Unstructured,
        false,
        &[],
    )];
    let runner = harness.runner();

    runner
        .process_document_by_path(a.to_string_lossy().as_ref())
        .unwrap();

    // Only the addressed file is indexed — the source directory is never
    // walked, so the sibling stays out of the database.
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(docs[0].original_path, a.to_string_lossy().into_owned());
    assert!(!harness.sink.chunk_ids().unwrap().is_empty());

    // An existing file outside every configured source fails
    // explicitly (a *missing* file converges as a no-op instead).
    let elsewhere = TempDir::new("elsewhere-by-path");
    let outside = elsewhere.sub("file.txt");
    fs::write(&outside, "x\n").unwrap();
    let err = runner
        .process_document_by_path(outside.to_string_lossy().as_ref())
        .unwrap_err();
    assert!(
        matches!(err, IngestionError::NoSourceForPath { .. }),
        "{err:?}"
    );
}

#[test]
fn process_document_by_path_converges_on_deleted_file() {
    let root = TempDir::new("by-path-gone");
    let src = root.sub("src");
    fs::create_dir_all(&src).unwrap();
    let a = src.join("a.txt");
    fs::write(&a, "alpha\n").unwrap();

    let mut harness = Harness::new();
    harness.global.sources = vec![source_config(
        src.to_string_lossy().as_ref(),
        SourceType::Unstructured,
        false,
        &[],
    )];
    let runner = harness.runner();

    runner
        .process_document_by_path(a.to_string_lossy().as_ref())
        .unwrap();
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");

    // The file is deleted after the run: the next call converges (the
    // document row is removed) and reports success.
    fs::remove_file(&a).unwrap();
    runner
        .process_document_by_path(a.to_string_lossy().as_ref())
        .unwrap();
    let docs = harness
        .db
        .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
        .unwrap()
        .unwrap();
    assert!(docs.is_empty(), "converged: {docs:?}");
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

    let doc = src.join("doc.txt").to_string_lossy().into_owned();
    runner.process_document_by_path(&doc).unwrap();
    let chunks = harness
        .db
        .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap();
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    let first_chunk_id = chunks[0].id;

    // A second run through the same mutex: the content hash is
    // unchanged, so the document is skipped and its chunk row keeps its
    // id (a re-run would full-clear and re-chunk it) — proves the lock
    // is released between runs.
    runner.process_document_by_path(&doc).unwrap();
    let chunks = harness
        .db
        .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())
        .unwrap()
        .unwrap();
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert_eq!(chunks[0].id, first_chunk_id, "{chunks:?}");
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
