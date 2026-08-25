//! Per-document ingestion pipeline (series change 3, design D2/D3).
//!
//! Oracle reference: `internal/ingestion/ingester.go` (`Ingest`,
//! `processDocument`, `generateEmbeddings`, `extractNerForChunks`,
//! `storeDocument`, `storeChunks`). Re-architected for Rust per the
//! no-1:1-copy directive:
//!
//! - **Collaborator injection (design D3):** [`Ingester::new`] takes the
//!   database handle, the ingestion config, one [`Source`] (parser + chunker
//!   fused), the [`EmbeddingProvider`], an optional [`NerProvider`], the
//!   entity [`Resolver`] and a [`VectorSink`] by reference. The oracle's
//!   constructor built DAOs, a transaction manager, the resolver and the
//!   logger internally; here the CLI/runner change owns all resource wiring,
//!   and tests inject mocks.
//! - **Vectors after commit (design D5):** the oracle stored its vec0 rows
//!   inside the SQLite transaction. Our vectors live in a separate index
//!   engine, so the chunk rows commit first and
//!   [`VectorSink::insert_batch`] runs after the commit. A failure there
//!   leaves vector-less chunks; orphan reconciliation (task 3.8) repairs the
//!   divergence — the chunk row is the source of truth.
//! - **No logger collaborator:** the oracle's structured logger is replaced
//!   by terse `eprintln!` warnings (stderr is reserved for non-MCP traffic,
//!   like the progress bar); a logging crate is not in the frozen stack.
//! - **Redundant second document update dropped:** the oracle re-updated the
//!   document row at the end of the transaction with values it had already
//!   written in `storeDocument`; one write is enough.
//! - **Facts (task 3.5):** NER results are held per chunk (the parallel
//!   `Vec<Option<NerResult>>` of [`Ingester::extract_ner`]); the fact half of
//!   the oracle's `storeEntities` (synthetic endpoint entities, `facts` rows,
//!   `fact_sources` with quotes, weight recompute) runs in
//!   [`facts::store_facts`] inside the same transaction.
//!
//! The pipeline's pure helpers (content hashing, quote extraction (design
//! D7), source-type resolution) live in [`helpers`] (task 3.3).

mod facts;
mod helpers;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use config::preset::IngestionConfig;
use db::{ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, DocumentDao, GcDao};
use embedding::EmbeddingProvider;
use vectors::VectorIndex;

use crate::entities::Resolver;
use crate::error::IngestionError;
use crate::ner::{NerProvider, NerResult};
use crate::parsers::walk_matched_files;
use crate::progress::{ProgressStats, ProgressTracker};
use crate::types::{Document, DocumentChunk, DocumentMetadata, Source};

pub use helpers::{compute_content_hash, extract_quote_from_chunk, source_type_from_metadata};

/// Default embedding batch size when the config declares none (oracle
/// parity: `if batchSize <= 0 { batchSize = 100 }`).
const DEFAULT_BATCH_SIZE: usize = 100;

/// The vector-index write seam of the ingestion pipeline (design D5).
///
/// The ingester only needs the post-commit write half of the vectors
/// engine's contract; a narrow trait keeps that dependency explicit and lets
/// tests record or fail writes without a LanceDB engine. The blanket
/// implementation makes every [`VectorIndex`] (notably
/// `vectors::LanceEngine`, behind `&` or `Arc`) a sink directly.
pub trait VectorSink: Send + Sync {
    /// Stores one vector per chunk after the SQLite transaction has
    /// committed (design D5). `chunk_id` is the SQLite chunk row id.
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), IngestionError>;
}

impl<T: VectorIndex + ?Sized> VectorSink for T {
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), IngestionError> {
        VectorIndex::insert_batch(self, rows).map_err(IngestionError::from)
    }
}

/// The per-document ingestion pipeline: parse → dedup → chunk → embed → NER
/// → one SQLite transaction → vector writes (design D2/D5).
///
/// All collaborators are injected by reference (design D3). Construction
/// cannot fail, so [`Self::new`] returns `Self` — a deliberate deviation
/// from the design sketch's `Result<Self>`: there is no fallible step.
pub struct Ingester<'a> {
    db: &'a Db,
    cfg: &'a IngestionConfig,
    source: &'a dyn Source,
    embed: &'a dyn EmbeddingProvider,
    ner: Option<&'a dyn NerProvider>,
    resolver: &'a Resolver,
    vectors: &'a dyn VectorSink,
}

impl<'a> Ingester<'a> {
    /// Wraps the injected collaborators (design D3).
    pub fn new(
        db: &'a Db,
        cfg: &'a IngestionConfig,
        source: &'a dyn Source,
        embed: &'a dyn EmbeddingProvider,
        ner: Option<&'a dyn NerProvider>,
        resolver: &'a Resolver,
        vectors: &'a dyn VectorSink,
    ) -> Self {
        Self {
            db,
            cfg,
            source,
            embed,
            ner,
            resolver,
            vectors,
        }
    }

    /// Runs the full pipeline over the source directory.
    ///
    /// Flow (oracle `Ingest`): validate the root is a directory → count
    /// files for the progress bar → backup hook (task 3.6) → rebuild-clear
    /// hook (task 3.6, when `rebuild`) → parse → per-document pipeline.
    /// Parse errors count into the stats; a parse that produced nothing but
    /// errors fails the run (oracle parity). Per-document failures count
    /// into [`ProgressStats::errors`] and never abort the run (design D8).
    ///
    /// # Errors
    ///
    /// [`IngestionError::Io`] when the root cannot be stat'ed,
    /// [`IngestionError::NotADirectory`] when it is a file,
    /// [`IngestionError::NoDocumentsParsed`] when parsing found nothing but
    /// errors. Document-level failures are counted, not returned.
    pub fn ingest(
        &self,
        source_path: &Path,
        rebuild: bool,
    ) -> Result<ProgressStats, IngestionError> {
        let meta = fs::metadata(source_path).map_err(|source| IngestionError::Io {
            path: source_path.to_path_buf(),
            source,
        })?;
        if !meta.is_dir() {
            return Err(IngestionError::NotADirectory {
                path: source_path.to_path_buf(),
            });
        }

        let total_files = self.count_files(source_path);
        let mut tracker = ProgressTracker::new(
            total_files,
            &format!("Ingesting from {}", source_path.display()),
        );

        // TODO(task 3.6): create_backup() before the first write (design
        // D6): VACUUM INTO snapshot, warn-only on failure.
        if rebuild {
            // TODO(task 3.6): clear_source_data(source_path) before parsing
            // (design D6): all documents under the source root, one
            // transaction.
        }

        let parse_result = self.source.parse(source_path);
        for _ in &parse_result.errors {
            tracker.increment_errors();
        }
        if parse_result.documents.is_empty() && !parse_result.errors.is_empty() {
            return Err(IngestionError::NoDocumentsParsed {
                count: parse_result.errors.len(),
            });
        }

        for (index, doc) in parse_result.documents.iter().enumerate() {
            if let Err(err) = self.process_document(doc, &mut tracker) {
                tracker.increment_errors();
                eprintln!(
                    "warning: document {}/{} {} failed: {err}",
                    index + 1,
                    parse_result.documents.len(),
                    doc.source_path.display()
                );
            } else {
                tracker.increment_files();
            }
        }

        tracker.finish();
        Ok(tracker.stats())
    }

    /// The per-document pipeline (oracle `processDocument`): hash-dedup →
    /// chunk → batched embeddings → per-chunk NER → one transaction
    /// (document + chunks + entities + facts) → post-commit vector writes
    /// (design D5).
    ///
    /// # Errors
    ///
    /// Chunker, embedding, NER, storage and vector-index errors. The caller
    /// counts the failure and continues with the next document.
    fn process_document(
        &self,
        doc: &Document,
        tracker: &mut ProgressTracker,
    ) -> Result<(), IngestionError> {
        let path = doc.source_path.to_string_lossy().into_owned();
        let content_hash = compute_content_hash(&doc.content);

        // Read-only dedup lookup outside the transaction (oracle parity).
        let existing_doc = self.db.with_conn(|conn| {
            DocumentDao::new(ConnectionOrTx::Connection(conn)).get_by_path(&path)
        })??;
        if existing_doc
            .as_ref()
            .is_some_and(|existing| existing.content_hash.as_deref() == Some(content_hash.as_str()))
        {
            tracker.increment_documents_skipped();
            return Ok(());
        }

        let chunks = self.source.chunk(&doc.content, &doc.metadata)?;
        if chunks.is_empty() {
            // Empty document (oracle: warn + skip): nothing to index.
            eprintln!(
                "warning: empty document, skip {}",
                doc.source_path.display()
            );
            tracker.increment_documents_skipped();
            return Ok(());
        }

        let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
        let vectors = self.generate_embeddings(&texts, tracker)?;
        let ner_results = self.extract_ner(&chunks)?;

        // The persisted metadata is the extension bag: the typed fields
        // already live in dedicated columns, and the domain filter (task
        // 3.7) reads `$.domain` from this bag.
        let metadata_json = serde_json::to_string(&doc.metadata.extra)
            .map_err(|source| IngestionError::MetadataJson { source })?;
        let source_type = Self::document_source_type(&doc.metadata);

        // One transaction for all of this document's SQLite writes
        // (design D2): the DAOs, the GC and the resolver share the same
        // transaction handle, so entity resolution never hits the SQLite
        // write lock from a second connection (oracle rationale).
        let chunk_ids = self.db.exec_tx(|tx| -> Result<Vec<i64>, IngestionError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let docs = DocumentDao::new(exec);
            let chunk_dao = ChunkDao::new(exec);
            let links = ChunkEntityDao::new(exec);
            let gc = GcDao::new(exec);

            let (doc_id, is_new) = match &existing_doc {
                Some(existing) => {
                    docs.update(
                        existing.id,
                        &path,
                        Some(&metadata_json),
                        Some(&content_hash),
                    )?;
                    gc.full_clear_doc_by_id(existing.id)?;
                    (existing.id, false)
                }
                None => {
                    let doc_id = docs.create(
                        source_type,
                        &path,
                        Some(&metadata_json),
                        Some(&content_hash),
                    )?;
                    (doc_id, true)
                }
            };

            let mut chunk_ids = Vec::with_capacity(chunks.len());
            for (chunk, ner_result) in chunks.iter().zip(ner_results.iter()) {
                let chunk_id = chunk_dao.create(
                    doc_id,
                    &chunk.text,
                    chunk.sequence_num as i64,
                    // Byte offsets of a file-sized document cannot reach the
                    // i64 boundary; the truncation is unreachable.
                    Some(chunk.start_offset as i64),
                    Some(chunk.end_offset as i64),
                )?;
                chunk_ids.push(chunk_id);

                if let Some(ner_result) = ner_result {
                    if !ner_result.entities.is_empty() {
                        let resolved =
                            self.resolver
                                .add_entities(exec, doc_id, &ner_result.entities)?;
                        tracker.add_entities(resolved.len() as u64);
                        for entity in &resolved {
                            links.link(chunk_id, entity.id)?;
                        }
                    }
                    // Task 3.5: the fact half of the oracle's `storeEntities`
                    // (no-op when the chunk has no facts).
                    facts::store_facts(
                        exec,
                        self.resolver,
                        tracker,
                        doc_id,
                        chunk_id,
                        &chunk.text,
                        &ner_result.facts,
                        &path,
                        chunk.sequence_num,
                    )?;
                }
            }

            tracker.add_chunks(chunks.len() as u64);
            if is_new {
                tracker.increment_documents_created();
            } else {
                tracker.increment_documents_updated();
            }
            Ok(chunk_ids)
        })?;

        // Design D5: vectors are written after the commit; the chunk row is
        // the source of truth, and a failure here is repaired by orphan
        // reconciliation (task 3.8).
        let rows: Vec<(u32, &[f32])> = chunk_ids
            .iter()
            .enumerate()
            .map(|(index, &chunk_id)| (chunk_id as u32, vectors[index].as_slice()))
            .collect();
        self.vectors.insert_batch(&rows)?;

        Ok(())
    }

    /// Generates the chunk embeddings in config-sized batches (oracle
    /// `generateEmbeddings`). A provider that returns a different vector
    /// count than text count is a hard error: continuing would misalign
    /// every later vector.
    fn generate_embeddings(
        &self,
        texts: &[String],
        tracker: &mut ProgressTracker,
    ) -> Result<Vec<Vec<f32>>, IngestionError> {
        let batch_size = if self.cfg.batch_size > 0 {
            self.cfg.batch_size as usize
        } else {
            DEFAULT_BATCH_SIZE
        };
        let total_batches = texts.len().div_ceil(batch_size);
        let mut all_vectors = Vec::with_capacity(texts.len());
        for (batch_number, batch) in texts.chunks(batch_size).enumerate() {
            let vectors = self.embed.generate_embeddings(batch)?;
            if vectors.len() != batch.len() {
                return Err(IngestionError::EmbeddingCountMismatch {
                    batch: batch_number + 1,
                    total_batches,
                    expected: batch.len(),
                    actual: vectors.len(),
                });
            }
            all_vectors.extend(vectors);
        }
        tracker.add_embeddings(all_vectors.len() as u64);
        Ok(all_vectors)
    }

    /// Runs the NER stage over every chunk (oracle `extractNerForChunks`).
    ///
    /// The results are held in a `Vec<Option<NerResult>>` parallel to the
    /// chunks (design D2 of the sources change): chunks stay pure chunking
    /// artifacts and the NER layer attaches its results through its own
    /// structure. `None` means "nothing found" (the provider's `Ok(None)`).
    fn extract_ner(
        &self,
        chunks: &[DocumentChunk],
    ) -> Result<Vec<Option<NerResult>>, IngestionError> {
        let Some(ner) = self.ner else {
            return Ok(vec![None; chunks.len()]);
        };
        if self.cfg.ner.disabled {
            return Ok(vec![None; chunks.len()]);
        }
        chunks
            .iter()
            .map(|chunk| ner.extract_entities(&chunk.text, &chunk.metadata.extra))
            .collect()
    }

    /// The `source_type` column value for a document: the typed
    /// [`DocumentMetadata::source_type`] field (the re-architecture moved it
    /// out of the free-form map; the oracle's `getSourceType` read the same
    /// logical key). Empty → `"unknown"`.
    fn document_source_type(metadata: &DocumentMetadata) -> &str {
        if metadata.source_type.is_empty() {
            "unknown"
        } else {
            &metadata.source_type
        }
    }

    /// Counts the processable files under `source_path` (the progress bar
    /// total; oracle `countFiles`). Count errors are collected, not
    /// returned: they surface again — and louder — in the parse stage.
    fn count_files(&self, source_path: &Path) -> u64 {
        let extensions: HashSet<String> = self
            .source
            .supported_extensions()
            .iter()
            .map(|ext| ext.to_ascii_lowercase())
            .collect();
        let mut total = 0u64;
        let mut errors = Vec::new();
        walk_matched_files(
            source_path,
            |path| {
                path.extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|ext| extensions.contains(&ext.to_ascii_lowercase()))
            },
            |_path| {
                total += 1;
                Ok(())
            },
            &mut errors,
        );
        total
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use db::test_util::in_memory_db;
    use db::{
        ChunkDao, ChunkEntityDao, ConnectionOrTx, DocumentDao, EntityDao, FactDao, FactSourceDao,
    };
    use embedding::EmbeddingError;
    use serde_json::Map;

    use super::*;
    use crate::ner::{NerEntity, NerFact, NerResult};
    use crate::types::{Chunker, DocumentMetadata, ParseResult, Parser};

    /// An in-memory test source: `parse` reads every `.txt` file of the
    /// root (content = file content, `BROKEN`-prefixed content simulates a
    /// parse failure); `chunk` splits on non-empty lines (one chunk per
    /// line, byte offsets into the content).
    struct TestSource;

    impl Parser for TestSource {
        fn parse(&self, source_path: &Path) -> ParseResult {
            let mut documents = Vec::new();
            let mut errors = Vec::new();
            // Paths whose content simulates a parse failure (converted to
            // errors after the walk: the visit closure may not touch the
            // `errors` vector the walk helper owns).
            let mut broken = Vec::new();
            walk_matched_files(
                source_path,
                |path| path.extension().is_some_and(|ext| ext == "txt"),
                |path| {
                    let content =
                        fs::read_to_string(path).map_err(|source| IngestionError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    if content.starts_with("BROKEN") {
                        broken.push(path.to_path_buf());
                        return Ok(());
                    }
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
            for path in broken {
                errors.push(IngestionError::Io {
                    path,
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "simulated parse failure",
                    ),
                });
            }
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
            let mut offset = 0;
            for line in content.lines() {
                if !line.trim().is_empty() {
                    chunks.push(DocumentChunk {
                        doc_id: None,
                        text: line.to_owned(),
                        sequence_num: chunks.len(),
                        start_offset: offset,
                        end_offset: offset + line.len(),
                        metadata: metadata.clone(),
                    });
                }
                offset += line.len() + 1;
            }
            Ok(chunks)
        }
    }

    impl Source for TestSource {}

    /// A deterministic embedding provider: the i-th vector of a batch is
    /// all `(i + 1)`. `mismatch` makes it return one vector short (the
    /// count-mismatch error path); `fail_marker` makes it error on any batch
    /// containing a text with that substring (per-document failure path).
    /// `calls` records each batch size.
    struct MockEmbedding {
        dim: usize,
        mismatch: Mutex<bool>,
        fail_marker: Mutex<Option<String>>,
        calls: Mutex<Vec<usize>>,
    }

    impl MockEmbedding {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                mismatch: Mutex::new(false),
                fail_marker: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
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
}
