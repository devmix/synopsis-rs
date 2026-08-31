//! WAL journal (ADR 0004 §3/§4): the DEL-only write path with the
//! WAL-first supersession invariant, the versioned in-memory stale-set
//! cache (zero SQL in steady state), and the open-time helpers
//! (reconciliation, stale-set load).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, Transaction, params};

use super::options::map_sqlite;
use super::{UsearchEngine, disk_segments_read_guard, mutex_guard};
use crate::VectorsError;

/// WAL flag: a vector is invalid in that segment — deleted or superseded
/// by a fresher layer (ADR 0004 §3). The only flag the write path writes;
/// `ADD (1)` and `UPD (4)` are reserved, never written, because the key
/// record lives in the sidecar key manifests (the WAL stays small and
/// holds invalidations only).
pub(super) const WAL_DEL: u8 = 2;

/// Versioned in-memory stale-set cache (ADR 0004 §3):
/// `segment_id → deleted/superseded chunk ids`.
///
/// The cache is the in-memory mirror of the durable WAL: it is loaded with
/// one SQL select on open ([`load_stale_sets`]) and updated in place by
/// every write transaction (the version is bumped with each update).
/// Steady-state reads (`search`, `count`, `chunk_ids`) take a snapshot of
/// the sets — zero SQL on the query path (ADR 0004 §3, §6).
pub(super) struct StaleCache {
    /// Bumped by every write transaction (and by the open-time load);
    /// readers can observe that the sets changed.
    version: Arc<AtomicU64>,
    sets: Mutex<HashMap<u32, HashSet<u32>>>,
}

impl StaleCache {
    /// An empty cache (no WAL attached, or a freshly created engine):
    /// version 0, no stale sets.
    pub(super) fn empty() -> Self {
        Self {
            version: Arc::new(AtomicU64::new(0)),
            sets: Mutex::new(HashMap::new()),
        }
    }

    /// A cache preloaded with the open-time WAL load (version 1).
    pub(super) fn loaded(sets: HashMap<u32, HashSet<u32>>) -> Self {
        Self {
            version: Arc::new(AtomicU64::new(1)),
            sets: Mutex::new(sets),
        }
    }

    /// Replaces the sets wholesale and bumps the version (used by
    /// [`UsearchEngine::with_wal_db`], which attaches a WAL to an
    /// existing engine, and by the compaction thread, which clears the
    /// stale sets of the replaced segments after the WAL cleanup).
    pub(super) fn replace(&self, sets: HashMap<u32, HashSet<u32>>) {
        let mut current = cache_guard(&self.sets);
        *current = sets;
        self.version.fetch_add(1, Ordering::Release);
    }

    /// The current version (0 = never written; bumped per write
    /// transaction).
    ///
    /// Read only by the write-path tests (the steady-state query path
    /// reads the sets, not the version), hence the lint opt-out.
    #[allow(dead_code)]
    pub(super) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// A snapshot of the stale sets (the steady-state read: no SQL).
    pub(super) fn snapshot(&self) -> HashMap<u32, HashSet<u32>> {
        cache_guard(&self.sets).clone()
    }

    /// Applies an in-place update to the sets and bumps the version. Every
    /// WAL write transaction goes through here after its commit, so the
    /// cache is always current (ADR 0004 §3).
    pub(super) fn apply(&self, update: impl FnOnce(&mut HashMap<u32, HashSet<u32>>)) {
        let mut sets = cache_guard(&self.sets);
        update(&mut sets);
        self.version.fetch_add(1, Ordering::Release);
    }
}

impl UsearchEngine {
    /// Runs `apply` inside ONE SQLite transaction on the dedicated WAL
    /// connection (ADR 0004 §3 invariant 3: all DEL rows of one operation
    /// commit atomically, BEFORE the RAM mutation — WAL-first). A no-op
    /// when no WAL connection is attached (RAM-only engine).
    fn wal_transaction(
        &self,
        apply: impl FnOnce(&Transaction) -> Result<(), VectorsError>,
    ) -> Result<(), VectorsError> {
        // `transaction` needs `&mut Connection`: the guard is interior
        // mutability, so the mutable deref is explicit.
        let mut wal = mutex_guard(&self.wal);
        let Some(conn) = wal.as_deref_mut() else {
            return Ok(());
        };
        let tx = conn.transaction().map_err(map_sqlite)?;
        apply(&tx)?;
        tx.commit().map_err(map_sqlite)
    }

    /// The ADR 0004 §3 `insert` write rule as one WAL transaction: for
    /// every batch key present in a DISK segment, one `(s, key, DEL)`
    /// supersession row. Keys present only in RAM write no rows: the RAM
    /// `add` replaces the old copy in place (non-multi index), so the key
    /// stays live in the freshest layer — a `(0, key, DEL)` row would hide
    /// the fresh vector from search until the next flush.
    ///
    /// After the commit the stale cache is bumped with the written rows
    /// (steady-state search reads the cache, zero SQL).
    pub(super) fn insert_supersession(&self, keys: &[u32]) -> Result<(), VectorsError> {
        // No WAL attached (RAM-only engine): nothing to journal, and the
        // stale cache must not be bumped (there is no durable log behind it).
        if mutex_guard(&self.wal).is_none() {
            return Ok(());
        }
        // The DISK layers are immutable for this operation (flush/
        // compaction replace the list under the write lock, excluded by
        // this read guard); the sidecar manifests are the durable key
        // record of each layer (ADR 0004 §3).
        let pairs: Vec<(u32, u32)> = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            let mut pairs = Vec::new();
            for segment in segments.iter() {
                for &key in keys {
                    if segment.keys.contains(&key) {
                        pairs.push((segment.id, key));
                    }
                }
            }
            pairs
        };
        if pairs.is_empty() {
            return Ok(());
        }
        self.wal_transaction(|tx| {
            for &(segment_id, chunk_id) in &pairs {
                upsert_del(tx, segment_id, chunk_id)?;
            }
            Ok(())
        })?;
        self.stale.apply(|sets| {
            for &(segment_id, chunk_id) in &pairs {
                sets.entry(segment_id).or_default().insert(chunk_id);
            }
        });
        Ok(())
    }

    /// The ADR 0004 §3/§4 `delete` write rule as one WAL transaction:
    /// `(0, key, DEL)` for keys present in RAM (the RAM snapshot file may
    /// still hold them until the next save) plus `(s, key, DEL)` for keys
    /// present in each DISK segment. After the commit the stale cache is
    /// bumped with the written rows.
    pub(super) fn delete_invalidations(&self, keys: &[u32]) -> Result<(), VectorsError> {
        // No WAL attached (RAM-only engine): nothing to journal, and the
        // stale cache must not be bumped (there is no durable log behind it).
        if mutex_guard(&self.wal).is_none() {
            return Ok(());
        }
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        // RAM (segment 0): the on-disk RAM snapshot mirrors the in-memory
        // index, so a key invalidates segment 0 only while it is present in
        // the RAM manifest (ADR 0004 §4.2). A key absent from RAM and every
        // DISK segment is an idempotent no-op — no rows, no version bump.
        let ram_keys = mutex_guard(&self.ram_keys);
        for &key in keys {
            if ram_keys.contains(&key) {
                pairs.push((0, key));
            }
        }
        drop(ram_keys);
        for segment in disk_segments_read_guard(&self.disk_segments).iter() {
            for &key in keys {
                if segment.keys.contains(&key) {
                    pairs.push((segment.id, key));
                }
            }
        }
        if pairs.is_empty() {
            return Ok(());
        }
        self.wal_transaction(|tx| {
            for &(segment_id, chunk_id) in &pairs {
                upsert_del(tx, segment_id, chunk_id)?;
            }
            Ok(())
        })?;
        self.stale.apply(|sets| {
            for &(segment_id, chunk_id) in &pairs {
                sets.entry(segment_id).or_default().insert(chunk_id);
            }
        });
        Ok(())
    }

    /// Removes every row from `usearch_vectors_log` (used by `rebuild`:
    /// the entire state is replaced and persisted, so every record is
    /// stale) and resets the stale cache with it.
    ///
    /// The WAL table delete is skipped when no WAL connection is attached;
    /// the cache reset always runs (the cache is empty without a WAL, but
    /// the version bump keeps readers consistent with the cleared state).
    pub(super) fn clear_wal(&self) -> Result<(), VectorsError> {
        let wal = mutex_guard(&self.wal);
        if let Some(conn) = wal.as_deref() {
            conn.execute("DELETE FROM usearch_vectors_log", [])
                .map_err(map_sqlite)?;
        }
        self.stale.apply(|sets| sets.clear());
        Ok(())
    }

    /// The ADR 0004 §5 flush WAL step (procedure step 3): removes every
    /// segment-0 row in one transaction. After the RAM layer is persisted
    /// as a new DISK segment, the RAM-layer invalidations are redundant:
    /// their keys are physically absent from the flushed file, and the
    /// older-segment supersessions live in their own rows (ADR §3). A
    /// no-op when no WAL connection is attached.
    pub(super) fn flush_wal_ram_rows(&self) -> Result<(), VectorsError> {
        if mutex_guard(&self.wal).is_none() {
            return Ok(());
        }
        self.wal_transaction(|tx| {
            tx.execute("DELETE FROM usearch_vectors_log WHERE segment_id = 0", [])
                .map_err(map_sqlite)?;
            Ok(())
        })
    }
}

/// Upserts one DEL row — the only rows the write path writes
/// (ADR 0004 §3): `PK (segment_id, chunk_id)`, the latest operation wins,
/// `created_at = datetime('now')` (UTC).
fn upsert_del(tx: &Transaction, segment_id: u32, chunk_id: u32) -> Result<(), VectorsError> {
    tx.execute(
        "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at) \
         VALUES (?1, ?2, ?3, datetime('now'))",
        params![segment_id as i64, chunk_id as i64, WAL_DEL as i64],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

/// WAL reconciliation on open (ADR 0004 §8 step 4): removes the orphan
/// rows whose segment files no longer exist — the self-heal after a
/// crashed compaction/flush. `segment_id = 0` (the RAM layer) is always
/// kept.
pub(super) fn reconcile_wal(conn: &Connection, disk_ids: &[u32]) -> Result<(), VectorsError> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT segment_id FROM usearch_vectors_log WHERE segment_id != 0")
        .map_err(map_sqlite)?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite)?
        .collect::<Result<_, _>>()
        .map_err(map_sqlite)?;
    for id in ids {
        let exists = disk_ids.contains(&(u32::try_from(id).unwrap_or(u32::MAX)));
        if !exists {
            conn.execute(
                "DELETE FROM usearch_vectors_log WHERE segment_id = ?1",
                params![id],
            )
            .map_err(map_sqlite)?;
        }
    }
    Ok(())
}

/// Loads the raw per-segment stale sets from the WAL (ADR 0004 §8 step 6):
/// `segment_id → deleted/superseded chunk ids` for every DEL record. The
/// result seeds the versioned search-path cache ([`StaleCache::loaded`]).
pub(super) fn load_stale_sets(
    conn: &Connection,
) -> Result<HashMap<u32, HashSet<u32>>, VectorsError> {
    let mut stmt = conn
        .prepare("SELECT segment_id, chunk_id FROM usearch_vectors_log WHERE flags = ?1")
        .map_err(map_sqlite)?;
    let rows = stmt
        .query_map(params![WAL_DEL as i64], |row| {
            let segment_id = row.get::<_, i64>(0)?;
            let chunk_id = row.get::<_, i64>(1)?;
            Ok((segment_id, chunk_id))
        })
        .map_err(map_sqlite)?;
    let mut stale: HashMap<u32, HashSet<u32>> = HashMap::new();
    for row in rows {
        let (segment_id, chunk_id) = row.map_err(map_sqlite)?;
        let segment_id = u32::try_from(segment_id).map_err(|_| {
            VectorsError::Engine(format!("WAL segment_id {segment_id} exceeds u32 range"))
        })?;
        let chunk_id = u32::try_from(chunk_id).map_err(|_| {
            VectorsError::Engine(format!("WAL chunk_id {chunk_id} exceeds u32 range"))
        })?;
        stale.entry(segment_id).or_default().insert(chunk_id);
    }
    Ok(stale)
}

/// Locks the stale-cache sets, recovering the guard from a poisoned mutex
/// (a panicked reader leaves the sets untouched — they are only ever
/// updated by the write transactions).
fn cache_guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod wal_tests {
    //! WAL write-path tests (usearch-wal-persistence task 3.5, ADR 0004
    //! §3/§4): DEL-only supersession, the versioned stale cache,
    //! manifest-based `count`/`chunk_ids`, and the WAL-first invariant.

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::super::UsearchEngine;
    use super::super::test_util::{
        TempDir, create_wal_table, test_config, test_vector, wal_rows, write_segment,
    };
    use super::*;
    use crate::VectorsError;

    /// Opens a WAL database (table created) at `dir/knowledge.db`.
    fn wal_db(dir: &TempDir) -> PathBuf {
        let path = dir.0.join("knowledge.db");
        let conn = Connection::open(&path).unwrap();
        create_wal_table(&conn);
        drop(conn);
        path
    }

    /// ADR 0004 §3 insert rule: supersession rows are written only for
    /// keys present in a DISK segment. A key that lives in RAM only is
    /// replaced in place by the RAM `add` (non-multi index), so a
    /// re-insert writes no WAL rows at all — a `(0, k, DEL)` row would
    /// hide the fresh vector from search until the next flush.
    #[test]
    fn insert_ram_only_writes_no_wal_rows() {
        let dir = TempDir::new("insert-ram-only");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        engine.insert(7, &test_vector(8, 1)).unwrap();
        engine.insert(7, &test_vector(8, 2)).unwrap(); // re-insert (supersession)

        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "a RAM-only re-insert must write no WAL rows (ADR 0004 §3): {:?}",
            wal_rows(&conn)
        );
        drop(conn);

        assert!(engine.index.contains(7), "RAM holds the key once");
        assert_eq!(engine.count().unwrap(), 1, "count unchanged");
    }

    /// ADR 0004 §3: a key present in a DISK segment gets one
    /// `(s, k, DEL)` supersession row per such segment — and no
    /// `(0, k, DEL)` row. Re-inserting again must not duplicate the row
    /// (`PK (segment_id, chunk_id)`, INSERT OR REPLACE).
    #[test]
    fn insert_supersedes_disk_segment_rows() {
        let dir = TempDir::new("insert-supersede");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7, 8]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // Before the re-insert: RAM empty, segment-1 keys {7, 8} live.
        assert_eq!(engine.count().unwrap(), 2);

        engine.insert(7, &test_vector(8, 1)).unwrap(); // supersedes segment-1's 7
        engine.insert(7, &test_vector(8, 2)).unwrap(); // again: no duplicate row

        let conn = Connection::open(&db_path).unwrap();
        let rows = wal_rows(&conn);
        assert_eq!(
            rows,
            vec![(1, 7)],
            "exactly one (1, 7, DEL) supersession row: {rows:?}"
        );
        drop(conn);

        // The stale cache mirrors the WAL: segment 1 stale {7}.
        let mut stale: HashMap<u32, HashSet<u32>> = HashMap::new();
        stale.insert(1, HashSet::from([7]));
        assert_eq!(engine.stale_sets(), stale);

        // count = |ram − stale[0]| + |keys(1) − stale[1]| = 1 + 1 (key 8).
        assert_eq!(engine.count().unwrap(), 2);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 8]);
    }

    /// ADR 0004 §4: delete writes `(0, k, DEL)` for the key in RAM plus
    /// `(s, k, DEL)` for the DISK segment holding it — one transaction.
    #[test]
    fn delete_writes_ram_and_disk_del_rows() {
        let dir = TempDir::new("delete-rows");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7, 8]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // 7 in RAM (the insert wrote the (1, 7, DEL) supersession) + in segment-1.
        engine.insert(7, &test_vector(8, 1)).unwrap();
        engine.delete_by_chunk_ids(&[7]).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let rows = wal_rows(&conn);
        assert_eq!(
            rows,
            vec![(0, 7), (1, 7)],
            "(0,7,DEL) for RAM + (1,7,DEL) for the DISK segment: {rows:?}"
        );
        drop(conn);

        assert!(!engine.index.contains(7), "removed from RAM");
        assert_eq!(engine.count().unwrap(), 1, "only segment-1 key 8 is live");
        assert_eq!(engine.chunk_ids().unwrap(), vec![8]);
    }

    /// Deleting a key that is in neither RAM nor any DISK segment writes
    /// no rows (idempotent no-op, no WAL bloat).
    #[test]
    fn delete_absent_key_writes_no_rows() {
        let dir = TempDir::new("delete-absent");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        engine.delete_by_chunk_ids(&[99]).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "an absent key writes no rows: {:?}",
            wal_rows(&conn)
        );
        drop(conn);
        assert_eq!(engine.count().unwrap(), 1);
    }

    /// ADR 0004 §6: `count`/`chunk_ids` span RAM + DISK via the manifests
    /// minus the stale sets (ADR audit #5 closed), and a stale row hides
    /// the key only in the layer it names.
    #[test]
    fn count_and_chunk_ids_span_ram_and_disk() {
        let dir = TempDir::new("count-span");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7, 8]);
        write_segment(&dir.0, 2, &config, &[9]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // No WAL rows: every manifest key is live — RAM + both DISK layers.
        assert_eq!(engine.count().unwrap(), 3);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 8, 9]);

        // 8 is in segment-1 only: one (1, 8, DEL) row hides it there.
        engine.delete_by_chunk_ids(&[8]).unwrap();
        assert_eq!(engine.count().unwrap(), 2);
        let mut ids = engine.chunk_ids().unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 9]);
    }

    /// ADR 0004 §3 steady state: after the open-time load and the write
    /// transactions, `count`/`chunk_ids` read the in-memory cache — zero
    /// SQL. Proof: detach the WAL connection (private field, same module
    /// tree) and the values stay correct.
    #[test]
    fn count_and_chunk_ids_read_the_cache_without_sql() {
        let dir = TempDir::new("cache-no-sql");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7, 8]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        engine.insert(7, &test_vector(8, 1)).unwrap(); // (1, 7, DEL)
        engine.delete_by_chunk_ids(&[8]).unwrap(); // (1, 8, DEL)
        assert_eq!(
            engine.count().unwrap(),
            1,
            "RAM holds key 7, segment-1 all stale"
        );

        // Detach the WAL connection: the steady-state reads must not need it.
        *mutex_guard(&engine.wal) = None;
        assert_eq!(engine.count().unwrap(), 1, "count from the cache, no SQL");
        assert_eq!(engine.chunk_ids().unwrap(), vec![7]);
    }

    /// ADR 0004 §3: the cache version is bumped by every write
    /// transaction (and only then).
    #[test]
    fn stale_cache_version_bumps_on_writes() {
        let dir = TempDir::new("version");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        let v0 = engine.stale.version();
        engine.insert(7, &test_vector(8, 1)).unwrap(); // writes (1, 7, DEL)
        let v1 = engine.stale.version();
        assert!(
            v1 > v0,
            "the insert transaction bumps the version ({v0} -> {v1})"
        );

        engine.delete_by_chunk_ids(&[7]).unwrap(); // writes (0, 7, DEL) + (1, 7, DEL)
        let v2 = engine.stale.version();
        assert!(
            v2 > v1,
            "the delete transaction bumps the version ({v1} -> {v2})"
        );

        // An empty write (absent key) commits no transaction, no bump.
        engine.delete_by_chunk_ids(&[99]).unwrap();
        assert_eq!(
            engine.stale.version(),
            v2,
            "an empty transaction does not bump"
        );
    }

    /// ADR 0004 §3 invariant 3 (WAL-first): a failed WAL transaction must
    /// not mutate the RAM layer. Simulated with a second connection
    /// holding an exclusive lock (the engine's `BEGIN IMMEDIATE` fails
    /// with SQLITE_BUSY before any row is written).
    #[test]
    fn failed_transaction_leaves_ram_unchanged() {
        let dir = TempDir::new("wal-first");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        let blocker = Connection::open(&db_path).unwrap();
        blocker.execute("BEGIN EXCLUSIVE", []).unwrap();

        assert!(
            engine.insert(7, &test_vector(8, 1)).is_err(),
            "the locked WAL must make the insert fail"
        );
        blocker.execute("ROLLBACK", []).unwrap();
        drop(blocker);

        assert!(
            !engine.index.contains(7),
            "RAM unchanged after the failed transaction"
        );
        assert_eq!(
            engine.count().unwrap(),
            1,
            "count unchanged (segment-1 key 7)"
        );
        assert!(engine.stale_sets().is_empty(), "the cache was not bumped");

        // Once the lock is gone the same insert succeeds.
        engine.insert(7, &test_vector(8, 1)).unwrap();
        assert!(engine.index.contains(7));
        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(wal_rows(&conn), vec![(1, 7)]);
        drop(conn);
    }

    /// ADR 0004 §3 invariant 3: one operation's DEL rows commit
    /// atomically — a failure mid-transaction leaves no partial rows and
    /// does not bump the cache (the bump happens after the commit).
    #[test]
    fn failed_apply_rolls_back_all_rows() {
        let dir = TempDir::new("atomic");
        let config = test_config();
        UsearchEngine::create(&dir.0, config.clone()).unwrap();
        write_segment(&dir.0, 1, &config, &[7]);
        write_segment(&dir.0, 2, &config, &[7]);
        let db_path = wal_db(&dir);
        let engine = UsearchEngine::open_with_wal(&dir.0, config, Some(db_path.as_path())).unwrap();

        // A failing "write transaction": the first upsert succeeds, the
        // second errors -> the whole transaction must roll back.
        let err = engine
            .wal_transaction(|tx| {
                upsert_del(tx, 1, 7).unwrap();
                Err(VectorsError::Engine("simulated failure".to_string()))
            })
            .unwrap_err();
        assert!(err.to_string().contains("simulated failure"));

        let conn = Connection::open(&db_path).unwrap();
        assert!(
            wal_rows(&conn).is_empty(),
            "no partial rows after the rollback: {:?}",
            wal_rows(&conn)
        );
        drop(conn);
        assert!(
            engine.stale_sets().is_empty(),
            "the cache is bumped only after the commit"
        );
    }
}
