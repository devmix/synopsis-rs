//! Database fill procedure (design D11).
//!
//! Oracle: `../synopsis/internal/benchmark/filler.go`.
//!
//! The procedure: drop FTS triggers → clear + insert scalar tables in one
//! transaction → rebuild FTS → restore triggers → embed + insert vectors
//! in batches → build ANN index.

use std::collections::HashMap;
use std::time::Instant;

use db::Db;
use embedding::EmbeddingProvider;
use rusqlite::params;
use vectors::VectorIndex;

use super::generator::Dataset;

/// Result of filling the database.
#[derive(Debug)]
pub struct FillReport {
    /// Duration in milliseconds.
    pub duration_ms: f64,
    /// Number of vectors embedded.
    pub vectors: usize,
    /// Row counts per table.
    pub tables: HashMap<String, i64>,
}

/// Fill options.
pub struct FillOptions {
    /// Batch size for embedding calls.
    pub batch_size: usize,
    /// Optional progress callback `(done, total)`.
    pub progress: Option<Box<dyn Fn(usize, usize) + Send>>,
}

impl FillOptions {
    /// Creates fill options with the given batch size.
    pub fn new(batch_size: usize) -> Self {
        Self {
            batch_size,
            progress: None,
        }
    }

    /// Sets the progress callback.
    pub fn with_progress(mut self, f: impl Fn(usize, usize) + Send + 'static) -> Self {
        self.progress = Some(Box::new(f));
        self
    }
}

/// Fills the database with the dataset (scalar + vectors).
pub fn fill(
    db: &Db,
    ds: &Dataset,
    embed: &dyn EmbeddingProvider,
    vectors: &dyn VectorIndex,
    opts: &FillOptions,
) -> Result<FillReport, String> {
    let start = Instant::now();

    // 1. Drop FTS triggers.
    match db.with_conn(
        |conn: &rusqlite::Connection| -> Result<(), rusqlite::Error> {
            for sql in [
                "DROP TRIGGER IF EXISTS chunks_fts_ai",
                "DROP TRIGGER IF EXISTS chunks_fts_ad",
                "DROP TRIGGER IF EXISTS chunks_fts_au",
            ] {
                conn.execute(sql, [])?;
            }
            Ok(())
        },
    ) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("drop FTS triggers: {e}")),
        Err(e) => return Err(format!("drop FTS triggers: {e}")),
    }

    // 2. Clear + insert scalar tables in one transaction.
    let chunks_inserted = fill_scalar_tables(db, ds)?;
    if chunks_inserted == 0 && !ds.chunks.is_empty() {
        return Err("no chunks were inserted".to_owned());
    }

    // 3. Rebuild FTS + restore triggers.
    match db.with_conn(|conn: &rusqlite::Connection| -> Result<(), rusqlite::Error> {
        conn.execute("INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild')", [])?;
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS chunks_fts_ai AFTER INSERT ON chunks BEGIN
             INSERT INTO chunks_fts(rowid, chunk_text) VALUES (new.id, new.chunk_text); END",
            [],
        )?;
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS chunks_fts_ad AFTER DELETE ON chunks BEGIN
             INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text) VALUES('delete', old.id, old.chunk_text); END",
            [],
        )?;
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS chunks_fts_au AFTER UPDATE ON chunks BEGIN
             INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text) VALUES('delete', old.id, old.chunk_text);
             INSERT INTO chunks_fts(rowid, chunk_text) VALUES (new.id, new.chunk_text); END",
            [],
        )?;
        Ok(())
    }) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("rebuild FTS: {e}")),
        Err(e) => return Err(format!("rebuild FTS: {e}")),
    }

    // 4. Clear existing vectors and insert new ones in batches.
    let existing_ids = vectors.chunk_ids().map_err(|e| e.to_string())?;
    if !existing_ids.is_empty() {
        vectors
            .delete_by_chunk_ids(&existing_ids)
            .map_err(|e| e.to_string())?;
    }

    let batch_size = if opts.batch_size > 0 {
        opts.batch_size
    } else {
        100
    };
    let total = ds.chunks.len();
    let mut embedded = 0usize;

    for start_idx in (0..total).step_by(batch_size) {
        let end_idx = (start_idx + batch_size).min(total);
        let batch = &ds.chunks[start_idx..end_idx];
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();

        let vecs = embed
            .generate_embeddings(&texts)
            .map_err(|e| format!("embed chunks {start_idx}..{end_idx}: {e}"))?;
        if vecs.len() != batch.len() {
            return Err(format!(
                "embedding provider returned {} vectors for {} texts",
                vecs.len(),
                batch.len()
            ));
        }

        let rows: Vec<(u32, &[f32])> = batch
            .iter()
            .zip(vecs.iter())
            .map(|(c, v)| (c.id, v.as_slice()))
            .collect();
        vectors
            .insert_batch(&rows)
            .map_err(|e| format!("insert vectors batch {start_idx}: {e}"))?;

        embedded += batch.len();
        if let Some(ref cb) = opts.progress {
            cb(embedded, total);
        }
    }

    vectors
        .build_index()
        .map_err(|e| format!("build ANN index: {e}"))?;

    // 5. Collect table counts.
    let tables = table_counts(db)?;

    Ok(FillReport {
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        vectors: total,
        tables,
    })
}

fn fill_scalar_tables(db: &Db, ds: &Dataset) -> Result<usize, String> {
    let result: Result<usize, db::DbError> = db.exec_tx(|tx| {
        let cleanups = [
            "DELETE FROM fact_sources",
            "DELETE FROM chunk_entities",
            "DELETE FROM entity_sources",
            "DELETE FROM facts",
            "DELETE FROM entity_links",
            "DELETE FROM entities",
            "DELETE FROM chunks",
            "DELETE FROM documents",
        ];
        for sql in &cleanups {
            tx.execute(sql, [])?;
        }

        for d in &ds.documents {
            tx.execute(
                "INSERT INTO documents (id, source_type, original_path, metadata_json, content_hash) VALUES (?, ?, ?, ?, ?)",
                params![d.id, d.source_type, d.original_path, d.metadata_json, d.content_hash],
            )?;
        }
        for c in &ds.chunks {
            tx.execute(
                "INSERT INTO chunks (id, doc_id, chunk_text, sequence_num, start_offset, end_offset) VALUES (?, ?, ?, ?, ?, ?)",
                params![c.id, c.doc_id, c.text, c.seq_num, c.start_offset, c.end_offset],
            )?;
        }
        for e in &ds.entities {
            tx.execute(
                "INSERT INTO entities (id, type, name, domain, description, confidence) VALUES (?, ?, ?, ?, ?, ?)",
                params![e.id, e.entity_type, e.name, e.domain, e.description, e.confidence],
            )?;
        }
        for ce in &ds.chunk_entities {
            tx.execute(
                "INSERT OR IGNORE INTO chunk_entities (chunk_id, entity_id) VALUES (?, ?)",
                params![ce.chunk_id, ce.entity_id],
            )?;
        }
        for f in &ds.facts {
            tx.execute(
                "INSERT INTO facts (id, subject_entity_id, predicate, object_entity_id, domain, status, valid_from, valid_to, weight) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![f.id, f.subject_id, f.predicate, f.object_id, f.domain, f.status, f.valid_from, f.valid_to, f.weight],
            )?;
        }
        for fs in &ds.fact_sources {
            tx.execute(
                "INSERT INTO fact_sources (fact_id, document_id, quote) VALUES (?, ?, ?)",
                params![fs.fact_id, fs.document_id, fs.quote],
            )?;
        }
        for es in &ds.entity_sources {
            tx.execute(
                "INSERT OR IGNORE INTO entity_sources (entity_id, document_id) VALUES (?, ?)",
                params![es.entity_id, es.document_id],
            )?;
        }
        for el in &ds.entity_links {
            tx.execute(
                "INSERT OR IGNORE INTO entity_links (subject_entity_id, target_entity_id, relation_type, method, confidence, evidence) VALUES (?, ?, ?, ?, ?, ?)",
                params![el.subject_id, el.target_id, el.relation_type, el.method, el.confidence, el.evidence],
            )?;
        }

        // Pin AUTOINCREMENT sequences.
        for (table, max_id) in [
            ("documents", ds.documents.len()),
            ("chunks", ds.chunks.len()),
            ("entities", ds.entities.len()),
            ("facts", ds.facts.len()),
        ] {
            tx.execute("DELETE FROM sqlite_sequence WHERE name = ?", params![table])?;
            if max_id > 0 {
                tx.execute(
                    "INSERT INTO sqlite_sequence (name, seq) VALUES (?, ?)",
                    params![table, max_id as i64],
                )?;
            }
        }

        Ok(ds.chunks.len())
    });
    result.map_err(|e| e.to_string())
}

fn table_counts(db: &Db) -> Result<HashMap<String, i64>, String> {
    let tables = [
        "documents",
        "chunks",
        "entities",
        "facts",
        "fact_sources",
        "entity_sources",
        "chunk_entities",
        "entity_links",
    ];
    let mut counts = HashMap::new();
    for table in &tables {
        let n: i64 = db
            .with_conn(|conn| {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("count {table}: {e}"))?;
        counts.insert(table.to_string(), n);
    }
    Ok(counts)
}
