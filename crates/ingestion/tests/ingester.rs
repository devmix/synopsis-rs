//! Integration tests for the per-document ingestion pipeline (change
//! `ingestion-pipeline`, design D2/D3).
//!
//! Relocated from `src/ingester/mod.rs` (change test-hygiene-phase-1,
//! task 1.11): the 23 unit tests that exercise [`Ingester`] through the
//! test-local `TestSource` / `Harness` fixtures. `walk_matched_files`
//! is reached through the `ingestion::test_support` seam (design D3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use config::preset::{ChunkingStrategy, IngestionConfig, MarkdownChunkerConfig};
use db::test_util::in_memory_db;
use db::{
    ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, EntityDao, FactDao, FactSourceDao,
};
use embedding::{EmbeddingError, EmbeddingProvider};
use serde_json::Map;

use ingestion::{
    Chunker, Document, DocumentChunk, DocumentMetadata, Ingester, IngestionError, MarkdownChunker,
    MarkdownSource, NerEntity, NerFact, NerProvider, NerResult, ParseResult, Parser, ProgressStats,
    Resolver, Source, VectorSink,
};

/// An in-memory test source: `parse` reads every `.txt` file of the
/// root (content = file content, `BROKEN`-prefixed content simulates a
/// parse failure); `chunk` splits on non-empty lines (one chunk per
/// line, byte offsets into the content).
struct TestSource;

impl TestSource {
    /// Reads one `.txt` file; `BROKEN`-prefixed content simulates a
    /// parse failure (same contract as the walk).
    fn read_file(path: &Path) -> Result<Document, IngestionError> {
        let content = fs::read_to_string(path).map_err(|source| IngestionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if content.starts_with("BROKEN") {
            return Err(IngestionError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "simulated parse failure",
                ),
            });
        }
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
        // Paths whose content simulates a parse failure (converted to
        // errors after the walk: the visit closure may not touch the
        // `errors` vector the walk helper owns).
        let mut broken = Vec::new();
        ingestion::test_support::walk_matched_files(
            source_path,
            |path| path.extension().is_some_and(|ext| ext == "txt"),
            |path| {
                match Self::read_file(path) {
                    Ok(doc) => documents.push(doc),
                    Err(err) => broken.push(err),
                }
                Ok(())
            },
            &mut errors,
        );
        errors.extend(broken);
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
        let mut offset = 0;
        for line in content.lines() {
            if !line.trim().is_empty() {
                let text = line.to_owned();
                chunks.push(DocumentChunk {
                    doc_id: None,
                    text: text.clone(),
                    // No section context in this test source: search_text is
                    // the text itself.
                    search_text: text,
                    sequence_num: chunks.len(),
                    start_offset: offset,
                    end_offset: offset + line.len(),
                    // No chunk-specific keys in this test source: the bag is
                    // the document's `extra` as-is.
                    metadata: metadata.extra.clone(),
                });
            }
            offset += line.len() + 1;
        }
        Ok(chunks)
    }
}

impl Source for TestSource {}

/// A source that produces a single chunk whose `search_text` carries a
/// breadcrumb context the pure-slice `text` does not (search-text-embedding
/// task 2.1): lets a test distinguish the embedding input (`search_text`)
/// from the NER input (`text`).
struct BreadCrumbSource;

impl Parser for BreadCrumbSource {
    fn parse(&self, source_path: &Path) -> ParseResult {
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        // Per-file read failures are collected here and folded in after the
        // walk (the visit closure may not touch the `errors` the walk owns).
        let mut broken = Vec::new();
        ingestion::test_support::walk_matched_files(
            source_path,
            |path| path.extension().is_some_and(|ext| ext == "txt"),
            |path| {
                match std::fs::read_to_string(path) {
                    Ok(content) => documents.push(Document {
                        source_path: path.to_path_buf(),
                        content,
                        metadata: DocumentMetadata {
                            source_type: "test".to_owned(),
                            ..Default::default()
                        },
                    }),
                    Err(source) => broken.push(IngestionError::Io {
                        path: path.to_path_buf(),
                        source,
                    }),
                }
                Ok(())
            },
            &mut errors,
        );
        errors.extend(broken);
        ParseResult { documents, errors }
    }

    fn parse_file(&self, path: &Path, _root: &Path) -> Result<Document, IngestionError> {
        let content = std::fs::read_to_string(path).map_err(|source| IngestionError::Io {
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

    fn supported_extensions(&self) -> &[&str] {
        &[".txt"]
    }
}

impl Chunker for BreadCrumbSource {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        let text = content.to_owned();
        Ok(vec![DocumentChunk {
            doc_id: None,
            text: text.clone(),
            // The synthetic search text: a breadcrumb context the pure
            // `text` does not carry.
            search_text: format!("Breadcrumb Context\n\n{text}"),
            sequence_num: 0,
            start_offset: 0,
            end_offset: content.len(),
            // No chunk-specific keys in this test source: the bag is the
            // document's `extra` as-is.
            metadata: metadata.extra.clone(),
        }])
    }
}

impl Source for BreadCrumbSource {}

/// A NER provider that records the content it receives (to verify the NER
/// stage runs on the pure `text`, not `search_text`; task 2.1 criterion 5).
struct RecordingNer {
    contents: Mutex<Vec<String>>,
}

impl NerProvider for RecordingNer {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn extract_entities(
        &self,
        content: &str,
        _metadata: &Map<String, serde_json::Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        self.contents.lock().unwrap().push(content.to_owned());
        Ok(None)
    }
}

/// A deterministic embedding provider: the i-th vector of a batch is
/// all `(i + 1)`. `mismatch` makes it return one vector short (the
/// count-mismatch error path); `fail_marker` makes it error on any batch
/// containing a text with that substring (per-document failure path).
/// `calls` records each batch size; `texts` records the exact texts handed to
/// the provider (in order) so a test can assert the embedding input.
struct MockEmbedding {
    dim: usize,
    mismatch: Mutex<bool>,
    fail_marker: Mutex<Option<String>>,
    calls: Mutex<Vec<usize>>,
    texts: Mutex<Vec<String>>,
}

impl MockEmbedding {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            mismatch: Mutex::new(false),
            fail_marker: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
        }
    }
}

impl EmbeddingProvider for MockEmbedding {
    fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if let Some(marker) = self.fail_marker.lock().unwrap().as_ref()
            && texts.iter().any(|text| text.contains(marker))
        {
            return Err(EmbeddingError::Ort("simulated engine failure".to_owned()));
        }
        self.calls.lock().unwrap().push(texts.len());
        self.texts.lock().unwrap().extend(texts.iter().cloned());
        let count = if *self.mismatch.lock().unwrap() {
            texts.len().saturating_sub(1)
        } else {
            texts.len()
        };
        Ok((0..count).map(|i| vec![(i + 1) as f32; self.dim]).collect())
    }

    fn vector_dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// A NER stub returning a fixed entity list for every non-empty chunk.
struct StubNer {
    entities: Vec<NerEntity>,
}

impl NerProvider for StubNer {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn extract_entities(
        &self,
        content: &str,
        _metadata: &Map<String, serde_json::Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        if content.trim().is_empty() || self.entities.is_empty() {
            return Ok(None);
        }
        Ok(Some(NerResult {
            entities: self.entities.clone(),
            facts: Vec::new(),
        }))
    }
}

/// A [`VectorSink`] that records every row (tests assert on the
/// post-commit writes directly).
struct RecordingSink {
    rows: Mutex<Vec<(u32, Vec<f32>)>>,
}

impl VectorSink for RecordingSink {
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), IngestionError> {
        self.rows
            .lock()
            .unwrap()
            .extend(rows.iter().map(|(id, vector)| (*id, vector.to_vec())));
        Ok(())
    }
}

/// A [`VectorSink`] that always fails (the design D5 post-commit
/// failure path: the SQLite data must survive).
struct FailingSink;

impl VectorSink for FailingSink {
    fn insert_batch(&self, _rows: &[(u32, &[f32])]) -> Result<(), IngestionError> {
        Err(IngestionError::Vectors(vectors::VectorsError::Engine(
            "simulated engine failure".to_owned(),
        )))
    }
}

fn test_entity(name: &str, entity_type: &str) -> NerEntity {
    NerEntity {
        name: name.to_owned(),
        entity_type: entity_type.to_owned(),
        description: String::new(),
        confidence: 1.0,
        domain: String::new(),
        metadata: Map::new(),
    }
}

/// A fact with an empty (global) domain and no metadata.
fn test_fact(
    subject: &str,
    subject_type: &str,
    predicate: &str,
    object: &str,
    object_type: &str,
) -> NerFact {
    NerFact {
        subject_type: subject_type.to_owned(),
        subject_name: subject.to_owned(),
        predicate: predicate.to_owned(),
        object_type: object_type.to_owned(),
        object_name: object.to_owned(),
        domain: String::new(),
        metadata: Map::new(),
    }
}

/// A NER stub returning a fixed result (entities AND facts) for every
/// non-empty chunk.
struct FactStubNer {
    result: NerResult,
}

impl NerProvider for FactStubNer {
    fn name(&self) -> &'static str {
        "fact-stub"
    }

    fn extract_entities(
        &self,
        content: &str,
        _metadata: &Map<String, serde_json::Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        if content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(self.result.clone()))
    }
}

/// A unique temp directory, removed on drop (tests run in parallel).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "synopsis-ingestion-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_file(root: &Path, name: &str, content: &str) {
    fs::write(root.join(name), content).unwrap();
}

/// Wires an in-memory database with the mock collaborators.
struct Harness {
    db: db::Db,
    cfg: IngestionConfig,
    source: TestSource,
    embed: Arc<MockEmbedding>,
    ner: StubNer,
    resolver: Resolver,
    sink: Arc<RecordingSink>,
}

impl Harness {
    fn new() -> Self {
        Self {
            db: in_memory_db(),
            cfg: IngestionConfig::default(),
            source: TestSource,
            embed: Arc::new(MockEmbedding::new(4)),
            ner: StubNer {
                entities: vec![test_entity("Alice", "person")],
            },
            resolver: Resolver::new(0.8),
            sink: Arc::new(RecordingSink {
                rows: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs the pipeline with the default (recording) sink.
    fn run(&self, root: &Path, rebuild: bool) -> ProgressStats {
        self.run_with(root, Some(&self.ner), self.sink.as_ref(), rebuild)
            .unwrap()
    }

    /// Runs the pipeline with explicit NER and sink overrides.
    fn run_with(
        &self,
        root: &Path,
        ner: Option<&dyn NerProvider>,
        sink: &dyn VectorSink,
        rebuild: bool,
    ) -> Result<ProgressStats, IngestionError> {
        let ingester = Ingester::new(
            &self.db,
            &self.cfg,
            &self.source,
            self.embed.as_ref(),
            ner.map(|n| n as &dyn NerProvider),
            &self.resolver,
            sink,
        );
        ingester.ingest(root, rebuild)
    }
}

#[test]
fn happy_path_creates_document_chunks_entities_and_vectors() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two\nline three");
    let h = Harness::new();

    let stats = h.run(&root, false);

    assert_eq!(stats.files_processed, 1, "{stats:?}");
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.documents_updated, 0, "{stats:?}");
    assert_eq!(stats.documents_skipped, 0, "{stats:?}");
    assert_eq!(stats.chunks_created, 3, "{stats:?}");
    assert_eq!(stats.embeddings_created, 3, "{stats:?}");
    assert_eq!(stats.entities_extracted, 3, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    let (doc, chunks) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let chunk_dao = ChunkDao::new(exec);
            let doc = docs.list().unwrap().pop().expect("one document");
            let chunks = chunk_dao.list_by_doc_id(doc.id).unwrap();
            (doc, chunks)
        })
        .unwrap();
    assert_eq!(doc.source_type, "test");
    assert!(doc.content_hash.is_some());
    assert_eq!(doc.metadata_json.as_deref(), Some("{}"));
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].chunk_text, "line one");
    assert_eq!(chunks[0].sequence_num, 0);

    // One entity (Alice) deduplicated across the three chunks.
    let entities =
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
        })
        .unwrap();
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].name, "Alice");

    // Every chunk is linked to the entity.
    let links =
        h.db.with_conn(|conn| {
            let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            chunks
                .iter()
                .map(|chunk| links.get_entities_by_chunk(chunk.id).unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert!(links.iter().all(|ids| ids == &vec![entities[0].id]));

    // Post-commit vector writes (design D5): ids match the committed
    // chunk rows, in chunk order, with the mock's deterministic values.
    let rows = h.sink.rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 3);
    for (row, chunk) in rows.iter().zip(chunks.iter()) {
        assert_eq!(row.0, chunk.id as u32);
    }
    assert_eq!(rows[0].1, vec![1.0f32; 4]);
    assert_eq!(rows[2].1, vec![3.0f32; 4]);
}

#[test]
fn unchanged_document_is_skipped() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let h = Harness::new();

    let first = h.run(&root, false);
    assert_eq!(first.documents_created, 1, "{first:?}");
    let rows_after_first = h.sink.rows.lock().unwrap().len();

    let second = h.run(&root, false);
    assert_eq!(second.files_processed, 1, "{second:?}");
    assert_eq!(second.documents_skipped, 1, "{second:?}");
    assert_eq!(second.documents_created, 0, "{second:?}");
    assert_eq!(second.chunks_created, 0, "{second:?}");
    assert_eq!(second.errors, 0, "{second:?}");

    // No re-embedding, no re-write, no new chunk rows.
    assert_eq!(h.sink.rows.lock().unwrap().len(), rows_after_first);
    let chunk_count =
        h.db.with_conn(|conn| {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc = docs.list().unwrap().pop().expect("document");
            ChunkDao::new(ConnectionOrTx::Connection(conn))
                .list_by_doc_id(doc.id)
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(chunk_count, 2);
}

#[test]
fn changed_document_updates_and_full_clears_old_data() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "alpha\nbeta");
    let h = Harness::new();

    let first = h.run(&root, false);
    assert_eq!(first.documents_created, 1, "{first:?}");
    let old_chunk_ids =
        h.db.with_conn(|conn| {
            let docs = DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc = docs.list().unwrap().pop().expect("document");
            ChunkDao::new(ConnectionOrTx::Connection(conn))
                .list_by_doc_id(doc.id)
                .unwrap()
                .into_iter()
                .map(|chunk| chunk.id)
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(old_chunk_ids.len(), 2);

    // Overwrite with different content: the document is updated, the
    // old chunks and their entity links are fully cleared, and the
    // stale entity (no remaining provenance, no facts) is deleted.
    write_file(&root, "a.txt", "gamma");
    let second = h.run(&root, false);
    assert_eq!(second.documents_updated, 1, "{second:?}");
    assert_eq!(second.documents_created, 0, "{second:?}");
    assert_eq!(second.chunks_created, 1, "{second:?}");

    let chunks =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let doc = docs.list().unwrap().pop().expect("document");
            ChunkDao::new(exec).list_by_doc_id(doc.id).unwrap()
        })
        .unwrap();
    assert_eq!(chunks.len(), 1, "old chunks must be gone");
    assert_eq!(chunks[0].chunk_text, "gamma");
    assert!(!old_chunk_ids.contains(&chunks[0].id));

    // The re-run re-resolved Alice (fresh row: the stale entity had no
    // remaining provenance), and only the new chunk is linked.
    let entities =
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
        })
        .unwrap();
    assert_eq!(entities.len(), 1);
    let links =
        h.db.with_conn(|conn| {
            ChunkEntityDao::new(ConnectionOrTx::Connection(conn))
                .get_entities_by_chunk(chunks[0].id)
                .unwrap()
        })
        .unwrap();
    assert_eq!(links, vec![entities[0].id]);
}

#[test]
fn empty_document_is_skipped() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "\n\n   \n");
    let h = Harness::new();

    let stats = h.run(&root, false);
    assert_eq!(stats.files_processed, 1, "{stats:?}");
    assert_eq!(stats.documents_skipped, 1, "{stats:?}");
    assert_eq!(stats.documents_created, 0, "{stats:?}");
    assert_eq!(stats.chunks_created, 0, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    let doc_count =
        h.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(doc_count, 0);
    assert!(h.sink.rows.lock().unwrap().is_empty());
}

#[test]
fn embeddings_are_generated_in_config_batches() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "one\ntwo\nthree\nfour\nfive");
    let mut h = Harness::new();
    h.cfg.batch_size = 2;

    let stats = h.run(&root, false);
    assert_eq!(stats.embeddings_created, 5, "{stats:?}");
    assert_eq!(*h.embed.calls.lock().unwrap(), vec![2, 2, 1]);
}

#[test]
fn embedding_count_mismatch_fails_the_document() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let h = Harness::new();
    *h.embed.mismatch.lock().unwrap() = true;

    let stats = h.run(&root, false);
    assert_eq!(stats.errors, 1, "{stats:?}");
    assert_eq!(stats.files_processed, 0, "{stats:?}");
    assert_eq!(stats.documents_created, 0, "{stats:?}");

    // Nothing committed, nothing written to the index.
    let doc_count =
        h.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(doc_count, 0);
    assert!(h.sink.rows.lock().unwrap().is_empty());
}

#[test]
fn per_document_failure_is_isolated() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "ok line");
    write_file(&root, "b.txt", "FAIL line");
    let h = Harness::new();
    *h.embed.fail_marker.lock().unwrap() = Some("FAIL".to_owned());

    // Files are processed in sorted walk order: a.txt first, b.txt
    // second. a.txt must commit even though b.txt fails embedding.
    let stats = h.run(&root, false);
    assert_eq!(stats.files_processed, 1, "{stats:?}");
    assert_eq!(stats.errors, 1, "{stats:?}");
    assert_eq!(stats.documents_created, 1, "{stats:?}");

    let paths: Vec<String> =
        h.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .into_iter()
                .map(|doc| doc.original_path)
                .collect()
        })
        .unwrap();
    assert_eq!(
        paths,
        vec![root.join("a.txt").to_string_lossy().into_owned()]
    );
}

#[test]
fn vector_failure_after_commit_keeps_sqlite_data() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let h = Harness::new();
    let sink = FailingSink;

    let stats = h.run_with(&root, Some(&h.ner), &sink, false).unwrap();
    assert_eq!(stats.errors, 1, "{stats:?}");
    assert_eq!(stats.files_processed, 0, "{stats:?}");

    // Design D5: the transaction committed before the vector write, so
    // the document and its chunks survive the index failure.
    let (doc_count, chunk_count) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let doc = docs.list().unwrap().pop().expect("document survived");
            let chunk_count = ChunkDao::new(exec).list_by_doc_id(doc.id).unwrap().len();
            (1, chunk_count)
        })
        .unwrap();
    assert_eq!(doc_count, 1);
    assert_eq!(chunk_count, 2);
}

#[test]
fn ner_disabled_skips_the_ner_stage() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let mut h = Harness::new();
    h.cfg.ner.disabled = true;

    let stats = h.run(&root, false);
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.entities_extracted, 0, "{stats:?}");
    assert_eq!(stats.chunks_created, 2, "{stats:?}");

    let entity_count =
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(entity_count, 0);
}

#[test]
fn ner_absent_skips_the_ner_stage() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one");
    let h = Harness::new();

    let stats = h.run_with(&root, None, h.sink.as_ref(), false).unwrap();
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.entities_extracted, 0, "{stats:?}");
}

// (search-text-embedding task 2.1, criteria 3 + 4 + 5) with a chunk whose
// `search_text` differs from `text`: the embedding leg receives
// `search_text`, the NER stage receives the pure `text`, and the chunk row
// persists both.
#[test]
fn embedding_input_is_search_text_and_ner_input_is_text() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "body line");
    let h = Harness::new();
    let ner = RecordingNer {
        contents: Mutex::new(Vec::new()),
    };

    let ingester = Ingester::new(
        &h.db,
        &h.cfg,
        &BreadCrumbSource,
        h.embed.as_ref(),
        Some(&ner),
        &h.resolver,
        h.sink.as_ref(),
    );
    let stats = ingester.ingest(&root, false).unwrap();
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.chunks_created, 1, "{stats:?}");
    assert_eq!(stats.embeddings_created, 1, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    // The embedding leg received the search_text (the breadcrumb context).
    let embedded = h.embed.texts.lock().unwrap().clone();
    assert_eq!(embedded, vec!["Breadcrumb Context\n\nbody line".to_owned()]);

    // The NER stage received the pure text, not the search_text.
    let ner_inputs = ner.contents.lock().unwrap().clone();
    assert_eq!(ner_inputs, vec!["body line".to_owned()]);

    // The chunk row persists both texts (criterion 4).
    let (chunk_text, search_text) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let doc = docs.list().unwrap().pop().expect("one document");
            let chunk = ChunkDao::new(exec)
                .list_by_doc_id(doc.id)
                .unwrap()
                .pop()
                .expect("one chunk");
            (chunk.chunk_text, chunk.search_text)
        })
        .unwrap();
    assert_eq!(chunk_text, "body line");
    assert_eq!(search_text, "Breadcrumb Context\n\nbody line");
}

// (chunk-metadata-persistence task 3.1) end to end: the ingester
// serializes each chunk's metadata bag to `chunks.metadata_json`. A
// sectioned Markdown chunk carries the `breadcrumb`/`section_title` keys;
// a chunk with an empty bag stores `NULL`.
#[test]
fn chunk_metadata_bag_is_persisted_to_metadata_json() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(
        &root,
        "guide.md",
        "# A\n\n## A.1\ntext under a1\n\n## A.2\ntext under a2",
    );
    write_file(&root, "plain.md", "Just plain text without headers.");
    let h = Harness::new();
    let source = MarkdownSource::new(Box::new(MarkdownChunker::new(MarkdownChunkerConfig {
        strategy: ChunkingStrategy::Headers,
        max_chunk_size: 1000,
        overlap_size: 0,
        ..Default::default()
    })));

    let ingester = Ingester::new(
        &h.db,
        &h.cfg,
        &source,
        h.embed.as_ref(),
        Some(&h.ner),
        &h.resolver,
        h.sink.as_ref(),
    );
    let stats = ingester.ingest(&root, false).unwrap();
    assert_eq!(stats.documents_created, 2, "{stats:?}");
    assert_eq!(stats.chunks_created, 3, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    // `list` orders by created_at (second resolution), so the documents are
    // matched by path, not position.
    let (guide_chunks, plain_chunks) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let chunk_dao = ChunkDao::new(exec);
            let mut guide = Vec::new();
            let mut plain = Vec::new();
            for doc in docs.list().unwrap() {
                if doc.original_path.ends_with("guide.md") {
                    guide = chunk_dao.list_by_doc_id(doc.id).unwrap();
                } else if doc.original_path.ends_with("plain.md") {
                    plain = chunk_dao.list_by_doc_id(doc.id).unwrap();
                }
            }
            (guide, plain)
        })
        .unwrap();

    // The sectioned document: both chunks persist the bag with the
    // breadcrumb and section_title keys (criterion 2).
    assert_eq!(guide_chunks.len(), 2);
    for (index, chunk) in guide_chunks.iter().enumerate() {
        let bag: serde_json::Value =
            serde_json::from_str(chunk.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            bag["section_title"],
            serde_json::Value::String(["A.1", "A.2"][index].to_owned()),
            "chunk {index}: section_title key"
        );
        assert_eq!(bag["heading_level"], serde_json::Value::from(2));
        assert_eq!(
            bag["breadcrumb"].as_str(),
            Some(format!("> A\n > {}", ["A.1", "A.2"][index]).as_str()),
            "chunk {index}: breadcrumb key"
        );
    }

    // The headingless document: the single chunk's bag is empty → NULL
    // (criterion 3).
    assert_eq!(plain_chunks.len(), 1);
    assert_eq!(
        plain_chunks[0].metadata_json, None,
        "an empty bag must store NULL"
    );
}

#[test]
fn ingest_rejects_a_file_root() {
    let dir = TempDir::new();
    let file = dir.0.join("a.txt");
    write_file(&dir.0, "a.txt", "content");
    let h = Harness::new();

    let err = h
        .run_with(&file, Some(&h.ner), h.sink.as_ref(), false)
        .unwrap_err();
    match err {
        IngestionError::NotADirectory { path } => assert_eq!(path, file),
        other => panic!("expected NotADirectory, got {other:?}"),
    }
}

#[test]
fn ingest_rejects_a_missing_root() {
    let dir = TempDir::new();
    let missing = dir.0.join("no-such-dir");
    let h = Harness::new();

    let err = h
        .run_with(&missing, Some(&h.ner), h.sink.as_ref(), false)
        .unwrap_err();
    assert!(matches!(err, IngestionError::Io { .. }), "{err:?}");
}

#[test]
fn parse_with_only_errors_fails_the_run() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "broken.txt", "BROKEN content");
    let h = Harness::new();

    let err = h
        .run_with(&root, Some(&h.ner), h.sink.as_ref(), false)
        .unwrap_err();
    match err {
        IngestionError::NoDocumentsParsed { count } => assert_eq!(count, 1),
        other => panic!("expected NoDocumentsParsed, got {other:?}"),
    }
}

#[test]
fn parse_errors_are_counted_alongside_documents() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "broken.txt", "BROKEN content");
    write_file(&root, "good.txt", "line one");
    let h = Harness::new();

    let stats = h.run(&root, false);
    assert_eq!(stats.errors, 1, "{stats:?}");
    assert_eq!(stats.files_processed, 1, "{stats:?}");
    assert_eq!(stats.documents_created, 1, "{stats:?}");
}

// ── Task 3.5: facts ──────────────────────────────────────────────────

#[test]
fn facts_are_stored_with_synthetic_entities_and_quotes() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "Alice works at Acme Corp.");
    let h = Harness::new();
    // NER finds Alice; the fact's object (Acme Corp) is synthetic-only.
    let ner = FactStubNer {
        result: NerResult {
            entities: vec![test_entity("Alice", "person")],
            facts: vec![test_fact(
                "Alice",
                "person",
                "works_at",
                "Acme Corp",
                "organization",
            )],
        },
    };

    let stats = h
        .run_with(&root, Some(&ner), h.sink.as_ref(), false)
        .unwrap();
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.facts_created, 1, "{stats:?}");
    assert_eq!(stats.fact_sources_created, 1, "{stats:?}");
    // Alice (NER) + Acme Corp (synthetic, created by the fact stage).
    assert_eq!(stats.entities_extracted, 2, "{stats:?}");

    let (fact, source, links, doc_id) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let doc = docs.list().unwrap().pop().expect("one document");
            let fact = FactDao::new(exec)
                .list_all()
                .unwrap()
                .pop()
                .expect("one fact");
            let source = FactSourceDao::new(exec)
                .get_by_fact_id(fact.id)
                .unwrap()
                .pop()
                .expect("one source");
            let links = ChunkEntityDao::new(exec).get_entities_by_chunk(1).unwrap();
            (fact, source, links, doc.id)
        })
        .unwrap();

    assert_eq!(fact.predicate, "works_at");
    assert_eq!(fact.domain, "");
    assert_eq!(fact.status, "approved");
    assert_eq!(fact.metadata_json, None, "empty fact metadata → NULL");
    assert_eq!(fact.weight, 1, "recomputed from the single source");

    let (subject, object) =
        h.db.with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap();
            let by_name = |name: &str| {
                entities
                    .iter()
                    .find(|entity| entity.name == name)
                    .expect("entity")
                    .id
            };
            (by_name("Alice"), by_name("Acme Corp"))
        })
        .unwrap();
    assert_eq!(fact.subject_entity_id, Some(subject));
    assert_eq!(fact.object_entity_id, Some(object));

    assert_eq!(source.document_id, doc_id);
    assert_eq!(
        source.quote.as_deref(),
        Some("Alice works at Acme Corp."),
        "the whole chunk fits the quote window"
    );
    assert_eq!(source.extracted_at.len(), 20, "{:?}", source.extracted_at);
    assert!(
        source.extracted_at.ends_with('Z'),
        "{:?}",
        source.extracted_at
    );

    // The chunk is linked to BOTH the NER entity and the synthetic one.
    assert_eq!(links, vec![subject, object]);
}

/// Two facts sharing a subject produce three synthetic entities (Alice,
/// Acme Corp, Bob), not four — endpoints are de-duplicated by
/// (name, type, domain).
#[test]
fn synthetic_endpoints_are_deduplicated_across_facts() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "Alice works at Acme Corp and manages Bob.");
    let h = Harness::new();
    let ner = FactStubNer {
        result: NerResult {
            entities: Vec::new(),
            facts: vec![
                test_fact("Alice", "person", "works_at", "Acme Corp", "organization"),
                test_fact("Alice", "person", "manages", "Bob", "person"),
            ],
        },
    };

    let stats = h
        .run_with(&root, Some(&ner), h.sink.as_ref(), false)
        .unwrap();
    assert_eq!(stats.facts_created, 2, "{stats:?}");
    assert_eq!(stats.fact_sources_created, 2, "{stats:?}");
    assert_eq!(stats.entities_extracted, 3, "{stats:?}");

    let entity_count =
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(entity_count, 3, "Alice, Acme Corp and Bob — once each");
}

/// The same fact in two chunks de-duplicates to ONE `facts` row with two
/// `fact_sources` rows; the weight recompute yields 2.
#[test]
fn duplicate_facts_share_one_row_and_recompute_weights() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(
        &root,
        "a.txt",
        "Alice works at Acme Corp.\nAlice works at Acme Corp.",
    );
    let h = Harness::new();
    let ner = FactStubNer {
        result: NerResult {
            entities: vec![test_entity("Alice", "person")],
            facts: vec![test_fact(
                "Alice",
                "person",
                "works_at",
                "Acme Corp",
                "organization",
            )],
        },
    };

    let stats = h
        .run_with(&root, Some(&ner), h.sink.as_ref(), false)
        .unwrap();
    assert_eq!(stats.facts_created, 2, "{stats:?}");
    assert_eq!(stats.fact_sources_created, 2, "{stats:?}");

    let (fact_count, sources, weight) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let facts = FactDao::new(exec);
            let fact = facts.list_all().unwrap().pop().expect("one fact row");
            let sources = FactSourceDao::new(exec).get_by_fact_id(fact.id).unwrap();
            (facts.count().unwrap(), sources.len(), fact.weight)
        })
        .unwrap();
    assert_eq!(fact_count, 1, "create_or_ignore de-duplicates the row");
    assert_eq!(sources, 2, "one provenance row per chunk");
    assert_eq!(weight, 2, "recomputed from the two sources");
}

/// An empty `NerResult` (no entities, no facts) is a no-op: the document
/// and its chunks are stored, nothing else.
#[test]
fn empty_ner_result_is_a_no_op() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "Alice works at Acme Corp.");
    let h = Harness::new();
    let ner = FactStubNer {
        result: NerResult::default(),
    };

    let stats = h
        .run_with(&root, Some(&ner), h.sink.as_ref(), false)
        .unwrap();
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.chunks_created, 1, "{stats:?}");
    assert_eq!(stats.entities_extracted, 0, "{stats:?}");
    assert_eq!(stats.facts_created, 0, "{stats:?}");
}

/// An entities-only result (the default `StubNer`) never runs the facts
/// stage.
#[test]
fn entities_only_result_skips_the_facts_stage() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one");
    let h = Harness::new();

    let stats = h.run(&root, false);
    assert_eq!(stats.entities_extracted, 1, "{stats:?}");
    assert_eq!(stats.facts_created, 0, "{stats:?}");
    assert_eq!(stats.fact_sources_created, 0, "{stats:?}");

    let fact_count =
        h.db.with_conn(|conn| {
            FactDao::new(ConnectionOrTx::Connection(conn))
                .count()
                .unwrap()
        })
        .unwrap();
    assert_eq!(fact_count, 0);
}

// ── Task 3.6: backup + rebuild ────────────────────────────────────────

/// A rebuild deletes the source's documents BEFORE parsing, so the
/// re-ingest creates them fresh (not as updates) with fresh chunk rows.
#[test]
fn rebuild_clears_the_source_documents_before_parsing() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let h = Harness::new();

    let first = h.run(&root, false);
    assert_eq!(first.documents_created, 1, "{first:?}");

    let second = h.run(&root, true);
    assert_eq!(second.documents_created, 1, "{second:?}");
    assert_eq!(second.documents_updated, 0, "{second:?}");
    assert_eq!(second.documents_skipped, 0, "{second:?}");
    assert_eq!(second.chunks_created, 2, "{second:?}");
    assert_eq!(second.errors, 0, "{second:?}");

    let (doc_count, chunk_count) =
        h.db.with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let docs = DocumentDao::new(exec);
            let docs = docs.list().unwrap();
            let chunk_count = ChunkDao::new(exec)
                .list_by_doc_id(docs[0].id)
                .unwrap()
                .len();
            (docs.len(), chunk_count)
        })
        .unwrap();
    assert_eq!(doc_count, 1, "exactly one document after the rebuild");
    assert_eq!(chunk_count, 2, "fresh chunk rows, old ones cascade-deleted");
}

/// A rebuild clears only documents under the source root: a sibling
/// directory whose name merely starts with the root's must survive
/// (the old string-prefix match would have deleted it).
#[test]
fn rebuild_leaves_documents_outside_the_root() {
    let dir = TempDir::new();
    let root = dir.0.join("docs");
    let sibling = dir.0.join("docs2");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    write_file(&root, "a.txt", "line one");
    let h = Harness::new();

    // Seed a sibling document directly (the pipeline only walks `root`).
    let sibling_path = sibling.join("b.txt").to_string_lossy().into_owned();
    h.db.exec_tx(|tx| -> Result<(), IngestionError> {
        let docs = DocumentDao::new(ConnectionOrTx::Transaction(&*tx));
        docs.create("test", &sibling_path, None, Some("hash"))?;
        Ok(())
    })
    .unwrap();

    let stats = h.run(&root, true);
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    let paths: HashSet<String> =
        h.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .into_iter()
                .map(|doc| doc.original_path)
                .collect()
        })
        .unwrap();
    assert_eq!(
        paths,
        HashSet::from([
            root.join("a.txt").to_string_lossy().into_owned(),
            sibling_path,
        ]),
        "the root's document was re-created, the sibling survived"
    );
}

/// A rebuild on an empty database is a no-op clear: the run proceeds
/// and creates the documents.
#[test]
fn rebuild_with_no_matching_documents_is_a_noop() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one");
    let h = Harness::new();

    let stats = h.run(&root, true);
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.chunks_created, 1, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");
}

/// Two consecutive rebuilds converge to exactly one document.
#[test]
fn rebuild_is_idempotent() {
    let dir = TempDir::new();
    let root = dir.0.clone();
    write_file(&root, "a.txt", "line one\nline two");
    let h = Harness::new();

    let first = h.run(&root, true);
    assert_eq!(first.documents_created, 1, "{first:?}");
    let second = h.run(&root, true);
    assert_eq!(second.documents_created, 1, "{second:?}");
    assert_eq!(second.errors, 0, "{second:?}");

    let doc_count =
        h.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn))
                .list()
                .unwrap()
                .len()
        })
        .unwrap();
    assert_eq!(doc_count, 1, "exactly one document after two rebuilds");
}
