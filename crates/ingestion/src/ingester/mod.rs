//! Per-document ingestion pipeline (series change 3, design D2/D3).
//!
//! Design decisions:
//!
//! - **Collaborator injection (design D3):** [`Ingester::new`] takes the
//!   database handle, the ingestion config, one [`Source`] (parser + chunker
//!   fused), the [`EmbeddingProvider`], an optional [`NerProvider`], the
//!   entity [`Resolver`] and a [`VectorSink`] by reference. The CLI/runner
//!   change owns all resource wiring, and tests inject mocks.
//! - **Vectors after commit (design D5):** vectors live in a separate index
//!   engine, so the chunk rows commit first and
//!   [`VectorSink::insert_batch`] runs after the commit. A failure there
//!   leaves vector-less chunks; orphan reconciliation (task 3.8) repairs the
//!   divergence — the chunk row is the source of truth.
//! - **No logger collaborator:** the ingester takes no logger (design D3);
//!   `tracing` calls are owned by the serve layer's subscriber. These replace
//!   the crate's former `eprintln!` warnings (document-jobs-queue task 1.8).
//! - **Single document write:** the document row is written once per
//!   transaction; a redundant second update at the end of the transaction is
//!   dropped — one write is enough.
//! - **Facts (task 3.5):** NER results are held per chunk (the parallel
//!   `Vec<Option<NerResult>>` of the private `extract_ner` stage); the fact
//!   half of entity storage (synthetic endpoint entities, `facts` rows,
//!   `fact_sources` with quotes, weight recompute) runs in the private
//!   `store_facts` stage inside the same transaction.
//!
//! The pipeline's pure helpers (content hashing, quote extraction (design
//! D7), source-type resolution) live in the private `helpers` module
//! (task 3.3).

mod backup;
mod facts;
mod helpers;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use config::preset::IngestionConfig;
use db::{
    ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, DocumentDao, EntityLinkPayload, GcDao,
    QueueTaskDao, QueueTaskType,
};
use embedding::EmbeddingProvider;
use serde_json::{Map, Value};
use vectors::VectorIndex;

use crate::entities::{EntityChanges, Resolver};
use crate::error::IngestionError;
use crate::job_queue::now_unix_seconds;
use crate::ner::{NerProvider, NerResult};
use crate::parsers::walk_matched_files;
use crate::progress::{ProgressStats, ProgressTracker};
use crate::types::{Document, DocumentChunk, DocumentMetadata, Source};

pub use helpers::{compute_content_hash, extract_quote_from_chunk, source_type_from_metadata};

/// Default embedding batch size when the config declares none
/// (`batch_size <= 0` → 100).
const DEFAULT_BATCH_SIZE: usize = 100;

/// The vector-index write seam of the ingestion pipeline (design D5).
///
/// The ingester only needs the post-commit write half of the vectors
/// engine's contract; a narrow trait keeps that dependency explicit and lets
/// tests record or fail writes without a vector engine. The blanket
/// implementation makes every [`VectorIndex`] (behind `&` or `Arc`) a sink
/// directly.
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
    /// Flow: validate the root is a directory → count files for the progress
    /// bar → database backup (design D6) → rebuild-clear (design D6, when
    /// `rebuild`) → parse → per-document pipeline. Parse errors count into
    /// the stats; a parse that produced nothing but errors fails the run.
    /// Per-document failures count into [`ProgressStats::errors`] and never
    /// abort the run (design D8).
    ///
    /// # Errors
    ///
    /// [`IngestionError::Io`] when the root cannot be stat'ed,
    /// [`IngestionError::NotADirectory`] when it is a file,
    /// [`IngestionError::NoDocumentsParsed`] when parsing found nothing but
    /// errors. A database that cannot even be read (backup stage) or a
    /// rebuild-clear failure also propagates (design D8: DB failure fails
    /// the source); a backup snapshot failure is a warning, not an error.
    /// Document-level failures are counted, not returned.
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

        // Design D6: snapshot the database before the first destructive
        // write. A failed snapshot is a warning inside `create_backup`
        // (returns `false`); only an unreadable database propagates.
        backup::create_backup(self.db)?;
        if rebuild {
            // Design D6: a rebuild clears every document under the source
            // root BEFORE parsing, in one transaction.
            let cleared = backup::clear_source_data(self.db, source_path)?;
            if cleared > 0 {
                tracing::info!(
                    cleared,
                    source = %source_path.display(),
                    "rebuild: cleared documents under source root"
                );
            }
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
                tracing::error!(
                    doc = %doc.source_path.display(),
                    index = index + 1,
                    total = parse_result.documents.len(),
                    error = %err,
                    "document failed"
                );
            } else {
                tracker.increment_files();
            }
        }

        tracker.finish();
        Ok(tracker.stats())
    }

    /// The per-document pipeline: hash-dedup →
    /// chunk → batched embeddings → per-chunk NER → one transaction
    /// (document + chunks + entities + facts) → post-commit vector writes
    /// (design D5).
    ///
    /// `pub(crate)`: the ingest loop (above) and the queue worker
    /// (document-jobs-queue task 1.4, `runner::Runner::process_document_by_path`)
    /// both run this unchanged pipeline for a single document.
    ///
    /// # Errors
    ///
    /// Chunker, embedding, NER, storage and vector-index errors. The caller
    /// counts the failure and continues with the next document.
    pub(crate) fn process_document(
        &self,
        doc: &Document,
        tracker: &mut ProgressTracker,
    ) -> Result<(), IngestionError> {
        tracing::info!("process document {:?}", doc.source_path.display());

        let path = doc.source_path.to_string_lossy().into_owned();
        let content_hash = compute_content_hash(&doc.content);

        // Read-only dedup lookup outside the transaction.
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
            // Empty document (warn + skip): nothing to index.
            tracing::warn!(
                doc = %doc.source_path.display(),
                "empty document, skipping"
            );
            tracker.increment_documents_skipped();
            return Ok(());
        }

        // The embedding leg operates on `search_text` (breadcrumb + body for
        // sectioned chunks, the text otherwise) — search-text-embedding design
        // D3. NER below still runs on the pure `text`.
        let texts: Vec<String> = chunks
            .iter()
            .map(|chunk| chunk.search_text.clone())
            .collect();
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
        // write lock from a second connection.
        let (chunk_ids, doc_id, entity_changes) = self.db.exec_tx(
            |tx| -> Result<(Vec<i64>, i64, EntityChanges), IngestionError> {
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
                let mut changes = EntityChanges::default();
                for (chunk, ner_result) in chunks.iter().zip(ner_results.iter()) {
                    // Persist both texts (search-text-embedding task 2.1): the
                    // pure-slice `chunk_text` (byte-offset invariant) and the
                    // `search_text` the FTS5 index and the embedding leg used,
                    // plus the chunk's own metadata bag as raw JSON
                    // (chunk-metadata-persistence task 3.1; `NULL` when the bag
                    // is empty).
                    let chunk_metadata = Self::chunk_metadata_json(&chunk.metadata)?;
                    let chunk_id = chunk_dao.create_with_search_text(
                        doc_id,
                        &chunk.text,
                        &chunk.search_text,
                        chunk_metadata.as_deref(),
                        chunk.sequence_num as i64,
                        // Byte offsets of a file-sized document cannot reach the
                        // i64 boundary; the truncation is unreachable.
                        Some(chunk.start_offset as i64),
                        Some(chunk.end_offset as i64),
                    )?;
                    chunk_ids.push(chunk_id);

                    if let Some(ner_result) = ner_result {
                        if !ner_result.entities.is_empty() {
                            let (resolved, chunk_changes) =
                                self.resolver
                                    .add_entities(exec, doc_id, &ner_result.entities)?;
                            tracker.add_entities(resolved.len() as u64);
                            changes.merge(chunk_changes);
                            for entity in &resolved {
                                links.link(chunk_id, entity.id)?;
                            }
                        }
                        // Task 3.5: the fact half of entity storage (no-op when
                        // the chunk has no facts).
                        let fact_changes = facts::store_facts(
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
                        changes.merge(fact_changes);
                    }
                }

                tracker.add_chunks(chunks.len() as u64);
                if is_new {
                    tracker.increment_documents_created();
                } else {
                    tracker.increment_documents_updated();
                }
                Ok((chunk_ids, doc_id, changes))
            },
        )?;

        // Design D5: vectors are written after the commit; the chunk row is
        // the source of truth, and a failure here is repaired by orphan
        // reconciliation (task 3.8).
        let rows: Vec<(u32, &[f32])> = chunk_ids
            .iter()
            .enumerate()
            .map(|(index, &chunk_id)| (chunk_id as u32, vectors[index].as_slice()))
            .collect();
        self.vectors.insert_batch(&rows)?;

        // Design D6: enqueue an `entity:link` event when the change set is
        // non-empty (created or updated entities). The worker processes it
        // with the graph linker's incremental mode.
        if !entity_changes.is_empty() {
            let entity_ids = entity_changes.ids();
            let identity = doc_id.to_string();
            self.db
                .with_conn(|conn| {
                    let exec = ConnectionOrTx::Connection(conn);
                    let dao = QueueTaskDao::new(exec);
                    dao.enqueue(
                        QueueTaskType::EntityLink,
                        &identity,
                        &EntityLinkPayload {
                            entity_ids: entity_ids.clone(),
                        },
                        now_unix_seconds(),
                    )
                })
                .map_err(IngestionError::Db)?
                .map_err(IngestionError::QueueTask)?;
            tracing::info!(
                doc_id,
                count = entity_ids.len(),
                "entity:link event enqueued"
            );
        }

        Ok(())
    }

    /// Generates the chunk embeddings in config-sized batches. A provider
    /// that returns a different vector
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

    /// Runs the NER stage over every chunk.
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
            .map(|chunk| {
                tracing::info!(
                    "extract entities from chunk {:?}/{:?}",
                    chunk.sequence_num,
                    chunks.len()
                );
                ner.extract_entities(&chunk.text, &chunk.metadata)
            })
            .collect()
    }

    /// The raw-JSON form of a chunk's metadata bag for the
    /// `chunks.metadata_json` column (chunk-metadata-persistence task 3.1):
    /// the bag serialized as a JSON object, or `NULL` when the bag is empty
    /// (design D1: `NULL` reads as "no metadata" and is the common case for
    /// chunks without chunk-specific keys).
    fn chunk_metadata_json(
        metadata: &Map<String, Value>,
    ) -> Result<Option<String>, IngestionError> {
        if metadata.is_empty() {
            return Ok(None);
        }
        let json = serde_json::to_string(metadata)
            .map_err(|source| IngestionError::MetadataJson { source })?;
        Ok(Some(json))
    }

    /// The `source_type` column value for a document: the typed
    /// [`DocumentMetadata::source_type`] field (moved out of the free-form
    /// map). Empty → `"unknown"`.
    fn document_source_type(metadata: &DocumentMetadata) -> &str {
        if metadata.source_type.is_empty() {
            "unknown"
        } else {
            &metadata.source_type
        }
    }

    /// Counts the processable files under `source_path` (the progress bar
    /// total). Count errors are collected, not returned: they surface again
    /// — and louder — in the parse stage.
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
