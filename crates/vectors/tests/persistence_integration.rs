//! Persistence integration tests (usearch-wal-persistence task 3.10, ADR
//! 0004 §8): end-to-end restart, WAL supersession/delete, background
//! compaction to completion, and parallel-search parity, driving only the
//! public API against a real WAL database.
//!
//! The ADR 0004 §8 crash-recovery matrix and the config-validation matrix
//! are already covered by the crate's unit tests and are deliberately NOT
//! re-implemented here (deduplication):
//!
//! - crash matrix: `usearch::layout` (garbage / sidecar / dim mismatch),
//!   `usearch::flush_tests` (missing sidecar / segment), `usearch::compaction`
//!   (directory swap + WAL reconciliation);
//! - config validation: `vectors::tests::usearch_config_validate_*` (this
//!   crate) and the `config::preset` `vectors_usearch_section_*` tests.
//!
//! What these tests add is the seam a unit test cannot reach: a full restart
//! across a persistent WAL database, cross-layer search correctness, a
//! fire-and-forget compaction run to completion, and the multi-threaded vs
//! single-threaded search parity.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use vectors::{UsearchConfig, UsearchEngine, VectorIndexConfig};

/// Vector dimension for every test (tiny: full recall in the k = N
/// "find everything" searches, and a trivially exact HNSW graph).
const DIM: usize = 8;

/// A unique temporary directory that removes itself (and its contents) when
/// dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "synopsis-vectors-persist-{tag}-{}-{}",
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

/// A test config (dim 8, high HNSW quality: m = 16, efConstruction = 100)
/// with the given `max_segment_vectors` / `search_threads` / `ef_search`. The
/// `usearch` tuning section travels with the config (ADR 0004 §10) —
/// `create_with_wal` / `open_with_wal` read it from here (Revision 1).
fn build_config(
    max_segment_vectors: usize,
    search_threads: usize,
    ef_search: usize,
) -> VectorIndexConfig {
    let mut config = VectorIndexConfig::new(DIM, 16, 100, 1, 1, ef_search).expect("valid config");
    config.usearch = Some(UsearchConfig {
        max_segment_vectors,
        compaction_stale_threshold: 30,
        search_threads,
    });
    config
}

/// A deterministic axis unit vector: `1.0` on `axis`, `0.0` elsewhere.
fn axis_vector(axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    vector[axis % DIM] = 1.0;
    vector
}

/// Inserts `ids` as axis unit vectors (key `id` on axis `id % DIM`) in one batch.
fn insert_axis_rows(engine: &UsearchEngine, ids: impl Iterator<Item = u32>) {
    let rows: Vec<(u32, Vec<f32>)> = ids.map(|id| (id, axis_vector(id as usize))).collect();
    let refs: Vec<(u32, &[f32])> = rows
        .iter()
        .map(|(id, vector)| (*id, vector.as_slice()))
        .collect();
    engine.insert_batch(&refs).unwrap();
}

/// A WAL database (the `usearch_vectors_log` table created, migrations 3+4
/// shape) at `dir/knowledge.db`.
fn wal_db(dir: &TempDir) -> PathBuf {
    let path = dir.0.join("knowledge.db");
    let conn = Connection::open(&path).unwrap();
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
    drop(conn);
    path
}

/// The WAL rows as `(segment_id, chunk_id)` pairs, ordered.
fn wal_rows(db: &Path) -> Vec<(i64, i64)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT segment_id, chunk_id FROM usearch_vectors_log ORDER BY segment_id, chunk_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

/// Polls `check` until it returns true or `deadline` elapses; returns the
/// final value. Waits out the fire-and-forget background compaction.
fn wait_until(deadline: Duration, check: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if check() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// ADR 0004 §8: flushed DISK segments survive a restart. With
/// `max_segment_vectors = 100`, two 150-row batches flush to two DISK
/// segments; the shutdown save writes the (empty) RAM snapshot; a fresh
/// engine opened on the same directory + WAL database sees all 300 keys, and
/// every inserted vector is found by exact axis match.
#[test]
fn restart_persists_flushed_segments() {
    let dir = TempDir::new("restart");
    let db = wal_db(&dir);
    let config = build_config(100, 4, 512);

    let engine =
        UsearchEngine::create_with_wal(&dir.0, config.clone(), Some(db.as_path())).unwrap();
    insert_axis_rows(&engine, 1..=150); // RAM 150 >= 100 -> flush segment-1
    insert_axis_rows(&engine, 151..=300); // RAM 150 >= 100 -> flush segment-2
    assert_eq!(engine.count().unwrap(), 300);
    // Both flushed DISK segments are on disk (the ADR 0004 §1 layout).
    let segments = dir.0.join("segments");
    assert!(
        segments.join("segment-1.usearch").is_file(),
        "segment-1 index"
    );
    assert!(
        segments.join("segment-2.usearch").is_file(),
        "segment-2 index"
    );
    // Shutdown save (the persistence point; a no-op on the empty RAM layer).
    engine.build_index().unwrap();
    drop(engine);

    // A fresh engine on the same directory + WAL database sees the same data.
    let reopened = UsearchEngine::open_with_wal(&dir.0, config, Some(db.as_path())).unwrap();
    assert_eq!(
        reopened.count().unwrap(),
        300,
        "all flushed keys survive the restart"
    );
    let mut ids = reopened.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, (1..=300).collect::<Vec<u32>>());

    // Every inserted vector is found by exact axis match: the distance-0
    // group of an axis query is exactly the keys stored on that axis.
    for axis in 0..DIM {
        let results = reopened.search(&axis_vector(axis), 300).unwrap();
        let near: HashSet<u32> = results
            .iter()
            .filter(|(_, d)| *d < 0.5)
            .map(|(id, _)| *id)
            .collect();
        let expected: HashSet<u32> = (1..=300u32)
            .filter(|k| (*k as usize) % DIM == axis)
            .collect();
        assert_eq!(near, expected, "axis {axis} exact-match group");
    }
}

/// ADR 0004 §3/§6: with a WAL attached, an insert supersession and a delete
/// journal DEL rows, and search hides the stale DISK copies while the fresh
/// RAM copy wins. Key 5 is flushed to segment-1, then re-inserted with a new
/// vector (axis 6); key 7 is deleted. The stale DISK copies must not surface.
#[test]
fn search_hides_stale_disk_copies_after_supersession_and_delete() {
    let dir = TempDir::new("supersession");
    let db = wal_db(&dir);
    let config = build_config(10, 4, 512);

    let engine = UsearchEngine::create_with_wal(&dir.0, config, Some(db.as_path())).unwrap();
    insert_axis_rows(&engine, 1..=10); // RAM 10 >= 10 -> flush segment-1

    // Supersede key 5 with a new vector (axis 6); delete key 7.
    engine.insert(5, &axis_vector(6)).unwrap();
    engine.delete_by_chunk_ids(&[7]).unwrap();

    // The WAL journals one DEL row per stale DISK copy (key 5 superseded,
    // key 7 deleted — both in segment-1, neither in RAM).
    assert_eq!(
        wal_rows(&db),
        vec![(1, 5), (1, 7)],
        "supersession + delete journal segment-1 rows"
    );
    let mut expected_stale = HashMap::new();
    expected_stale.insert(1, HashSet::from([5, 7]));
    assert_eq!(
        engine.stale_sets(),
        expected_stale,
        "the stale cache mirrors the WAL"
    );
    // Key 5 stays live (fresh RAM copy), key 7 is gone: 10 - 1 = 9.
    assert_eq!(
        engine.count().unwrap(),
        9,
        "superseded key lives, deleted key gone"
    );
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3, 4, 5, 6, 8, 9, 10]);

    // Key 5's OLD vector (axis 5): the stale DISK copy (distance 0) is
    // hidden; only the fresh RAM copy (axis 6) surfaces, at distance ~2.0.
    let results = engine.search(&axis_vector(5), 10).unwrap();
    let key5: Vec<f32> = results
        .iter()
        .filter(|(id, _)| *id == 5)
        .map(|(_, d)| *d)
        .collect();
    assert_eq!(key5.len(), 1, "key 5 appears exactly once");
    assert!(
        key5[0] > 1.5,
        "key 5 is at the fresh distance, not the stale 0.0: {}",
        key5[0]
    );

    // Key 5's NEW vector (axis 6): the fresh copy + key 6 (also axis 6) at
    // distance ~0.
    let results = engine.search(&axis_vector(6), 10).unwrap();
    let near: HashSet<u32> = results
        .iter()
        .filter(|(_, d)| *d < 0.5)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        near,
        [5u32, 6u32].into_iter().collect::<HashSet<_>>(),
        "axis 6 group: fresh key 5 + key 6"
    );

    // Key 7's vector (axis 7): the deleted key must not surface at all.
    let results = engine.search(&axis_vector(7), 10).unwrap();
    assert!(
        !results.iter().any(|(id, _)| *id == 7),
        "deleted key 7 must not appear"
    );
    let near: HashSet<u32> = results
        .iter()
        .filter(|(_, d)| *d < 0.5)
        .map(|(id, _)| *id)
        .collect();
    assert!(near.is_empty(), "no live key on axis 7: {:?}", near);
}

/// ADR 0004 §7: a stale-fraction trigger runs a background compaction that
/// repacks the live keys into fresh segments, cleans the WAL, and leaves the
/// directory consistent across a restart. With `max_segment_vectors = 25`,
/// two 25-row batches make two DISK segments; deleting 40% of the keys
/// (20 / 50) trips the 30% threshold.
#[test]
fn compaction_end_to_end_reopen_consistent() {
    let dir = TempDir::new("compaction");
    let db = wal_db(&dir);
    let config = build_config(25, 4, 256);

    let engine =
        UsearchEngine::create_with_wal(&dir.0, config.clone(), Some(db.as_path())).unwrap();
    insert_axis_rows(&engine, 1..=25); // RAM 25 >= 25 -> flush segment-1
    insert_axis_rows(&engine, 26..=50); // RAM 25 >= 25 -> flush segment-2
    assert_eq!(engine.count().unwrap(), 50);

    // Delete 40% of the keys: 10 in segment-1 + 10 in segment-2.
    let deleted: Vec<u32> = (1..=10).chain(26..=35).collect();
    engine.delete_by_chunk_ids(&deleted).unwrap();
    assert_eq!(engine.count().unwrap(), 30, "live keys before compaction");

    // Fire-and-forget background repack; wait for the directory swap and the
    // post-swap WAL cleanup (the old segments are gone, the DISK rows cleared).
    engine.maybe_compact().unwrap();
    let segments = dir.0.join("segments");
    let finished = || {
        !segments.join("segment-1.usearch").exists()
            && !segments.join("segment-2.usearch").exists()
            && wal_rows(&db).iter().all(|(s, _)| *s == 0)
    };
    assert!(
        wait_until(Duration::from_secs(15), finished),
        "the background compaction must finish (old segments gone, WAL clean)"
    );
    // Let the in-memory segment-list swap (the last compaction step) land.
    std::thread::sleep(Duration::from_millis(50));

    let expected: Vec<u32> = (11..=25).chain(36..=50).collect();
    assert_eq!(engine.count().unwrap(), 30, "live keys after compaction");
    let mut ids = engine.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, expected, "the repack keeps exactly the live keys");

    // A fresh engine on the same directory + WAL database sees the same
    // state, and the WAL is clean (no DISK stale rows left behind).
    let reopened = UsearchEngine::open_with_wal(&dir.0, config, Some(db.as_path())).unwrap();
    assert_eq!(
        reopened.count().unwrap(),
        30,
        "reopened engine sees the live keys"
    );
    let mut ids = reopened.chunk_ids().unwrap();
    ids.sort_unstable();
    assert_eq!(ids, expected, "reopened engine matches the live key set");
    assert!(
        reopened.stale_sets().is_empty(),
        "the WAL is clean after compaction: {:?}",
        reopened.stale_sets()
    );
    // Every live key is still reachable by search.
    let found: HashSet<u32> = (0..DIM)
        .flat_map(|axis| reopened.search(&axis_vector(axis), 30).unwrap().into_iter())
        .map(|(id, _)| id)
        .collect();
    let expected_set: HashSet<u32> = expected.iter().copied().collect();
    assert!(
        expected_set.is_subset(&found),
        "all live keys reachable after reopen: missing {:?}",
        expected_set.difference(&found)
    );
}

/// ADR 0004 §6: the parallel search over the dedicated `search_threads` pool
/// is independent of the thread count — the two-thread engine and the
/// single-threaded reference return byte-identical top-k across the RAM +
/// DISK layers, including the stale-filtered (superseded / deleted) keys.
#[test]
fn parallel_search_matches_single_threaded_reference() {
    let dir = TempDir::new("parity");
    let db = wal_db(&dir);

    // Build the state with a two-thread engine (WAL attached).
    let config_a = build_config(400, 2, 256);
    let engine_a = UsearchEngine::create_with_wal(&dir.0, config_a, Some(db.as_path())).unwrap();
    insert_axis_rows(&engine_a, 1..=400); // RAM 400 >= 400 -> flush segment-1
    insert_axis_rows(&engine_a, 401..=800); // RAM 400 >= 400 -> flush segment-2
    insert_axis_rows(&engine_a, 801..=1000); // RAM 200 < 400 -> stays in RAM
    // Supersede two DISK keys with fresh vectors; delete one DISK and one RAM key.
    engine_a.insert(100, &axis_vector(5)).unwrap(); // in segment-1
    engine_a.insert(500, &axis_vector(5)).unwrap(); // in segment-2
    engine_a.delete_by_chunk_ids(&[250, 900]).unwrap(); // 250 in segment-1, 900 in RAM

    // Persist the RAM layer so the reference engine restores the same state.
    engine_a.build_index().unwrap();
    assert_eq!(engine_a.count().unwrap(), 998, "1000 - 2 deleted");

    // The single-threaded reference engine on the same directory + WAL db.
    let config_b = build_config(400, 1, 256);
    let engine_b = UsearchEngine::open_with_wal(&dir.0, config_b, Some(db.as_path())).unwrap();
    assert_eq!(
        engine_b.count().unwrap(),
        engine_a.count().unwrap(),
        "both engines count the same live keys"
    );

    // The top-k must be byte-identical for every axis query, regardless of
    // the search pool size (the per-layer HNSW results and the merge are
    // deterministic; the pool size only parallelises the layer queries).
    for axis in 0..DIM {
        let query = axis_vector(axis);
        let results_a = engine_a.search(&query, 10).unwrap();
        let results_b = engine_b.search(&query, 10).unwrap();
        assert_eq!(
            results_a, results_b,
            "axis {axis}: the two-thread and one-thread searches must match"
        );
    }
}
