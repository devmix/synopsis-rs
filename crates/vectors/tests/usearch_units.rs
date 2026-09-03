//! Integration tests for `vectors::usearch::UsearchEngine` (moved
//! verbatim from the `flush_tests` module in
//! `crates/vectors/src/usearch/mod.rs`, test-hygiene phase-2 task 2.6).
//! Import paths rewritten from `super::` to the public `vectors` API;
//! the crate-private `keys_manifest::read_keys` codec and the RAM layer
//! index size are reached through `vectors::test_support`. The shared
//! fixtures (temp dir, config, vector, WAL-table helpers) are local
//! copies of the crate's inline `test_util` fixtures, which stay in the
//! crate for the sibling inline test modules.
//!
//! Flush-on-overflow and shutdown-save tests (usearch-wal-persistence
//! task 3.7, ADR 0004 §5/§7/§8): the overflow trigger, the flush
//! ordering crash matrix, the segment-0 WAL cleanup, and the idempotent
//! shutdown save point.

// Test code: unwrap/expect are intentional (the fixtures are deterministic).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use vectors::test_support::{ram_index_size, read_keys};
use vectors::{UsearchConfig, UsearchEngine, VectorIndexConfig, VectorsError};

/// A unique temporary directory that removes itself (and its contents)
/// when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "synopsis-vectors-test-{}-{tag}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small, fast test config (dim 8, minimal HNSW parameters).
fn test_config() -> VectorIndexConfig {
    VectorIndexConfig::new(8, 4, 8, 8).expect("valid test config")
}

/// A deterministic test vector: unit vector on axis `axis`.
fn test_vector(dim: usize, axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; dim];
    vector[axis % dim] = 1.0;
    vector
}

/// Creates the ADR 0004 WAL table (migrations 3+4 shape) in `conn`.
fn create_wal_table(conn: &Connection) {
    conn.execute(
        "CREATE TABLE usearch_vectors_log (
            segment_id INTEGER NOT NULL,
            chunk_id INTEGER NOT NULL,
            flags INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (segment_id, chunk_id)
        )",
        [],
    )
    .unwrap();
}

/// The WAL rows as `(segment_id, chunk_id)` pairs, ordered.
fn wal_rows(conn: &Connection) -> Vec<(i64, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT segment_id, chunk_id FROM usearch_vectors_log \
             ORDER BY segment_id, chunk_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

/// A test config (dim 8) with `max_segment_vectors = max` and the
/// given `ef_search`, as the pair [`UsearchEngine::create_with_config`]
/// and the `VectorIndexConfig.usearch` section expect. The HNSW graph
/// quality is raised (m = 16, efConstruction = 100) so the small
/// fixtures get full recall in the k = N "find everything" searches.
fn small_config(max: usize, ef_search: usize) -> (VectorIndexConfig, UsearchConfig) {
    let mut config = test_config();
    config.m = 16;
    config.ef_construction = 100;
    config.ef_search = ef_search;
    let usearch = UsearchConfig {
        max_segment_vectors: max,
        ..UsearchConfig::default()
    };
    config.usearch = Some(usearch.clone());
    (config, usearch)
}

/// Inserts `ids` as axis unit vectors in one batch.
fn insert_axis_rows(engine: &UsearchEngine, ids: impl Iterator<Item = u32>) {
    let rows: Vec<(u32, Vec<f32>)> = ids.map(|id| (id, test_vector(8, id as usize))).collect();
    let refs: Vec<(u32, &[f32])> = rows
        .iter()
        .map(|(id, vector)| (*id, vector.as_slice()))
        .collect();
    engine.insert_batch(&refs).unwrap();
}

/// A WAL database (table created) at `dir/knowledge.db`.
fn wal_db(dir: &TempDir) -> PathBuf {
    let path = dir.0.join("knowledge.db");
    let conn = Connection::open(&path).unwrap();
    create_wal_table(&conn);
    drop(conn);
    path
}

/// ADR 0004 §5: with `max_segment_vectors = 100`, inserting 150 rows
/// flushes the first 100 to segment-1 and keeps the last 50 in RAM;
/// the search spans both layers and finds every row.
#[test]
fn overflow_flush_creates_disk_segment_and_search_spans_layers() {
    let dir = TempDir::new("overflow-flush");
    let (config, usearch_config) = small_config(100, 512);
    let engine = UsearchEngine::create_with_config(&dir.0, config, usearch_config).unwrap();

    // Three batches of 50: batch 2 pushes the RAM layer to 100
    // (>= max_segment_vectors) -> flush; batch 3 lands in the fresh
    // RAM layer.
    insert_axis_rows(&engine, 1..=50);
    insert_axis_rows(&engine, 51..=100);
    insert_axis_rows(&engine, 101..=150);

    // One DISK segment (segment-1) with the first 100 keys.
    let segments = dir.0.join("segments");
    assert!(
        segments.join("segment-1.usearch").is_file(),
        "segment-1 index"
    );
    assert_eq!(
        read_keys(&segments.join("segment-1.keys")).unwrap(),
        (1..=100).collect::<Vec<u32>>(),
        "the sidecar holds the flushed keys"
    );
    // RAM holds the remaining 50.
    assert_eq!(ram_index_size(&engine), 50, "RAM size after the flush");
    // count/chunk_ids span RAM + DISK (manifests, no stale rows yet).
    assert_eq!(engine.count().unwrap(), 150);
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=150).collect::<Vec<u32>>());

    // Search spans both layers: every inserted key is found (k = 150;
    // the high ef_search of the test config gives full recall here).
    let results = engine.search(&test_vector(8, 1), 150).unwrap();
    assert_eq!(results.len(), 150, "every inserted key is found");
    let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
    assert_eq!(found, (1..=150).collect::<HashSet<_>>());
}

/// ADR 0004 §5 step 3: the flush deletes every segment-0 WAL row —
/// the RAM-layer invalidations become redundant once the RAM layer is
/// a DISK segment.
#[test]
fn flush_deletes_wal_segment0_rows() {
    let dir = TempDir::new("flush-wal");
    let (config, usearch_config) = small_config(100, 8);
    let db_path = wal_db(&dir);
    UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
    let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

    // Batch 1 (1..=100) flushes to segment-1; batch 2 (101..=150) in RAM.
    insert_axis_rows(&engine, 1..=100);
    insert_axis_rows(&engine, 101..=150);
    // Delete 11 RAM keys: the WAL gets (0, k, DEL) rows.
    let deleted: Vec<u32> = (101..=111).collect();
    engine.delete_by_chunk_ids(&deleted).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        wal_rows(&conn),
        (101..=111).map(|key| (0, key as i64)).collect::<Vec<_>>(),
        "the deletes journal segment-0 rows"
    );
    drop(conn);

    // Batch 3 (151..=211, 61 keys) pushes RAM to 100 -> the second
    // flush must delete the segment-0 rows.
    insert_axis_rows(&engine, 151..=211);

    let segments = dir.0.join("segments");
    assert!(
        segments.join("segment-1.usearch").is_file(),
        "segment-1 intact"
    );
    assert!(
        segments.join("segment-2.usearch").is_file(),
        "segment-2 flushed"
    );
    let conn = Connection::open(&db_path).unwrap();
    assert!(
        wal_rows(&conn).is_empty(),
        "the flush must delete the segment-0 rows: {:?}",
        wal_rows(&conn)
    );
    drop(conn);

    // count = segment-1 (100: 1..=100) + segment-2 (100: the 39
    // surviving RAM keys 112..=150 plus the new 151..=211).
    assert_eq!(engine.count().unwrap(), 200);
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=100).chain(112..=211).collect::<Vec<u32>>());
}

/// ADR 0004 §5/§7 crash matrix: a crash between the segment file and
/// the sidecar (or a lost sidecar) leaves an incomplete pair — open
/// must fail with the distinct sidecar error (ADR §3), never recover
/// silently.
#[test]
fn flush_crash_missing_sidecar_is_distinct_error() {
    // Build the flushed state (segment-1 + 50 RAM keys).
    let dir = TempDir::new("crash-sidecar");
    let (config, usearch_config) = small_config(100, 8);
    let engine = UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
    insert_axis_rows(&engine, 1..=150);
    drop(engine);

    // The sidecar is gone: the pair is incomplete.
    std::fs::remove_file(dir.0.join("segments").join("segment-1.keys")).unwrap();

    match UsearchEngine::open(&dir.0, config) {
        Err(VectorsError::NotFound(path)) => {
            assert!(
                path.contains("segment-1.keys"),
                "the error must name the missing manifest: {path}"
            );
        }
        Ok(_) => panic!("expected a distinct sidecar error, got an opened engine"),
        Err(other) => panic!("expected a distinct sidecar error, got: {other:?}"),
    }
}

/// ADR 0004 §5/§7 crash matrix: a lost segment file (the sidecar
/// survives) is recoverable — open drops the segment with its keys
/// and the engine stays consistent on the surviving layers.
#[test]
fn flush_crash_missing_segment_file_recovers() {
    // With a WAL: the flushed state + a saved RAM snapshot.
    let dir = TempDir::new("crash-segfile");
    let (config, usearch_config) = small_config(100, 64);
    let db_path = wal_db(&dir);
    UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
    let engine =
        UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();
    // Three batches of 50: batch 2 pushes RAM to 100 -> segment-1;
    // batch 3 leaves the 50 RAM keys to be saved.
    insert_axis_rows(&engine, 1..=50);
    insert_axis_rows(&engine, 51..=100);
    insert_axis_rows(&engine, 101..=150);
    // Persist the RAM snapshot (the shutdown save) so the reopen sees
    // the 50 RAM keys.
    engine.build_index().unwrap();
    drop(engine);

    // The segment file is gone (the sidecar survives).
    std::fs::remove_file(dir.0.join("segments").join("segment-1.usearch")).unwrap();

    let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();
    assert_eq!(engine.count().unwrap(), 50, "only the RAM layer survives");
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (101..=150).collect::<Vec<u32>>());
    // The surviving RAM keys are searchable.
    let results = engine.search(&test_vector(8, 101), 50).unwrap();
    let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        found,
        (101..=150).collect::<HashSet<_>>(),
        "every surviving RAM key is found"
    );
}

/// ADR 0004 §4: the shutdown save point (`build_index`) persists the
/// RAM layer; it is idempotent — a repeated save with no new inserts
/// is a no-op with no error and no duplicate.
#[test]
fn shutdown_save_persists_ram_and_is_idempotent() {
    let dir = TempDir::new("shutdown-save");
    let (config, usearch_config) = small_config(100, 64);
    let engine = UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
    insert_axis_rows(&engine, 1..=10);
    // The shutdown save point (trait method): persist the RAM layer.
    engine.build_index().unwrap();
    drop(engine);

    // A fresh engine on the same layout sees all 10 rows.
    let reopened = UsearchEngine::open(&dir.0, config.clone()).unwrap();
    assert_eq!(reopened.count().unwrap(), 10);
    let mut ids = reopened.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=10).collect::<Vec<u32>>());
    let results = reopened.search(&test_vector(8, 1), 10).unwrap();
    let found: HashSet<u32> = results.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        found,
        (1..=10).collect::<HashSet<_>>(),
        "search finds every saved row"
    );

    // Save again with no new inserts: no error, no duplicate.
    reopened.build_index().unwrap();
    drop(reopened);
    let again = UsearchEngine::open(&dir.0, config).unwrap();
    assert_eq!(
        again.count().unwrap(),
        10,
        "no duplicate after the second save"
    );
    let mut ids = again.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=10).collect::<Vec<u32>>());
}

/// ADR 0004 §5/§6: `count`/`chunk_ids` across RAM + DISK after a
/// flush — manifests minus the per-segment stale sets (a superseded
/// DISK key and a deleted RAM key both drop out exactly once).
#[test]
fn count_and_chunk_ids_across_flush() {
    let dir = TempDir::new("count-flush");
    let (config, usearch_config) = small_config(100, 8);
    let db_path = wal_db(&dir);
    UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
    let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

    insert_axis_rows(&engine, 1..=100); // flushes to segment-1
    insert_axis_rows(&engine, 101..=150); // RAM
    // Supersede a DISK key (re-insert 50) and delete a RAM key (150).
    engine.insert(50, &test_vector(8, 5)).unwrap();
    engine.delete_by_chunk_ids(&[150]).unwrap();

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(
        wal_rows(&conn),
        vec![(0, 150), (1, 50)],
        "the supersession + delete rows"
    );
    drop(conn);

    // count = |keys(1) − stale[1]| + |ram − stale[0]|
    //       = (100 − 1) + (51 − 1) = 149.
    assert_eq!(engine.count().unwrap(), 149);
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=149).collect::<Vec<u32>>());
}
