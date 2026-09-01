//! Background compaction (ADR 0004 §7, usearch-wal-persistence task 3.8):
//! the stale-vector-fraction trigger, the monotonic-id repack on a
//! background `std::thread`, and the atomic directory swap with the
//! post-swap WAL cleanup.
//!
//! The query path is never blocked: `maybe_compact` returns promptly after
//! spawning the repack, and searches continue on the old segment views
//! (`Arc` clones) until the atomic in-memory list swap. A concurrent
//! `maybe_compact` while a repack is running is a no-op (single-flight).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use rayon::prelude::*;
use rusqlite::Connection;
use usearch::{Index, IndexOptions};

use super::keys_manifest::write_keys;
use super::layout::{DiskSegment, segments_dir, segments_old_dir, segments_tmp_dir, to_str};
use super::options::{map_sqlite, map_usearch, options};
use super::wal::StaleCache;
use super::{UsearchEngine, disk_segments_read_guard, disk_segments_write_guard, mutex_guard};
use crate::VectorsError;

/// The state the background repack thread works with (ADR 0004 §7):
/// `Arc` clones of the engine's shared state, so the thread outlives the
/// `maybe_compact` caller while the engine stays fully usable during the
/// repack (searches keep the old segment views alive).
struct CompactionState {
    disk_segments: Arc<RwLock<Vec<DiskSegment>>>,
    stale: Arc<StaleCache>,
    wal: Arc<Mutex<Option<Box<Connection>>>>,
    layout_lock: Arc<Mutex<()>>,
    root: PathBuf,
    index_options: IndexOptions,
    max_segment_vectors: usize,
    compaction_stale_threshold: u8,
}

impl UsearchEngine {
    /// ADR 0004 §7: the compaction trigger and the background spawn.
    ///
    /// Trigger: `stale_total / total_disk * 100 > compaction_stale_threshold`
    /// (integer arithmetic: `stale_total * 100 > threshold * total_disk`),
    /// where `stale_total` is the DEL rows with `segment_id > 0` from the
    /// versioned cache and `total_disk` is the sum of the DISK `size()`.
    /// Below the threshold (or with no DISK data) this is a cheap no-op.
    ///
    /// The repack runs on a background `std::thread` (fire-and-forget
    /// under the single-flight flag): this call returns promptly and the
    /// query path is never blocked (ADR 0004 §7). A concurrent
    /// `maybe_compact` while a repack is running is a no-op.
    pub fn maybe_compact(&self) -> Result<(), VectorsError> {
        // Trigger (ADR 0004 §7): the stale fraction from the versioned
        // cache (zero SQL) against the DISK layer sizes.
        let stale = self.stale.snapshot();
        let stale_total: usize = stale
            .iter()
            .filter(|(id, _)| **id > 0)
            .map(|(_, set)| set.len())
            .sum();
        let total_disk: usize = disk_segments_read_guard(&self.disk_segments)
            .iter()
            .map(|segment| segment.index.size())
            .sum();
        let threshold = self.usearch_config.compaction_stale_threshold;
        if total_disk == 0 || stale_total * 100 <= threshold as usize * total_disk {
            return Ok(());
        }
        // Single-flight (ADR 0004 §7): while a repack is running, a
        // concurrent trigger is a no-op.
        if self
            .compacting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        let state = CompactionState {
            disk_segments: Arc::clone(&self.disk_segments),
            stale: Arc::clone(&self.stale),
            wal: Arc::clone(&self.wal),
            layout_lock: Arc::clone(&self.layout_lock),
            root: self.root.clone(),
            index_options: options(&self.config),
            max_segment_vectors: self.usearch_config.max_segment_vectors,
            compaction_stale_threshold: self.usearch_config.compaction_stale_threshold,
        };
        let compacting = Arc::clone(&self.compacting);
        let compacting_for_thread = Arc::clone(&compacting);
        // Fire-and-forget: the thread clears the flag on completion (or on
        // failure — the trigger state is unchanged, so the next
        // `maybe_compact` retries; the query path never sees the error).
        match std::thread::Builder::new()
            .name("usearch-compaction".to_string())
            .spawn(move || {
                let _ = state.run();
                compacting_for_thread.store(false, Ordering::SeqCst);
            }) {
            Ok(_) => Ok(()),
            Err(err) => {
                // The spawn failed: the flag is still set, clear it so the
                // next `maybe_compact` retries.
                compacting.store(false, Ordering::SeqCst);
                Err(VectorsError::Engine(format!(
                    "compaction thread spawn: {err}"
                )))
            }
        }
    }
}

impl CompactionState {
    /// The ADR 0004 §7 procedure, under the layout lock (one structural
    /// procedure at a time — a concurrent flush cannot compute the same
    /// next id): live-key collection → new segments in the scratch
    /// directory → atomic directory swap → WAL cleanup → in-memory list
    /// swap. Every crash point is recoverable by `open` (ADR 0004 §7
    /// crash matrix; the "directory before WAL" ordering is mandatory).
    fn run(&self) -> Result<(), VectorsError> {
        let _layout = mutex_guard(&self.layout_lock);

        // Double-check (ADR 0004 §7 single-flight): the trigger read the stale
        // fraction BEFORE acquiring the flag; a concurrent repack may have cleared it
        // since. Re-validate under the layout lock and bail if the work is already
        // done — otherwise a redundant second repack would move the same live keys to
        // yet newer ids.
        {
            let stale = self.stale.snapshot();
            let stale_total: usize = stale
                .iter()
                .filter(|(id, _)| **id > 0)
                .map(|(_, set)| set.len())
                .sum();
            let total_disk: usize = disk_segments_read_guard(&self.disk_segments)
                .iter()
                .map(|segment| segment.index.size())
                .sum();
            let threshold = self.compaction_stale_threshold;
            if total_disk == 0 || stale_total * 100 <= threshold as usize * total_disk {
                return Ok(());
            }
        }

        // 1. Live keys of every segment: keys(n) − stale[n] (sidecar +
        //    cache, no index scans; ADR 0004 §7 step 1). The snapshot is
        //    id-sorted (every writer keeps the list sorted).
        let stale = self.stale.snapshot();
        let snapshot: Vec<(u32, Arc<Index>, HashSet<u32>)> = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments
                .iter()
                .map(|segment| {
                    let live = match stale.get(&segment.id) {
                        Some(old) => segment.keys.difference(old).copied().collect(),
                        None => segment.keys.clone(),
                    };
                    (segment.id, segment.index.clone(), live)
                })
                .collect()
        };
        // 2. The new ids continue the monotonic sequence (ADR 0004 §2:
        //    ids are never renumbered, higher = fresher).
        let next_id = snapshot
            .iter()
            .map(|(id, _, _)| *id)
            .max()
            .map(|id| id.saturating_add(1))
            .unwrap_or(1);
        // 3. Vector collection via get(key), parallel per segment (ADR
        //    0004 §7 step 2); a key live in several segments (crash
        //    window only) keeps the fresher vector.
        let live = collect_live(&snapshot, self.index_options.dimensions)?;
        // 4. New segments N+1..N+M in the scratch directory (ADR 0004 §7
        //    step 3), sliced by max_segment_vectors, each with its
        //    sidecar manifest.
        let scratch = segments_tmp_dir(&self.root);
        std::fs::create_dir_all(&scratch)?;
        let mut built: Vec<(u32, Vec<u32>)> = Vec::new();
        for (i, chunk) in live.chunks(self.max_segment_vectors).enumerate() {
            let id = next_id.saturating_add(i as u32);
            let index = Index::new(&self.index_options).map_err(map_usearch)?;
            // The 2.26 core rejects an insertion without reserved
            // capacity (module docs: reserve-before-mutate).
            index.reserve(chunk.len().max(1)).map_err(map_usearch)?;
            for (key, vector) in chunk {
                index.add(*key as u64, vector).map_err(map_usearch)?;
            }
            index
                .save(&to_str(&scratch.join(format!("segment-{id}.usearch")))?)
                .map_err(map_usearch)?;
            let mut keys: Vec<u32> = chunk.iter().map(|(key, _)| *key).collect();
            keys.sort_unstable();
            write_keys(&scratch.join(format!("segment-{id}.keys")), &keys)?;
            built.push((id, keys));
        }
        // 5. The atomic directory swap (ADR 0004 §7 step 4): a directory
        //    rename is atomic on one filesystem, so no reader ever sees a
        //    half-swapped directory.
        swap_segment_dirs(&self.root)?;
        // 6. WAL cleanup AFTER the swap (ADR 0004 §7 step 5): the old ids
        //    are gone and the new segments are clean — every DISK row is
        //    orphaned. A crash between the swap and this delete leaves
        //    orphans that the next `open` reconciles (ADR 0004 §7 crash
        //    matrix); the reverse order would resurrect stale keys.
        let wal = mutex_guard(&self.wal);
        if let Some(conn) = wal.as_deref() {
            conn.execute("DELETE FROM usearch_vectors_log WHERE segment_id > 0", [])
                .map_err(map_sqlite)?;
        }
        // 7. In-memory list swap + stale-cache bump (ADR 0004 §7 step 6):
        //    searches on the old views keep working until this moment.
        let mut replaced = Vec::with_capacity(built.len());
        for (id, keys) in built {
            let path = segments_dir(&self.root).join(format!("segment-{id}.usearch"));
            let index = Index::restore_view(&to_str(&path)?).map_err(map_usearch)?;
            replaced.push(DiskSegment {
                id,
                index: Arc::new(index),
                keys: keys.into_iter().collect(),
            });
        }
        *disk_segments_write_guard(&self.disk_segments) = replaced;
        self.stale.apply(|sets| sets.retain(|id, _| *id == 0));
        Ok(())
    }
}

/// A live vector of one segment: `(chunk_id, vector)`.
type LiveEntry = (u32, Vec<f32>);

/// ADR 0004 §7 step 2: collects the live vectors of the snapshot —
/// `get(key)` per live key, parallel across segments. A key live in
/// several segments (crash window only — the write path supersedes) keeps
/// the FRESHER segment's vector (higher id wins). The result is sorted by
/// key (deterministic chunking).
fn collect_live(
    snapshot: &[(u32, Arc<Index>, HashSet<u32>)],
    dim: usize,
) -> Result<Vec<LiveEntry>, VectorsError> {
    // Parallel per segment; the collect preserves the (id-sorted) order.
    let per_segment: Result<Vec<Vec<LiveEntry>>, VectorsError> = snapshot
        .par_iter()
        .map(|(_, index, live)| {
            let mut buffer = vec![0.0f32; dim];
            let mut out = Vec::with_capacity(live.len());
            for &key in live.iter() {
                let found = index.get::<f32>(key as u64, &mut buffer).map_err(map_usearch)?;
                if found == 0 {
                    // The sidecar and the index disagree: a corrupt
                    // segment (crash window). Fail the repack rather than
                    // drop the key — the trigger state is unchanged, so
                    // the next `maybe_compact` retries.
                    return Err(VectorsError::Engine(format!(
                        "compaction: key {key} is in the segment manifest but missing from the index"
                    )));
                }
                out.push((key, buffer.to_vec()));
            }
            Ok(out)
        })
        .collect();
    // Fresher-wins dedup: the snapshot is id-sorted, so a later (higher
    // id) segment overwrites an earlier one's copy of the same key.
    let mut merged: HashMap<u32, Vec<f32>> = HashMap::new();
    for entries in per_segment? {
        for (key, vector) in entries {
            merged.insert(key, vector);
        }
    }
    let mut live: Vec<LiveEntry> = merged.into_iter().collect();
    live.sort_by_key(|(key, _)| *key);
    Ok(live)
}

/// ADR 0004 §7 step 4: the atomic directory swap — `segments/` →
/// `segments.old/`, `segments.tmp/` → `segments/`, then remove
/// `segments.old/`.
fn swap_segment_dirs(root: &Path) -> Result<(), VectorsError> {
    let live_dir = segments_dir(root);
    let old_dir = segments_old_dir(root);
    let scratch = segments_tmp_dir(root);
    // Defensive: a pre-existing segments.old (crash residue) would make
    // the first rename fail — `open`'s garbage cleanup normally removes
    // it, but a compaction can run without a preceding open.
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir)?;
    }
    std::fs::rename(&live_dir, &old_dir)?;
    std::fs::rename(&scratch, &live_dir)?;
    std::fs::remove_dir_all(&old_dir)?;
    Ok(())
}

#[cfg(test)]
mod compaction_tests {
    //! Background compaction tests (usearch-wal-persistence task 3.8,
    //! ADR 0004 §7): the stale-fraction trigger, the monotonic-id repack,
    //! the atomic directory swap, the post-swap WAL cleanup, the crash
    //! matrix, and the single-flight guarantee.

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use rusqlite::Connection;

    use super::super::UsearchEngine;
    use super::super::keys_manifest::read_keys;
    use super::super::test_util::{
        TempDir, create_wal_table, insert_wal_row, test_config, test_vector, wal_rows,
        write_segment,
    };
    use crate::{UsearchConfig, VectorIndexConfig};

    /// A WAL database (table created) at `dir/knowledge.db`.
    fn wal_db(dir: &TempDir) -> PathBuf {
        let path = dir.0.join("knowledge.db");
        let conn = Connection::open(&path).unwrap();
        create_wal_table(&conn);
        drop(conn);
        path
    }

    /// A test config (dim 8) with `max_segment_vectors = max` and the
    /// given `ef_search` (the HNSW quality is raised so the small
    /// fixtures get full recall in the k = N searches).
    fn segment_config(max: usize, ef_search: usize) -> (VectorIndexConfig, UsearchConfig) {
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

    /// Generous bound for the background-repack poll. The repack is tiny
    /// (finishes in milliseconds under normal scheduling); this ceiling only
    /// matters when the OS deschedules the background thread for many seconds
    /// under full-workspace parallel load. 10 s proved too tight on a 16 GB
    /// laptop; 60 s is 6x headroom.
    const REPACK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

    /// Polls `cond` every 10 ms until it holds or the timeout expires.
    fn wait_until(cond: impl Fn() -> bool) {
        let deadline = Instant::now() + REPACK_WAIT_TIMEOUT;
        while !cond() {
            assert!(
                Instant::now() < deadline,
                "the compaction did not finish within the timeout"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The DISK segment ids present in `root/segments` (file scan; empty
    /// while the swap window has the directory briefly absent).
    fn segment_ids(root: &std::path::Path) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(root.join("segments")) else {
            return Vec::new();
        };
        let mut ids = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_prefix("segment-")?
                    .strip_suffix(".usearch")?
                    .parse::<u32>()
                    .ok()
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// ADR 0004 §7: below the stale threshold `maybe_compact` is a cheap
    /// no-op — no new segments, no WAL change, no background thread.
    #[test]
    fn below_threshold_is_a_noop() {
        let dir = TempDir::new("compact-below");
        let (config, usearch_config) = segment_config(1_000_000, 64);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        write_segment(&dir.0, 1, &config, &(1..=25).collect::<Vec<_>>());
        write_segment(&dir.0, 2, &config, &(26..=50).collect::<Vec<_>>());
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // 4 stale of 50 (8%) < 30%: below the threshold.
        engine.delete_by_chunk_ids(&[1, 2, 3, 4]).unwrap();

        engine.maybe_compact().unwrap();
        // Give a (wrongly spawned) repack a chance to act.
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(
            segment_ids(&dir.0),
            vec![1, 2],
            "no new segments below the threshold"
        );
        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            wal_rows(&conn),
            (1..=4).map(|key| (1, key as i64)).collect::<Vec<_>>(),
            "the WAL is untouched"
        );
        drop(conn);
        assert!(
            !engine.compacting.load(Ordering::SeqCst),
            "the single-flight flag is not set"
        );
    }

    /// ADR 0004 §7: above the threshold the background repack merges the
    /// DISK segments into new monotonic id(s) > the old max; the old ids
    /// are gone, the live content is unchanged, and the WAL rows for the
    /// old ids are deleted (after the swap).
    #[test]
    fn compaction_merges_segments_with_monotonic_ids() {
        let dir = TempDir::new("compact-merge");
        let (config, usearch_config) = segment_config(1_000_000, 512);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        write_segment(&dir.0, 1, &config, &(1..=20).collect::<Vec<_>>());
        write_segment(&dir.0, 2, &config, &(21..=40).collect::<Vec<_>>());
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // 16 stale of 40 (40%) > 30%: the trigger fires.
        let stale: Vec<u32> = (1..=8).chain(21..=28).collect();
        engine.delete_by_chunk_ids(&stale).unwrap();
        let live_before: Vec<u32> = (9..=20).chain(29..=40).collect();
        assert_eq!(
            engine.count().unwrap(),
            24,
            "24 live keys before the compaction"
        );

        // Reference: the search result before the compaction (the stale
        // filter hides the deleted keys).
        let before = engine.search(&test_vector(8, 1), 24).unwrap();
        let before_ids: HashSet<u32> = before.iter().map(|(id, _)| *id).collect();

        engine.maybe_compact().unwrap();
        // Wait for the background repack: the new segment exists and the
        // old ones are gone.
        wait_until(|| {
            let ids = segment_ids(&dir.0);
            if !(ids.contains(&3) && !ids.contains(&1) && !ids.contains(&2)) {
                return false;
            }
            // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
            // it too, or the WAL assertion below is racy under load.
            let conn = Connection::open(&db_path).unwrap();
            wal_rows(&conn).is_empty()
        });

        // The 24 live keys fit in one new segment-3 (id > old max); the
        // old ids are gone.
        assert_eq!(
            segment_ids(&dir.0),
            vec![3],
            "one new segment, old ids gone"
        );
        assert_eq!(
            read_keys(&dir.0.join("segments").join("segment-3.keys")).unwrap(),
            live_before,
            "the sidecar holds exactly the live keys"
        );
        // The live content is unchanged: count, key listing, and search.
        assert_eq!(engine.count().unwrap(), 24, "count unchanged");
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, live_before, "the live keys are unchanged");
        let after = engine.search(&test_vector(8, 1), 24).unwrap();
        let after_ids: HashSet<u32> = after.iter().map(|(id, _)| *id).collect();
        assert_eq!(before_ids, after_ids, "the search finds the same live keys");

        // The WAL rows of the old ids are deleted (after the swap).
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "the DISK WAL rows are cleaned: {:?}",
            wal_rows(&conn)
        );
        drop(conn);
        assert!(
            !engine.compacting.load(Ordering::SeqCst),
            "the single-flight flag is cleared"
        );
    }

    /// ADR 0004 §7: live vectors above `max_segment_vectors` are sliced
    /// into M = ceil(live / max) new segments, each ≤ max (the last one
    /// smaller).
    #[test]
    fn compaction_slices_live_vectors_by_max_segment_vectors() {
        let dir = TempDir::new("compact-slice");
        let (config, usearch_config) = segment_config(10, 512);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        write_segment(&dir.0, 1, &config, &(1..=25).collect::<Vec<_>>());
        write_segment(&dir.0, 2, &config, &(26..=50).collect::<Vec<_>>());
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // 22 stale of 50 (44%) > 30%: 28 live -> M = ceil(28/10) = 3.
        let stale: Vec<u32> = (1..=11).chain(26..=36).collect();
        engine.delete_by_chunk_ids(&stale).unwrap();

        engine.maybe_compact().unwrap();
        wait_until(|| {
            let ids = segment_ids(&dir.0);
            if !(ids.contains(&5) && !ids.contains(&1) && !ids.contains(&2)) {
                return false;
            }
            // The WAL DELETE runs after the directory swap (ADR 0004 §7): wait for
            // it too, or the WAL assertion below is racy under load.
            let conn = Connection::open(&db_path).unwrap();
            wal_rows(&conn).is_empty()
        });

        // New ids 3..=5 (N = 2): 10 + 10 + 8, each ≤ max.
        assert_eq!(
            segment_ids(&dir.0),
            vec![3, 4, 5],
            "three new segments, old ids gone"
        );
        let segments = dir.0.join("segments");
        let first = read_keys(&segments.join("segment-3.keys")).unwrap();
        let second = read_keys(&segments.join("segment-4.keys")).unwrap();
        let third = read_keys(&segments.join("segment-5.keys")).unwrap();
        assert_eq!(first.len(), 10, "the first slice is full");
        assert_eq!(second.len(), 10, "the second slice is full");
        assert_eq!(third.len(), 8, "the last slice is smaller");
        let mut live = first;
        live.extend(second);
        live.extend(third);
        live.sort_unstable();
        let expected: Vec<u32> = (12..=25).chain(37..=50).collect();
        assert_eq!(live, expected, "all live keys are repacked");
        assert_eq!(engine.count().unwrap(), 28, "count unchanged");
        let conn = Connection::open(&db_path).unwrap();
        assert!(wal_rows(&conn).is_empty(), "the DISK WAL rows are cleaned");
        drop(conn);
    }

    /// ADR 0004 §7 crash matrix: a crash between the directory swap and
    /// the WAL DELETE leaves the NEW segments in `segments/` plus the
    /// ORPHAN WAL rows of the old ids; `open` reconciles the orphans and
    /// the engine is consistent.
    #[test]
    fn crash_between_swap_and_wal_cleanup_recovers_on_open() {
        let dir = TempDir::new("compact-crash");
        let (config, usearch_config) = segment_config(1_000_000, 64);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        // The post-swap state: the new segment-3 is the live directory
        // content (the old segment-1/2 files are gone with segments.old).
        write_segment(&dir.0, 3, &config, &(1..=10).collect::<Vec<_>>());
        // The pre-cleanup state: the old ids' DEL rows are still there.
        let db_path = wal_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        insert_wal_row(&conn, 1, 1, 2);
        insert_wal_row(&conn, 2, 5, 2);
        drop(conn);

        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // The orphan rows (whose segment files no longer exist) are
        // removed by the open-time reconciliation (ADR 0004 §7 crash
        // matrix / §8 step 4).
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "the orphan rows are removed: {:?}",
            wal_rows(&conn)
        );
        drop(conn);
        assert_eq!(engine.count().unwrap(), 10, "the new segment is fully live");
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, (1..=10).collect::<Vec<_>>());
    }

    /// ADR 0004 §7: single-flight — concurrent `maybe_compact` calls run
    /// exactly ONE repack (the others see the in-progress flag and are
    /// no-ops). A second repack would move the first's output to yet
    /// newer ids, so the final id set proves how many ran.
    #[test]
    fn concurrent_maybe_compact_runs_exactly_once() {
        let dir = TempDir::new("compact-singleflight");
        let (config, usearch_config) = segment_config(1_000_000, 64);
        UsearchEngine::create_with_config(&dir.0, config.clone(), usearch_config).unwrap();
        for (id, range) in [(1, 1..=25u32), (2, 26..=50), (3, 51..=75), (4, 76..=100)] {
            let keys: Vec<u32> = range.collect();
            write_segment(&dir.0, id, &config, &keys);
        }
        let db_path = wal_db(&dir);
        let engine = Arc::new(
            UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap(),
        );

        // 40 stale of 100 (40%) > 30%: every early call passes the trigger.
        let stale: Vec<u32> = (1..=20).chain(26..=45).collect();
        engine.delete_by_chunk_ids(&stale).unwrap();

        // Four threads hammer `maybe_compact` concurrently: the
        // single-flight flag admits exactly one repack.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let engine = Arc::clone(&engine);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    engine.maybe_compact().unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        wait_until(|| !engine.compacting.load(Ordering::SeqCst));

        // Exactly one repack: the 60 live keys land in ONE segment-5. A
        // second repack would move them to a segment-6.
        assert_eq!(segment_ids(&dir.0), vec![5], "exactly one compaction ran");
        assert_eq!(engine.count().unwrap(), 60, "no data loss");
        let conn = Connection::open(&db_path).unwrap();
        assert!(wal_rows(&conn).is_empty(), "the WAL is cleaned");
        drop(conn);
    }

    /// A RAM-only engine (no DISK segments, no WAL) never triggers:
    /// `maybe_compact` is a cheap Ok no-op.
    #[test]
    fn ram_only_engine_is_a_noop() {
        let dir = TempDir::new("compact-ramonly");
        let engine = UsearchEngine::create(&dir.0, test_config()).unwrap();
        engine.insert(1, &test_vector(8, 1)).unwrap();
        engine.maybe_compact().unwrap();
        assert!(
            !engine.compacting.load(Ordering::SeqCst),
            "the single-flight flag is not set"
        );
        assert!(segment_ids(&dir.0).is_empty(), "no segments appear");
    }
}
