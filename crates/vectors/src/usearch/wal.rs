//! WAL journal and WAL-driven compaction (ADR 0004 §3/§4): open-time
//! reconciliation, stale-set loading, per-record journaling, and the
//! compaction helpers that enumerate live keys from the WAL.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, params};
use usearch::Index;

use super::layout::DiskSegment;
use super::options::{map_sqlite, map_usearch, options};
use super::{UsearchEngine, disk_segments_read_guard, disk_segments_write_guard};
use crate::VectorsError;

/// WAL flag: a vector was added (usearch-wal-persistence design).
pub(super) const WAL_ADD: u8 = 1;
/// WAL flag: a vector was deleted or superseded in that segment.
pub(super) const WAL_DEL: u8 = 2;

impl UsearchEngine {
    /// Journals one WAL record: `INSERT OR REPLACE` into
    /// `usearch_vectors_log` — one row per `chunk_id`, the latest
    /// operation wins, `created_at` is `datetime('now')` (UTC).
    ///
    /// A no-op when no WAL connection is attached ([`Self::with_wal_db`]).
    pub(super) fn write_wal(&self, chunk_id: u32, flags: u8) -> Result<(), VectorsError> {
        self.write_wal_with_segment(chunk_id, flags, 0) // segment_id=0 = current/RAM
    }

    fn write_wal_with_segment(
        &self,
        chunk_id: u32,
        flags: u8,
        segment_id: u32,
    ) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute(
            "INSERT OR REPLACE INTO usearch_vectors_log (segment_id, chunk_id, flags, created_at) \
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![segment_id as i64, chunk_id as i64, flags as i64],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    /// Removes WAL records for old DISK segments (segment_id > 0) after
    /// compaction — those segments no longer exist, so all their records
    /// (ADD, DEL) are stale.
    ///
    /// segment_id = 0 records (current/RAM operations) are preserved.
    ///
    /// A no-op when no WAL connection is attached.
    fn clear_old_segments(&self) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute("DELETE FROM usearch_vectors_log WHERE segment_id > 0", [])
            .map_err(map_sqlite)?;
        Ok(())
    }

    /// Removes every row from `usearch_vectors_log`.
    /// Used after rebuild when the entire index is replaced.
    ///
    /// A no-op when no WAL connection is attached.
    pub(super) fn clear_wal(&self) -> Result<(), VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(());
        };
        let conn = wal_guard(wal);
        conn.execute("DELETE FROM usearch_vectors_log", [])
            .map_err(map_sqlite)?;
        Ok(())
    }

    /// Check if compaction is needed and trigger it.
    pub fn maybe_compact(&mut self) -> Result<(), VectorsError> {
        let stale_pct = self.stale_vector_percentage()?;
        if stale_pct > self.usearch_config.compaction_stale_threshold as f64 {
            self.compact()?;
        }
        Ok(())
    }

    /// Merge all DISK segments, remove stale vectors, create new segments.
    /// RAM index is NOT touched — it's the current working set.
    /// If there are no DISK segments, this is a no-op.
    ///
    /// Uses SQL query to enumerate live keys per segment instead of
    /// `exact_search` (O(N) brute-force with distance computation).
    ///
    /// Transitional (task 3.3): the merge result replaces the in-memory
    /// list only — the on-disk directory swap (ADR 0004 §7) lands in task
    /// 3.7. New segments take the next monotonic ids (ADR 0004 §2).
    fn compact(&mut self) -> Result<(), VectorsError> {
        // No DISK segments to compact.
        {
            let segments = disk_segments_read_guard(&self.disk_segments);
            if segments.is_empty() {
                return Ok(());
            }
        }

        // 1. Load live keys per DISK segment via SQL (O(log N) indexed
        //    query).
        let live_keys_per_seg = self.load_live_keys_per_segment()?;

        // 2. Collect live vectors from DISK segments using get().
        //    Deduplicate by key: later segments win (higher segment_id =
        //    more recent snapshot).
        let dim = self.config.dim;
        let mut seen: HashSet<u32> = HashSet::new();
        let mut live_vectors: Vec<(u32, Vec<f32>)> = Vec::new();
        {
            let segments = disk_segments_read_guard(&self.disk_segments);
            for segment in segments.iter() {
                let keys = live_keys_per_seg.get(&segment.id);
                let Some(keys) = keys else {
                    continue;
                };
                for id in keys {
                    // Skip duplicates: later segment version wins.
                    if !seen.insert(*id) {
                        continue;
                    }
                    let mut vector = vec![0.0f32; dim];
                    segment
                        .index
                        .get::<f32>(*id as u64, &mut vector)
                        .map_err(map_usearch)?;
                    live_vectors.push((*id, vector));
                }
            }
        }

        // 3. Create new segments (sliced by max_segment_vectors) with the
        //    next monotonic ids.
        let next_id = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments.iter().map(|segment| segment.id).max().unwrap_or(0) + 1
        };
        let max = self.usearch_config.max_segment_vectors;
        let mut new_segments = Vec::new();
        for (segment_id, chunk) in (next_id..).zip(live_vectors.chunks(max)) {
            let new_index = Index::new(&options(&self.config)).map_err(map_usearch)?;
            new_index.reserve(chunk.len().max(1)).map_err(map_usearch)?;
            for (id, vector) in chunk {
                new_index.add(*id as u64, vector).map_err(map_usearch)?;
            }
            let keys: HashSet<u32> = chunk.iter().map(|(id, _)| *id).collect();
            new_segments.push(DiskSegment {
                id: segment_id,
                index: Arc::new(new_index),
                keys,
            });
        }

        // 4. Replace old segments (in-memory list; on-disk swap is 3.7).
        disk_segments_write_guard(&self.disk_segments).clone_from(&new_segments);

        // 5. Remove old DISK segment records from WAL (RAM records remain)
        self.clear_old_segments()?;

        Ok(())
    }

    /// Load live keys per DISK segment from WAL.
    /// Returns a map: segment_id → Vec<chunk_id> (live keys only).
    ///
    /// A key is live if it has an ADD record with `segment_id > 0` (DISK)
    /// and no DEL/UPD record for that chunk_id (in ANY segment_id).
    /// This handles the cross-segment stale problem: `delete_by_chunk_ids`
    /// writes DEL with `segment_id=0`, but the ADD records are in the
    /// DISK segment's `segment_id`.
    fn load_live_keys_per_segment(&self) -> Result<HashMap<u32, Vec<u32>>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashMap::new());
        };
        let conn = wal_guard(wal);

        // A key is live in a DISK segment if:
        // 1. It has an ADD record in that segment (flags = ADD, segment_id > 0)
        // 2. No DEL/UPD record exists for that chunk_id in ANY segment
        let mut stmt = conn
            .prepare(
                "SELECT segment_id, chunk_id FROM usearch_vectors_log \
                 WHERE flags = ?1 AND segment_id > 0 \
                 AND chunk_id NOT IN \
                   (SELECT chunk_id FROM usearch_vectors_log \
                    WHERE flags = ?2)",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![WAL_ADD as i64, WAL_DEL as i64], |row| {
                let segment_id = row.get::<_, i64>(0)?;
                let chunk_id = row.get::<_, i64>(1)?;
                Ok((segment_id, chunk_id))
            })
            .map_err(map_sqlite)?;

        let mut result: HashMap<u32, Vec<u32>> = HashMap::new();
        for row in rows {
            let (segment_id, chunk_id) = row.map_err(map_sqlite)?;
            let segment_id = u32::try_from(segment_id).map_err(|_| {
                VectorsError::Engine(format!("WAL segment_id {segment_id} exceeds u32 range"))
            })?;
            let chunk_id = u32::try_from(chunk_id).map_err(|_| {
                VectorsError::Engine(format!("WAL chunk_id {chunk_id} exceeds u32 range"))
            })?;
            result.entry(segment_id).or_default().push(chunk_id);
        }
        Ok(result)
    }

    /// Calculate percentage of stale vectors in DISK segments.
    /// Returns 0.0 if there are no DISK segments.
    ///
    /// Only counts DEL/UPD records where the chunk_id has a corresponding
    /// ADD record in a DISK segment (segment_id > 0). This avoids counting
    /// orphaned DEL records from segment_id=0 that don't correspond to
    /// any DISK vectors.
    fn stale_vector_percentage(&self) -> Result<f64, VectorsError> {
        let total = {
            let segments = disk_segments_read_guard(&self.disk_segments);
            segments
                .iter()
                .map(|segment| segment.index.size())
                .sum::<usize>()
        };
        if total == 0 {
            return Ok(0.0);
        }
        let stale = self.count_stale_in_disk()?;
        Ok(stale as f64 / total as f64 * 100.0)
    }

    /// Count stale vectors that exist in DISK segments.
    /// A vector is stale if it has a DEL record AND an ADD record
    /// with segment_id > 0 (i.e., it's in a DISK segment).
    fn count_stale_in_disk(&self) -> Result<usize, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(0);
        };
        let conn = wal_guard(wal);
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT chunk_id) FROM usearch_vectors_log \
                 WHERE flags = ?1 \
                 AND chunk_id IN \
                   (SELECT chunk_id FROM usearch_vectors_log \
                    WHERE flags = ?2 AND segment_id > 0)",
            )
            .map_err(map_sqlite)?;
        let count: i64 = stmt
            .query_row(params![WAL_DEL as i64, WAL_ADD as i64], |row| row.get(0))
            .map_err(map_sqlite)?;
        Ok(count as usize)
    }

    /// Load all WAL entries with DEL flag, grouped by segment_id.
    /// Returns a map: segment_id → HashSet<chunk_id>.
    ///
    /// Only DEL entries are included — UPD is not used in the current
    /// implementation and should not make vectors stale (UPD means
    /// "updated", i.e. old version replaced, new version is live).
    pub(super) fn load_wal_grouped(&self) -> Result<HashMap<u32, HashSet<u32>>, VectorsError> {
        let Some(wal) = &self.wal else {
            return Ok(HashMap::new());
        };
        load_stale_sets(&wal_guard(wal))
    }
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
/// raw map is what task 3.4 turns into the versioned search-path cache.
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

/// Locks the WAL connection, recovering the guard from a poisoned mutex:
/// a panic in an earlier WAL operation does not make the connection
/// unusable (SQLite rolls a panicked statement back itself, so the
/// database state stays consistent).
fn wal_guard(wal: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    match wal.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
