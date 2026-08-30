# Design: usearch-wal-persistence

## Key Decisions (Human 2026-08-31)

1. **WAL in SQLite** — `usearch_vectors_log` table for transactional integrity with chunks
2. **DISK_N segments** — multiple read-only mmap segments (as designed)
3. **Global compaction** — merges ALL segments, removes stale vectors, slices by 1M
4. **Parallel search** — search across segments in parallel via rayon
5. **Compaction heuristics** — background compaction when stale vectors > 30%
6. **WAL without vectors** — only chunk_id + flags (vector comes from chunks table)
7. **Per-segment WAL** — each segment tracks its own deletions/updates via segment_id

## Architecture

### Storage Model

```
┌─────────────────────────────────────────────────────────┐
│  SQLite (source of truth)                               │
│  ├── chunks (chunk_id, text, metadata)                  │
│  └── usearch_vectors_log (WAL)                          │
│      ├── segment_id (u32: 0 for current, N for snapshot)│
│      ├── chunk_id (u32)                                 │
│      ├── flags (u8: ADD=1, DEL=2, UPD=4)               │
│      └── created_at (timestamp)                         │
├─────────────────────────────────────────────────────────┤
│  DISK Segment 0 (read-only mmap)                       │
│  ├── index.usearch.0                                    │
│  └── WAL_0 = {chunk_ids with DEL|UPD at snapshot time}  │
├─────────────────────────────────────────────────────────┤
│  DISK Segment 1 (read-only mmap)                       │
│  ├── index.usearch.1                                    │
│  └── WAL_1 = {chunk_ids with DEL|UPD at snapshot time}  │
├─────────────────────────────────────────────────────────┤
│  ...                                                    │
├─────────────────────────────────────────────────────────┤
│  RAM Layer (ephemeral, mutable)                         │
│  └── usearch Index (current working set)                │
└─────────────────────────────────────────────────────────┘
```

### WAL Table Schema

```sql
CREATE TABLE usearch_vectors_log (
    segment_id  INTEGER NOT NULL,  -- 0 = current/RAM, N = snapshot segment
    chunk_id    INTEGER NOT NULL,
    flags       INTEGER NOT NULL,  -- ADD=1, DEL=2, UPD=4
    created_at  TEXT NOT NULL,
    PRIMARY KEY (segment_id, chunk_id)
);

CREATE INDEX idx_usearch_vectors_log_segment ON usearch_vectors_log(segment_id);
```

**Key insight:** `segment_id = 0` means "current/RAM operations". When a segment is snapshotted to DISK, its WAL entries get `segment_id = N` (the segment number). This allows cumulative filtering:

- DISK_0: filter by `WHERE segment_id IN (0)` (or `segment_id = 0` if WAL_0 empty)
- DISK_1: filter by `WHERE segment_id IN (0, 1)`
- DISK_N: filter by `WHERE segment_id IN (0, 1, ..., N)`

### WAL Flags

| Flag | Value | Description |
|------|-------|-------------|
| ADD  | 1     | Vector added |
| DEL  | 2     | Vector deleted |
| UPD  | 4     | Vector updated |

### Search Algorithm

```
1. For each DISK segment i (parallel via rayon):
   a. Load cumulative stale IDs: WHERE segment_id <= i AND flags & (DEL|UPD) != 0
   b. filtered_search(query, k, |key| !stale_ids.contains(key))
2. Search RAM layer with segment_id = 0 stale IDs
3. Merge results from all segments + RAM
4. Deduplicate by chunk_id, keep most recent
```

### Compaction Algorithm

Triggered by heuristic (background):
- Stale vectors > `compaction_stale_threshold` % of total

Process:
1. Read all segments and WAL
2. Identify stale vectors (cumulative DEL|UPD flags across all segments)
3. Create new segments with only live vectors (sliced by max_segment_vectors)
4. Replace old segments with new ones
5. Clear WAL (all segment_ids)

### Config

```yaml
vectors:
  engine: usearch
  usearch:
    max_segment_vectors: 1000000      # Vectors per segment after compaction
    compaction_stale_threshold: 30    # % stale vectors to trigger compaction
    search_threads: 4                 # Parallel search threads (rayon)
```

## Files Modified

- `crates/vectors/src/usearch_engine.rs` — WAL integration, parallel search, compaction
- `crates/db/migrations/knowledge/3-usearch-vectors-log/up.sql` — WAL table with segment_id
- `crates/config/src/preset.rs` — WAL config struct
- `crates/vectors/src/lib.rs` — WAL config passthrough

## Non-goals

- Changing VectorIndex trait
- Modifying LanceEngine
- MCP tool surface changes
