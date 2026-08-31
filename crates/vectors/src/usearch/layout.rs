//! On-disk layout helpers (ADR 0004 §1/§7/§8): the engine directory
//! constants and path helpers, the loaded DISK segment type, crash-garbage
//! cleanup, and the DISK-segment / RAM-layer loaders run at `open`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use usearch::Index;

use super::keys_manifest::read_keys;
use super::options::{empty_index, map_usearch};
use crate::{VectorIndexConfig, VectorsError};

/// RAM layer snapshot file inside the engine directory (ADR 0004 §1).
pub(super) const RAM_INDEX_FILE: &str = "ram.usearch";
/// RAM layer sidecar key manifest (ADR 0004 §1/§3).
const RAM_KEYS_FILE: &str = "ram.keys";
/// DISK segment directory (ADR 0004 §1).
const SEGMENTS_DIR: &str = "segments";
/// Compaction scratch directory: the new segments are assembled here
/// before the atomic directory swap (ADR 0004 §7).
const SEGMENTS_TMP_DIR: &str = "segments.tmp";
/// Previous segment directory, kept until the compaction swap is complete
/// (ADR 0004 §7).
const SEGMENTS_OLD_DIR: &str = "segments.old";
/// DISK segment file prefix: `segment-<n>.usearch` / `segment-<n>.keys`
/// (ADR 0004 §1; `n` is decimal and monotonic).
const SEGMENT_FILE_PREFIX: &str = "segment-";
/// DISK segment index file extension.
const SEGMENT_INDEX_EXT: &str = ".usearch";

/// The RAM layer snapshot file path (ADR 0004 §1).
pub(super) fn ram_index_path(root: &Path) -> PathBuf {
    root.join(RAM_INDEX_FILE)
}

/// The RAM layer sidecar manifest path (ADR 0004 §1).
pub(super) fn ram_keys_path(root: &Path) -> PathBuf {
    root.join(RAM_KEYS_FILE)
}

/// The DISK segment directory (ADR 0004 §1).
pub(super) fn segments_dir(root: &Path) -> PathBuf {
    root.join(SEGMENTS_DIR)
}

/// The compaction scratch directory (ADR 0004 §7): the new segments are
/// assembled here before the atomic directory swap.
pub(super) fn segments_tmp_dir(root: &Path) -> PathBuf {
    root.join(SEGMENTS_TMP_DIR)
}

/// The previous segment directory (ADR 0004 §7): `segments/` is renamed
/// here during the compaction swap and removed once the swap is complete.
pub(super) fn segments_old_dir(root: &Path) -> PathBuf {
    root.join(SEGMENTS_OLD_DIR)
}

/// The DISK segment sidecar manifest path for segment id `id`.
pub(super) fn segment_keys_path(root: &Path, id: u32) -> PathBuf {
    segments_dir(root).join(format!("{SEGMENT_FILE_PREFIX}{id}.keys"))
}

/// One loaded DISK segment (ADR 0004 §1/§2): the read-only mmap view, its
/// monotonic id (the `segment-<n>` file number; higher = fresher), and the
/// sidecar key manifest (the durable key record, ADR 0004 §3).
#[derive(Clone)]
pub(super) struct DiskSegment {
    pub(super) id: u32,
    pub(super) index: Arc<Index>,
    /// The sidecar key manifest (ADR 0004 §3): the durable key record for
    /// this layer. Task 3.4 reads it for `count`/`chunk_ids` and task 3.7
    /// for the compaction live-key set; task 3.3 only stores it.
    #[allow(dead_code)]
    pub(super) keys: HashSet<u32>,
}

/// True if the ADR 0004 §1 layout exists at `root`: a `ram.keys` manifest,
/// a `ram.usearch` snapshot, or a `segments/` directory.
pub(super) fn layout_exists(root: &Path) -> bool {
    ram_keys_path(root).is_file() || ram_index_path(root).is_file() || segments_dir(root).is_dir()
}

/// Removes crash garbage (ADR 0004 §7 crash matrix, §8 step 1): the
/// `segments.tmp/` and `segments.old/` directories and `*.tmp`/`*.old`
/// files left by atomic writes. The special case "no `segments/` but a
/// `segments.tmp/`" (a crash between the two directory renames of a
/// compaction) is repaired by promoting the scratch directory to the live
/// one.
pub(super) fn cleanup_garbage(root: &Path) -> Result<(), VectorsError> {
    let tmp_dir = root.join(SEGMENTS_TMP_DIR);
    let live_dir = segments_dir(root);
    let old_dir = root.join(SEGMENTS_OLD_DIR);
    if tmp_dir.is_dir() {
        if live_dir.is_dir() {
            std::fs::remove_dir_all(&tmp_dir)?;
        } else {
            // Crash between the two renames: the scratch directory holds
            // the compaction result and the live directory is gone.
            std::fs::rename(&tmp_dir, &live_dir)?;
        }
    }
    if old_dir.is_dir() {
        std::fs::remove_dir_all(&old_dir)?;
    }
    // Atomic-write residue in the root and the live segment directory.
    for dir in [root, live_dir.as_path()] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let is_garbage = name.ends_with(".tmp") || name.ends_with(".old");
            if is_garbage && entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Loads the DISK segments (ADR 0004 §8 step 2): scans `segments/` for
/// `segment-<n>.usearch` files, maps each read-only with its sidecar key
/// manifest, and checks the dimensionality (step 5). A missing or corrupt
/// sidecar is a distinct error, never a silent gap (ADR 0004 §3).
pub(super) fn load_disk_segments(
    root: &Path,
    config: &VectorIndexConfig,
) -> Result<Vec<DiskSegment>, VectorsError> {
    let dir = segments_dir(root);
    let mut segments = Vec::new();
    if dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                name.starts_with(SEGMENT_FILE_PREFIX) && name.ends_with(SEGMENT_INDEX_EXT)
            })
            .collect();
        files.sort();
        for path in files {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let id: u32 = file_name
                .strip_prefix(SEGMENT_FILE_PREFIX)
                .and_then(|rest| rest.strip_suffix(SEGMENT_INDEX_EXT))
                .and_then(|id_text| id_text.parse().ok())
                .ok_or_else(|| {
                    VectorsError::Engine(format!("segment file has a non-numeric id: {file_name}"))
                })?;
            // The sidecar is the durable key record (ADR 0004 §3); a
            // missing or corrupt manifest is a distinct error.
            let keys = read_keys(&segment_keys_path(root, id))?;
            let index = Index::restore_view(&to_str(&path)?).map_err(map_usearch)?;
            if index.dimensions() != config.dim {
                return Err(VectorsError::DimensionMismatch {
                    expected: config.dim,
                    actual: index.dimensions(),
                });
            }
            segments.push(DiskSegment {
                id,
                index: Arc::new(index),
                keys: keys.into_iter().collect(),
            });
        }
    }
    segments.sort_by_key(|segment| segment.id);
    Ok(segments)
}

/// Loads the RAM layer (ADR 0004 §8 step 3): the `ram.usearch` snapshot
/// into an in-memory copy (`restore_from_buffer` — never mmap, ADR audit
/// #12), or an empty index if the snapshot is absent (the honest loss
/// window, ADR 0004 §5). The key manifest comes from `ram.keys`; a
/// snapshot without its manifest is a distinct error (ADR 0004 §3).
pub(super) fn load_ram_layer(
    root: &Path,
    config: &VectorIndexConfig,
) -> Result<(Index, HashSet<u32>), VectorsError> {
    let snapshot = ram_index_path(root);
    let keys_path = ram_keys_path(root);
    if snapshot.is_file() {
        let bytes = std::fs::read(&snapshot)?;
        let index = Index::restore_from_buffer(&bytes).map_err(map_usearch)?;
        if index.dimensions() != config.dim {
            return Err(VectorsError::DimensionMismatch {
                expected: config.dim,
                actual: index.dimensions(),
            });
        }
        let keys = read_keys(&keys_path)?;
        return Ok((index, keys.into_iter().collect()));
    }
    // No snapshot: the RAM layer is empty (create-state or the loss
    // window). The manifest, when present, is trusted for the key listing.
    if keys_path.is_file() {
        let keys = read_keys(&keys_path)?;
        return Ok((empty_index(config)?, keys.into_iter().collect()));
    }
    Ok((empty_index(config)?, HashSet::new()))
}

/// usearch FFI paths are C strings: the path must be valid UTF-8.
pub(super) fn to_str(path: &Path) -> Result<String, VectorsError> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        VectorsError::InvalidArgument(format!("path is not valid UTF-8: {}", path.display()))
    })
}

#[cfg(test)]
mod layout_tests {
    //! On-disk layout, startup recovery, and rebuild tests
    //! (usearch-wal-persistence task 3.3, ADR 0004 §1/§4/§7/§8).

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use rusqlite::Connection;

    use super::super::{
        UsearchEngine,
        keys_manifest::{read_keys, write_keys},
        options::options,
        test_util::{
            TempDir, create_wal_table, insert_wal_row, test_config, test_vector, wal_rows,
            write_segment,
        },
    };
    use super::*;

    #[test]
    fn create_then_open_empty_layout() {
        let dir = TempDir::new("create-open");
        let config = test_config();
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        drop(engine);

        // ADR 0004 §1: the empty layout is durable from create — the RAM
        // snapshot pair (the empty snapshot carries the dimension, task
        // 3.3 revision) and the segment directory.
        assert!(dir.0.join("ram.keys").is_file(), "empty ram.keys manifest");
        assert!(
            dir.0.join("ram.usearch").is_file(),
            "the create-time empty snapshot exists"
        );
        assert!(dir.0.join("segments").is_dir(), "segments directory");
        assert!(
            read_keys(&dir.0.join("ram.keys")).unwrap().is_empty(),
            "the manifest starts empty"
        );

        // A second create on the same layout fails (use open for that).
        assert!(UsearchEngine::create(&dir.0, config.clone()).is_err());

        // open returns an empty engine (count = 0), no error.
        let engine = UsearchEngine::open(&dir.0, config).unwrap();
        assert_eq!(engine.count().unwrap(), 0);
        assert!(engine.chunk_ids().unwrap().is_empty());
    }

    /// Task 3.3 revision (regression guard): the dimension of an EMPTY
    /// created index is durable on disk — reopening with a different
    /// dimension fails with [`VectorsError::DimensionMismatch`]. Before
    /// the fix `create` wrote only the (empty) `ram.keys` manifest; the
    /// `ram.usearch` snapshot appeared at the first save, so an 8-dim and
    /// a 4-dim empty index were byte-identical on disk and `open` with a
    /// different dimension silently succeeded.
    #[test]
    fn empty_created_index_dimension_is_durable() {
        let dir = TempDir::new("empty-dim");
        let config = VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("dim-8 config");
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        assert_eq!(engine.count().unwrap(), 0, "the created index is empty");
        drop(engine);

        // A dim-4 config against the stored dim-8 layout must fail.
        let other = VectorIndexConfig::new(4, 4, 8, 1, 1, 8).expect("dim-4 config");
        match UsearchEngine::open(&dir.0, other) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (4, 8));
            }
            Ok(_) => panic!("expected DimensionMismatch, got an opened engine"),
            Err(other) => panic!("expected DimensionMismatch, got: {other:?}"),
        }
    }

    /// Task 3.3 revision: an empty `rebuild` (0 rows) leaves a
    /// dim-detectable layout — the saved snapshot carries the dimension,
    /// so a later `open` with a different dimension still fails.
    #[test]
    fn empty_rebuild_leaves_dim_detectable_layout() {
        let dir = TempDir::new("empty-rebuild");
        let config = VectorIndexConfig::new(8, 4, 8, 1, 1, 8).expect("dim-8 config");
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        engine
            .rebuild(&[(1, test_vector(8, 1)), (2, test_vector(8, 2))])
            .unwrap();
        // The empty rebuild: rows replaced by nothing, state persisted.
        engine.rebuild(&[]).unwrap();
        assert_eq!(engine.count().unwrap(), 0);
        assert!(dir.0.join("ram.usearch").is_file(), "snapshot persisted");
        drop(engine);

        let other = VectorIndexConfig::new(4, 4, 8, 1, 1, 8).expect("dim-4 config");
        assert!(
            matches!(
                UsearchEngine::open(&dir.0, other),
                Err(VectorsError::DimensionMismatch { .. })
            ),
            "an empty rebuild must leave the dimension detectable"
        );
    }

    #[test]
    fn open_missing_path_is_not_found() {
        let dir = TempDir::new("open-missing");
        let config = test_config();

        // A path that does not exist at all.
        match UsearchEngine::open(dir.0.join("nope"), config.clone()) {
            Err(VectorsError::NotFound(_)) => {}
            Ok(_) => panic!("expected NotFound, got an opened engine"),
            Err(other) => panic!("expected NotFound, got: {other:?}"),
        }
        // An existing directory without a layout.
        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::NotFound(_)) => {}
            Ok(_) => panic!("expected NotFound, got an opened engine"),
            Err(other) => panic!("expected NotFound, got: {other:?}"),
        }
    }

    #[test]
    fn garbage_cleanup_on_open() {
        let dir = TempDir::new("garbage");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A valid segment must survive the cleanup.
        write_segment(&dir.0, 2, &config, &[7]);

        // Crash garbage (ADR 0004 §7 crash matrix).
        let tmp_dir = dir.0.join("segments.tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::fs::write(tmp_dir.join("segment-9.usearch"), b"garbage").unwrap();
        std::fs::write(dir.0.join("segment-1.tmp"), b"garbage").unwrap();
        std::fs::write(dir.0.join("stale.old"), b"garbage").unwrap();

        let engine = UsearchEngine::open(&dir.0, config.clone()).unwrap();

        assert!(!dir.0.join("segments.tmp").exists(), "segments.tmp removed");
        assert!(!dir.0.join("segment-1.tmp").exists(), "*.tmp removed");
        assert!(!dir.0.join("stale.old").exists(), "*.old removed");
        assert!(
            dir.0.join("segments").join("segment-2.usearch").is_file(),
            "valid segment intact"
        );

        // The restored DISK segment is searchable end-to-end (mmap view):
        // key 7 holds the axis-0 unit vector, the query is axis 1, so the
        // L2sq distance is exactly 2.0.
        let results = engine.search(&test_vector(8, 2), 1).unwrap();
        assert_eq!(results, vec![(7, 2.0)]);
    }

    #[test]
    fn garbage_cleanup_promotes_segments_tmp() {
        let dir = TempDir::new("promote");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Simulate the crash between the two directory renames (ADR 0004
        // §7): segments/ is gone, the compaction result sits in segments.tmp/.
        std::fs::remove_dir_all(dir.0.join("segments")).unwrap();
        let tmp_dir = dir.0.join("segments.tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(3, &test_vector(8, 3)).unwrap();
        index
            .save(tmp_dir.join("segment-1.usearch").to_str().unwrap())
            .unwrap();
        write_keys(&tmp_dir.join("segment-1.keys"), &[3]).unwrap();

        let engine = UsearchEngine::open(&dir.0, config.clone()).unwrap();

        assert!(
            !dir.0.join("segments.tmp").exists(),
            "the scratch directory is promoted, not deleted"
        );
        assert!(
            dir.0.join("segments").join("segment-1.usearch").is_file(),
            "promoted segment intact"
        );
        let results = engine.search(&test_vector(8, 3), 1).unwrap();
        assert_eq!(results.first().map(|(id, _)| *id), Some(3));
    }

    #[test]
    fn open_missing_sidecar_is_distinct_error() {
        let dir = TempDir::new("missing-sidecar");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A segment index file WITHOUT its sidecar manifest (ADR 0004 §3:
        // both files of a pair must exist).
        let segments = dir.0.join("segments");
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(1, &test_vector(8, 1)).unwrap();
        index
            .save(segments.join("segment-1.usearch").to_str().unwrap())
            .unwrap();

        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::NotFound(path)) => {
                assert!(
                    path.contains("segment-1.keys"),
                    "the error must name the missing manifest: {path}"
                );
            }
            Ok(_) => panic!("expected a distinct error, got an opened engine"),
            Err(other) => panic!("expected a distinct error, got: {other:?}"),
        }
    }

    #[test]
    fn open_corrupt_sidecar_is_distinct_error() {
        let dir = TempDir::new("corrupt-sidecar");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        let segments = dir.0.join("segments");
        let index = Index::new(&options(&config)).unwrap();
        index.reserve(1).unwrap();
        index.add(1, &test_vector(8, 1)).unwrap();
        index
            .save(segments.join("segment-1.usearch").to_str().unwrap())
            .unwrap();
        // A manifest with a corrupted magic (8 zero bytes).
        std::fs::write(segments.join("segment-1.keys"), [0u8; 8]).unwrap();

        assert!(
            matches!(
                UsearchEngine::open(&dir.0, config),
                Err(VectorsError::KeysBadMagic)
            ),
            "a corrupted magic must be a distinct KeysBadMagic error, not a panic"
        );
    }

    #[test]
    fn open_dim_mismatch_is_distinct_error() {
        let dir = TempDir::new("dim-mismatch");
        let config = test_config(); // dim 8
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // A segment file built with another dimality (16).
        let other = VectorIndexConfig::new(16, 4, 8, 1, 1, 8).expect("other-dim config");
        write_segment(&dir.0, 1, &other, &[1, 2]);

        match UsearchEngine::open(&dir.0, config) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (8, 16));
            }
            Ok(_) => panic!("expected DimensionMismatch, got an opened engine"),
            Err(other) => panic!("expected DimensionMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn wal_reconciliation_drops_orphan_rows() {
        let dir = TempDir::new("wal-reconcile");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Segment 2 exists on disk; segment 5 does not (orphan rows).
        write_segment(&dir.0, 2, &config, &[10, 11]);

        let db_path = dir.0.join("knowledge.db");
        let conn = Connection::open(&db_path).unwrap();
        create_wal_table(&conn);
        insert_wal_row(&conn, 5, 1, 2); // orphan: no segment-5 files
        insert_wal_row(&conn, 2, 2, 2); // live: segment-2 exists
        insert_wal_row(&conn, 2, 3, 2);
        insert_wal_row(&conn, 0, 4, 2); // RAM (segment 0): always kept
        drop(conn);

        let engine =
            UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            wal_rows(&conn),
            vec![(0, 4), (2, 2), (2, 3)],
            "orphan rows are removed, existing-segment and RAM rows are kept"
        );
        drop(conn);

        // The raw stale sets are loaded at open (ADR 0004 §8 step 6).
        let mut expected: HashMap<u32, HashSet<u32>> = HashMap::new();
        expected.insert(2, HashSet::from([2, 3]));
        expected.insert(0, HashSet::from([4]));
        assert_eq!(engine.stale_sets(), expected);

        // The WAL write path stays attached after open: re-inserting a key
        // that lives in a DISK segment writes its supersession row (ADR
        // 0004 §3). Key 10 is in segment-2.
        engine.insert(10, &test_vector(8, 1)).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).contains(&(2, 10)),
            "the insert must journal the supersession row to the attached WAL: {:?}",
            wal_rows(&conn)
        );
        drop(conn);
    }

    #[test]
    fn rebuild_clears_disk_and_wal() {
        let dir = TempDir::new("rebuild");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();

        // Three DISK segments + WAL rows for all of them and for RAM.
        write_segment(&dir.0, 1, &config, &[1, 2]);
        write_segment(&dir.0, 2, &config, &[3]);
        write_segment(&dir.0, 3, &config, &[4, 5]);

        let db_path = dir.0.join("knowledge.db");
        let conn = Connection::open(&db_path).unwrap();
        create_wal_table(&conn);
        for &(segment_id, chunk_id) in &[(1u32, 1u32), (1, 2), (2, 3), (3, 4), (3, 5), (0, 9)] {
            insert_wal_row(&conn, segment_id, chunk_id, 2);
        }
        drop(conn);

        let engine =
            UsearchEngine::open_with_wal(&dir.0, config.clone(), Some(db_path.as_path())).unwrap();

        let rows: Vec<(u32, Vec<f32>)> = (100..103)
            .map(|id| (id, test_vector(8, id as usize)))
            .collect();
        engine.rebuild(&rows).unwrap();

        // 0 DISK segments on disk: the old layers must not survive (ADR
        // 0004 §4, defect #8 closed).
        let remaining: Vec<String> = std::fs::read_dir(dir.0.join("segments"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            remaining.is_empty(),
            "old DISK layers must not survive the rebuild: {remaining:?}"
        );

        // 0 WAL rows.
        let conn = Connection::open(&db_path).unwrap();
        assert!(wal_rows(&conn).is_empty(), "the WAL must be cleared");
        drop(conn);

        // RAM = rows only; the stale cache is reset with the WAL.
        assert_eq!(engine.count().unwrap(), rows.len() as u64);
        assert!(
            engine.stale_sets().is_empty(),
            "the stale sets must be reset with the WAL"
        );
    }

    #[test]
    fn ram_restore_round_trip() {
        let dir = TempDir::new("ram-restore");
        let config = test_config();
        let engine = UsearchEngine::create(&dir.0, config.clone()).unwrap();
        engine
            .insert_batch(&[(1, &test_vector(8, 1)), (2, &test_vector(8, 2))])
            .unwrap();
        engine.save().unwrap();

        // The snapshot + sidecar are on disk; no atomic-write residue.
        assert!(dir.0.join("ram.usearch").is_file(), "snapshot saved");
        assert_eq!(
            read_keys(&dir.0.join("ram.keys")).unwrap(),
            vec![1, 2],
            "the sidecar tracks the RAM keys"
        );
        let residue: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp") || name.ends_with(".old"))
            .collect();
        assert!(residue.is_empty(), "no atomic-write residue: {residue:?}");

        // The reopen restores the RAM layer into an in-memory copy.
        let reopened = UsearchEngine::open(&dir.0, config).unwrap();
        assert_eq!(reopened.count().unwrap(), 2);
        let results = reopened.search(&test_vector(8, 2), 1).unwrap();
        assert_eq!(results.first().map(|(id, _)| *id), Some(2));
    }
}
