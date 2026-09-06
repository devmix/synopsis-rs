//! End-to-end pipeline test (change `ingestion-pipeline`, task 3.9).
//!
//! Scenario set (ingest → rerun → modify → delete → cleanup): two real
//! sources (markdown + json) over a temp tree, the real [`Runner`] with a
//! mock embedding provider, the real [`RegexNer`] over a temp domain config
//! (the config loader is the only public way to obtain compiled patterns),
//! an in-memory SQLite database and an in-memory vector index. Every DB
//! assertion goes through the db-crate DAOs.
//!
//! The file deliberately uses crate-root re-exports (`ingestion::Runner`,
//! `ingestion::Ingester`, …) plus the queue modules (`ingestion::DocumentJobQueue`,
//! `ingestion::worker::DocumentWorker`): the pipeline's public API surface
//! is part of what this test pins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use config::DomainConfig;
use config::ontology::{GlobalConfig, GlobalNerConfig, NerMethod, SourceConfig, SourceType};
use config::preset::{IngestionConfig, LinkerConfig};
use db::test_util::in_memory_db;
use db::{
    Chunk, ChunkDao, ChunkEntityDao, ConnectionOrTx, Db, Document, DocumentDao, Entity, EntityDao,
    FactDao, FactSourceDao, QueueTaskDao,
};
use embedding::{EmbeddingError, EmbeddingProvider};
use ingestion::worker::DocumentWorker;
use ingestion::{
    DocumentJobQueue, Ingester, IngestionError, JsonChunker, JsonSource, MarkdownChunker,
    MarkdownSource, NerEntity, NerFact, NerPrompts, NerProvider, NerResult, Registry, Resolver,
    Runner, RunnerParams, VectorSink, load_ner_prompts,
};
use serde_json::{Map, Value};
use vectors::{VectorIndex, VectorsError};

/// One markdown document with two heading sections and one email each.
const MD_CONTENT: &str = "# Team\n\nContact alice@example.com for onboarding.\n\n## Policy\n\nBob's email is bob@corp.io.\n";
/// The modified markdown document (one section, a different email).
const MD_CONTENT_V2: &str = "# Team\n\nDave joined the team: dave@example.com.\n";
/// One JSON array document with two text fields; the description carries an
/// email.
const JSON_CONTENT: &str = "[\n  {\"title\": \"Widget\", \"description\": \"A blue widget maintained by carol@example.com\"}\n]\n";

// ── Test collaborators (same stub patterns as the unit tests) ─────────────

/// A deterministic embedding provider: the i-th vector of a batch is all
/// `(i + 1)`.
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

/// In-memory [`VectorIndex`]: records every row (tests assert on it).
struct MemoryIndex {
    rows: Mutex<BTreeMap<u32, Vec<f32>>>,
}

impl MemoryIndex {
    fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
        }
    }

    fn ids(&self) -> Vec<u32> {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
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

/// A [`VectorSink`] that records every row (the Ingester-direct test asserts
/// on the post-commit writes).
struct RecordingSink {
    rows: Mutex<Vec<(u32, Vec<f32>)>>,
}

impl VectorSink for RecordingSink {
    fn insert_batch(&self, rows: &[(u32, &[f32])]) -> Result<(), IngestionError> {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(rows.iter().map(|(id, vector)| (*id, vector.to_vec())));
        Ok(())
    }
}

/// A NER stub returning one entity and one fact for every non-empty chunk
/// (the facts/quotes flow; the runner tests use a stub of the same shape).
struct FactStubNer;

impl NerProvider for FactStubNer {
    fn name(&self) -> &'static str {
        "fact-stub"
    }

    fn extract_entities(
        &self,
        content: &str,
        _metadata: &Map<String, Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        if content.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(NerResult {
            entities: vec![NerEntity {
                name: "Alice".to_owned(),
                entity_type: "person".to_owned(),
                description: String::new(),
                confidence: 1.0,
                domain: String::new(),
                metadata: Map::new(),
            }],
            facts: vec![NerFact {
                subject_type: "person".to_owned(),
                subject_name: "Alice".to_owned(),
                predicate: "works_at".to_owned(),
                object_type: "organization".to_owned(),
                object_name: "Acme Corp".to_owned(),
                domain: String::new(),
                metadata: Map::new(),
            }],
        }))
    }
}

// ── Fixtures ───────────────────────────────────────────────────────────────

/// A unique temp directory, removed on drop (tests run in parallel).
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "synopsis-pipeline-e2e-{prefix}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
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

/// Loads the "hr" domain (one regex rule: email local part → `employee`)
/// through the real config loader — the only public way to obtain compiled
/// patterns (config design D5 compiles at load time).
fn hr_domain() -> DomainConfig {
    let dir = TempDir::new("domain");
    let xml = r#"<domain name="hr" version="1.0"><extraction><regex-rules><regex id="employee_from_email" entity="employee" pattern="([a-zA-Z0-9._%+\-]+)@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}" confidence="0.9"/></regex-rules></extraction></domain>"#;
    let path = dir.0.join("hr.xml");
    fs::write(&path, xml).unwrap();
    config::load_domain_config(&path).unwrap()
}

/// Builds a [`SourceConfig`] for the fixtures.
fn source_config(path: &str, source_type: SourceType, domains: &[&str]) -> SourceConfig {
    SourceConfig {
        path: path.to_owned(),
        source_type,
        disabled: false,
        space: String::new(),
        domains: domains.iter().map(|d| (*d).to_owned()).collect(),
        dataset: String::new(),
    }
}

/// The full test wiring: in-memory DB, real sources + chunkers, a real
/// "hr" domain config, a mock embedding provider and an in-memory vector
/// index.
struct Harness {
    /// Keeps the source tree alive until the harness is dropped (the guard
    /// is never read; `Drop` removes the tree).
    #[allow(dead_code)]
    root: TempDir,
    md_src: PathBuf,
    json_src: PathBuf,
    db: Db,
    cfg: IngestionConfig,
    global: GlobalConfig,
    domains: HashMap<String, DomainConfig>,
    registry: Registry,
    embed: MockEmbedding,
    sink: Arc<MemoryIndex>,
    prompts: NerPrompts,
    linker_cfg: LinkerConfig,
}

impl Harness {
    fn new() -> Self {
        let root = TempDir::new("src");
        let md_src = root.sub("docs");
        let json_src = root.sub("data");
        fs::create_dir_all(&md_src).unwrap();
        fs::create_dir_all(&json_src).unwrap();

        let mut cfg = IngestionConfig::default();
        // A directly constructed config skips the config crate's
        // apply_defaults: set the markdown cap explicitly (the chunker
        // clamps 0 to 1 character).
        cfg.chunking.markdown.max_chunk_size = 10_000;

        let mut registry = Registry::new();
        registry
            .register(
                "markdown",
                Box::new(MarkdownSource::new(Box::new(MarkdownChunker::new(
                    cfg.chunking.markdown.clone(),
                )))),
            )
            .unwrap();
        registry
            .register(
                "json",
                Box::new(JsonSource::new(Box::new(JsonChunker::new(
                    cfg.chunking.json.clone(),
                )))),
            )
            .unwrap();

        let global = GlobalConfig {
            sources: vec![
                source_config(
                    md_src.to_string_lossy().as_ref(),
                    SourceType::Markdown,
                    &["hr"],
                ),
                // No `json` variant in the strict enum: the tolerant word
                // passes through to the registry unchanged.
                source_config(
                    json_src.to_string_lossy().as_ref(),
                    SourceType::Unknown("json".to_owned()),
                    &["hr"],
                ),
            ],
            cross_domain_links: None,
            ner: GlobalNerConfig {
                methods: vec![NerMethod::Regex],
            },
            entities: Vec::new(),
            relations: Vec::new(),
            extraction: Default::default(),
        };

        Self {
            root,
            md_src,
            json_src,
            db: in_memory_db(),
            cfg,
            global,
            domains: HashMap::from([("hr".to_owned(), hr_domain())]),
            registry,
            embed: MockEmbedding { dim: 8 },
            sink: Arc::new(MemoryIndex::new()),
            prompts: load_ner_prompts("/nonexistent-ner-prompts").unwrap(),
            linker_cfg: LinkerConfig::default(),
        }
    }

    /// The real runner over the wired collaborators.
    fn runner(&self) -> Runner<'_> {
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

    /// Writes the fixture files into the source trees.
    fn seed(&self) {
        fs::write(self.md_src.join("team.md"), MD_CONTENT).unwrap();
        fs::write(self.json_src.join("widgets.json"), JSON_CONTENT).unwrap();
    }

    // ── DAO-backed read helpers ──────────────────────────────────────────

    fn docs(&self) -> Vec<Document> {
        self.db
            .with_conn(|conn| DocumentDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap()
    }

    fn doc_by_suffix(&self, suffix: &str) -> Document {
        self.docs()
            .into_iter()
            .find(|doc| doc.original_path.ends_with(suffix))
            .unwrap_or_else(|| panic!("no document ending in {suffix:?}"))
    }

    fn all_chunks(&self) -> Vec<Chunk> {
        self.db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).list_all())
            .unwrap()
            .unwrap()
    }

    fn chunks_of(&self, doc_id: i64) -> Vec<Chunk> {
        self.db
            .with_conn(|conn| {
                ChunkDao::new(ConnectionOrTx::Connection(conn)).list_by_doc_id(doc_id)
            })
            .unwrap()
            .unwrap()
    }

    fn entities(&self) -> Vec<Entity> {
        self.db
            .with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())
            .unwrap()
            .unwrap()
    }

    fn fact_count(&self) -> i64 {
        self.db
            .with_conn(|conn| FactDao::new(ConnectionOrTx::Connection(conn)).count())
            .unwrap()
            .unwrap()
    }

    fn fact_source_count(&self) -> i64 {
        self.db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM fact_sources", [], |row| row.get(0))
            })
            .unwrap()
            .unwrap()
    }

    fn jobs(&self) -> Vec<db::QueueTask> {
        self.db
            .with_conn(|conn| QueueTaskDao::new(ConnectionOrTx::Connection(conn)).list(None, None))
            .expect("with_conn")
            .expect("list tasks")
    }

    /// The queue/worker path that replaced the direct whole-tree run:
    /// reconcile every configured source (new/changed files → index jobs)
    /// and let one worker cycle process them.
    fn reconcile_and_process(&self, runner: &Runner<'_>, now: i64) {
        let queue = DocumentJobQueue::new(&self.db);
        for src in self.global.sources.iter().filter(|src| !src.disabled) {
            queue
                .reconcile_source(runner, &src.path)
                .unwrap_or_else(|err| panic!("reconcile {} failed: {err}", src.path));
        }
        let worker = DocumentWorker::new(&self.db, runner);
        worker
            .run_once(now)
            .unwrap_or_else(|err| panic!("worker cycle failed: {err}"));
    }
}

/// Asserts the sorted entity-name set equals `expected` (sorted).
fn assert_entity_names(h: &Harness, expected: &[&str]) {
    let mut names: Vec<String> = h.entities().into_iter().map(|e| e.name).collect();
    names.sort();
    let mut want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(names, want, "entity names");
}

/// Asserts the vector index holds exactly the live chunk ids.
fn assert_vectors_match_live_chunks(h: &Harness) {
    let mut stored = h.sink.ids();
    stored.sort();
    let mut live: Vec<u32> = h.all_chunks().iter().map(|c| c.id as u32).collect();
    live.sort();
    assert_eq!(stored, live, "vector index must mirror the chunk rows");
}

// ── Scenarios ──────────────────────────────────────────────────────────────

/// First run: both sources ingest through the queue/worker; documents,
/// chunks, entities, chunk provenance and vectors all land in the database
/// (facts stay empty: the regex stage produces no facts).
#[test]
fn first_run_populates_documents_chunks_entities_and_vectors() {
    let h = Harness::new();
    h.seed();
    let runner = h.runner();

    h.reconcile_and_process(&runner, i64::MAX / 2);

    // Both sources' jobs were processed (two new files → two index jobs).
    let jobs = h.jobs();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs.iter().all(|job| job.status == "done"), "{jobs:?}");

    // Documents: one per file, correct source types, the domain stamp in
    // the stored metadata.
    let docs = h.docs();
    assert_eq!(docs.len(), 2, "{docs:?}");
    let md = h.doc_by_suffix("team.md");
    let js = h.doc_by_suffix("widgets.json");
    assert_eq!(md.source_type, "markdown");
    assert_eq!(js.source_type, "json");
    assert_eq!(md.metadata_json.as_deref(), Some(r#"{"domain":["hr"]}"#));
    assert!(
        js.metadata_json
            .as_deref()
            .is_some_and(|m| m.contains("hr")),
        "{:?}",
        js.metadata_json
    );
    assert!(md.content_hash.is_some() && js.content_hash.is_some());

    // Chunks: two markdown heading sections + two JSON text fields.
    let chunks = h.all_chunks();
    assert_eq!(chunks.len(), 4, "{chunks:?}");
    assert_eq!(h.chunks_of(md.id).len(), 2);
    assert_eq!(h.chunks_of(js.id).len(), 2);

    // Entities: the three emails, one row each (rule type + domain).
    let entities = h.entities();
    assert_eq!(entities.len(), 3, "{entities:?}");
    assert_entity_names(&h, &["alice", "bob", "carol"]);
    for entity in &entities {
        assert_eq!(entity.entity_type, "employee", "{entity:?}");
        assert_eq!(entity.domain, "hr", "{entity:?}");
    }

    // Provenance: each chunk is linked to exactly the entities in its text.
    let links = h.db.with_conn(|conn| {
        let links = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
        chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.id,
                    chunk.chunk_text.clone(),
                    links.get_entities_by_chunk(chunk.id).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    });
    for (chunk_id, text, entity_ids) in links.unwrap() {
        let expected: Vec<i64> = entities
            .iter()
            .filter(|entity| text.contains(&format!("{}@", entity.name)))
            .map(|entity| entity.id)
            .collect();
        assert_eq!(entity_ids, expected, "chunk {chunk_id}: {text}");
    }

    // Facts: the regex stage extracts no facts, hence no quotes.
    assert_eq!(h.fact_count(), 0);
    assert_eq!(h.fact_source_count(), 0);

    // Vectors: one per chunk, the ids mirror the committed chunk rows
    // (design D5).
    assert_vectors_match_live_chunks(&h);
}

/// Second run over the unchanged tree: the content hashes match the
/// `documents` rows, so the reconcile produces no jobs and nothing is
/// re-embedded, re-chunked or re-written.
#[test]
fn second_run_skips_every_unchanged_document() {
    let h = Harness::new();
    h.seed();
    let runner = h.runner();
    let queue = DocumentJobQueue::new(&h.db);
    let worker = DocumentWorker::new(&h.db, &runner);

    // First run: both files are new → two index jobs, both processed.
    let first_md = queue
        .reconcile_source(&runner, h.md_src.to_string_lossy().as_ref())
        .unwrap();
    let first_json = queue
        .reconcile_source(&runner, h.json_src.to_string_lossy().as_ref())
        .unwrap();
    assert_eq!(first_md.indexed, 1, "{first_md:?}");
    assert_eq!(first_json.indexed, 1, "{first_json:?}");
    worker.run_once(i64::MAX / 2).unwrap();
    assert_eq!(h.docs().len(), 2);
    let rows_after_first = h.sink.ids().len();

    // Second run over the unchanged tree: the content hashes match the
    // documents rows → no new jobs.
    let second_md = queue
        .reconcile_source(&runner, h.md_src.to_string_lossy().as_ref())
        .unwrap();
    let second_json = queue
        .reconcile_source(&runner, h.json_src.to_string_lossy().as_ref())
        .unwrap();
    assert_eq!(second_md.unchanged, 1, "{second_md:?}");
    assert_eq!(second_md.indexed, 0, "{second_md:?}");
    assert_eq!(second_json.unchanged, 1, "{second_json:?}");
    assert_eq!(second_json.indexed, 0, "{second_json:?}");
    worker.run_once(i64::MAX / 2).unwrap();

    // No re-embedding, no new chunk rows, no new vector rows.
    assert_eq!(h.sink.ids().len(), rows_after_first);
    assert_eq!(h.all_chunks().len(), 4);
    assert_eq!(h.docs().len(), 2);
}

/// Modified file: the document is updated, its old chunks (and stale
/// entities/vectors) are replaced, the untouched source is skipped.
#[test]
fn modified_file_is_updated_and_stale_data_replaced() {
    let h = Harness::new();
    h.seed();
    let runner = h.runner();
    let queue = DocumentJobQueue::new(&h.db);
    let worker = DocumentWorker::new(&h.db, &runner);

    h.reconcile_and_process(&runner, i64::MAX / 2);

    let md = h.doc_by_suffix("team.md");
    let old_chunk_ids: Vec<i64> = h.chunks_of(md.id).into_iter().map(|c| c.id).collect();
    assert_eq!(old_chunk_ids.len(), 2);

    fs::write(h.md_src.join("team.md"), MD_CONTENT_V2).unwrap();
    let md_stats = queue
        .reconcile_source(&runner, h.md_src.to_string_lossy().as_ref())
        .unwrap();
    let json_stats = queue
        .reconcile_source(&runner, h.json_src.to_string_lossy().as_ref())
        .unwrap();
    assert_eq!(md_stats.indexed, 1, "{md_stats:?}"); // changed hash → re-index
    assert_eq!(json_stats.unchanged, 1, "{json_stats:?}");
    worker.run_once(i64::MAX / 2).unwrap();

    // The md document now has exactly one chunk with the new content; the
    // old chunk rows are gone (full-clear on update).
    let new_chunks = h.chunks_of(md.id);
    assert_eq!(new_chunks.len(), 1, "{new_chunks:?}");
    assert!(
        new_chunks[0].chunk_text.contains("dave@example.com"),
        "{new_chunks:?}"
    );
    assert!(!old_chunk_ids.contains(&new_chunks[0].id));

    // The stale entities (alice/bob: their only provenance was full-cleared)
    // were swept by the worker's post-batch cleanup; dave + carol survive.
    assert_entity_names(&h, &["carol", "dave"]);

    // The stale md vectors were reconciled away (design D5).
    assert_vectors_match_live_chunks(&h);
}

/// Leftovers: one orphan of every kind is swept, live pipeline data
/// (provenance-backed entities, the ingested documents and their vectors)
/// survives.
#[test]
fn cleanup_orphaned_data_sweeps_leftovers_and_keeps_live_data() {
    let h = Harness::new();
    h.seed();
    let runner = h.runner();
    h.reconcile_and_process(&runner, i64::MAX / 2);

    // One orphan of every kind.
    let ghost =
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                "person",
                "Ghost Person",
                "hr",
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap();
    let orphan_fact =
        h.db.with_conn(|conn| {
            FactDao::new(ConnectionOrTx::Connection(conn))
                .create(None, "dangling", None, "hr", None, None, None)
        })
        .unwrap()
        .unwrap();
    // `create` stores 'approved' (protected from orphan GC): flip the
    // status so the fact is a genuine orphan candidate.
    h.db.with_conn(|conn| {
        conn.execute(
            "UPDATE facts SET status = 'draft' WHERE id = ?",
            [orphan_fact],
        )
    })
    .unwrap()
    .unwrap();
    h.db.with_conn(|conn| {
        DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
            "test",
            "/elsewhere/ghost.md",
            None,
            None,
        )
    })
    .unwrap()
    .unwrap();

    let stats = runner.cleanup_orphaned_data().unwrap();
    assert_eq!(stats.entities_deleted, 1, "{stats:?}");
    assert_eq!(stats.facts_deleted, 1, "{stats:?}");
    assert_eq!(stats.documents_deleted, 1, "{stats:?}");
    assert_eq!(stats.vectors_deleted, 0, "{stats:?}");

    // The orphans are gone; the pipeline's own data survived.
    assert!(
        h.db.with_conn(|conn| {
            EntityDao::new(ConnectionOrTx::Connection(conn)).get_by_id(ghost)
        })
        .unwrap()
        .unwrap()
        .is_none(),
        "the provenance-less entity must be swept"
    );
    assert_entity_names(&h, &["alice", "bob", "carol"]);
    assert_eq!(h.docs().len(), 2);
    assert_eq!(h.all_chunks().len(), 4);
    assert_vectors_match_live_chunks(&h);
}

/// Facts and quotes: the Ingester's fact stage (synthetic endpoint
/// entities, `facts` row, `fact_sources` quote) end to end, through the
/// real markdown source and the DAOs.
#[test]
fn facts_and_quotes_are_persisted_end_to_end() {
    let dir = TempDir::new("facts");
    let root = dir.0.clone();
    fs::write(root.join("acme.md"), "Alice works at Acme Corp.").unwrap();

    let db = in_memory_db();
    let mut cfg = IngestionConfig::default();
    cfg.chunking.markdown.max_chunk_size = 10_000;
    let source = MarkdownSource::new(Box::new(MarkdownChunker::new(
        cfg.chunking.markdown.clone(),
    )));
    let embed = MockEmbedding { dim: 4 };
    let resolver = Resolver::new(0.8);
    let sink = RecordingSink {
        rows: Mutex::new(Vec::new()),
    };
    let ingester = Ingester::new(
        &db,
        &cfg,
        &source,
        &embed,
        Some(&FactStubNer),
        &resolver,
        &sink,
    );

    let stats = ingester.ingest(&root, false).unwrap();
    assert_eq!(stats.documents_created, 1, "{stats:?}");
    assert_eq!(stats.chunks_created, 1, "{stats:?}");
    // Alice (NER) + Acme Corp (synthetic, created by the fact stage).
    assert_eq!(stats.entities_extracted, 2, "{stats:?}");
    assert_eq!(stats.facts_created, 1, "{stats:?}");
    assert_eq!(stats.fact_sources_created, 1, "{stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");

    // The fact row: endpoints resolved to entity rows, weight from the
    // single source; the quote spans the whole chunk.
    let (fact, source_row, chunk_id) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let fact = FactDao::new(exec)
                .list_all()
                .unwrap()
                .pop()
                .expect("one fact");
            let source_row = FactSourceDao::new(exec)
                .get_by_fact_id(fact.id)
                .unwrap()
                .pop()
                .expect("one source");
            let chunk = ChunkDao::new(exec)
                .list_all()
                .unwrap()
                .pop()
                .expect("one chunk");
            (fact, source_row, chunk.id)
        })
        .unwrap();
    assert_eq!(fact.predicate, "works_at");
    assert_eq!(fact.status, "approved");
    assert_eq!(fact.weight, 1, "recomputed from the single source");
    assert!(fact.subject_entity_id.is_some(), "{fact:?}");
    assert!(fact.object_entity_id.is_some(), "{fact:?}");
    assert_eq!(
        source_row.quote.as_deref(),
        Some("Alice works at Acme Corp."),
        "the whole chunk fits the quote window"
    );
    assert!(source_row.extracted_at.ends_with('Z'), "{source_row:?}");

    // Both endpoint entities exist and the chunk is linked to both.
    let (names, links) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let names: Vec<String> = EntityDao::new(exec)
                .list()
                .unwrap()
                .into_iter()
                .map(|entity| entity.name)
                .collect();
            let links = ChunkEntityDao::new(exec)
                .get_entities_by_chunk(chunk_id)
                .unwrap();
            (names, links)
        })
        .unwrap();
    let mut names = names;
    names.sort();
    assert_eq!(names, vec!["Acme Corp".to_owned(), "Alice".to_owned()]);
    let mut links = links;
    links.sort();
    let endpoint_ids: Vec<i64> = vec![
        fact.subject_entity_id.unwrap(),
        fact.object_entity_id.unwrap(),
    ];
    assert_eq!(links, endpoint_ids, "the chunk links to both endpoints");

    // The post-commit vector write recorded the committed chunk row.
    let rows = sink.rows.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, chunk_id as u32);
    assert_eq!(rows[0].1, vec![1.0f32; 4]);
}
