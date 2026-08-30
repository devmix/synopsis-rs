# Design: usearch-wal-persistence

## Key Decisions (Human 2026-08-31)

1. **WAL in SQLite** — `usearch_vectors_log` table for transactional integrity with chunks
2. **DISK_N segments** — multiple read-only mmap segments (as designed)
3. **Global compaction** — merges ALL segments, not just last two
4. **Parallel search** — search across segments in parallel (configurable threads)
5. **Compaction heuristics** — background compaction when stale vectors > 30%

## Architecture

### Storage Model

```
┌─────────────────────────────────────────────────────────┐
│  SQLite (source of truth)                               │
│  ├── chunks (chunk_id, text, metadata)                  │
│  └── usearch_vectors_log (WAL)                          │
│      ├── chunk_id (u32)                                 │
│      ├── flags (u8: ADD=1, DEL=2, UPD=4)               │
│      ├── vector (BLOB: f32 × dim)                       │
│      └── created_at (timestamp)                         │
├─────────────────────────────────────────────────────────┤
│  DISK Segment 0 (read-only mmap)                       │
│  └── index.usearch.0                                    │
├─────────────────────────────────────────────────────────┤
│  DISK Segment 1 (read-only mmap)                       │
│  └── index.usearch.1                                    │
├─────────────────────────────────────────────────────────┤
│  ...                                                    │
├─────────────────────────────────────────────────────────┤
│  RAM Layer (ephemeral)                                 │
│  └── usearch Index (rebuilt from SQLite + WAL replay)   │
└─────────────────────────────────────────────────────────┘
```

### WAL Table Schema

```sql
CREATE TABLE usearch_vectors_log (
    chunk_id    INTEGER PRIMARY KEY,
    flags       INTEGER NOT NULL,  -- ADD=1, DEL=2, UPD=4
    vector      BLOB,              -- f32 × dim (NULL for DEL)
    created_at  TEXT NOT NULL       -- ISO 8601 timestamp
);

CREATE INDEX idx_usearch_vectors_log_flags ON usearch_vectors_log(flags);
```

### WAL Flags

| Flag | Value | Description |
|------|-------|-------------|
| ADD  | 1     | Vector added |
| DEL  | 2     | Vector deleted |
| UPD  | 4     | Vector updated |

### Search Algorithm

```
1. Load DISK segments (mmap, parallel)
2. For each segment in parallel (configurable threads):
   a. Search segment (HNSW)
   b. Filter by WAL: exclude chunk_ids with flags & (DEL|UPD)
   c. Return results with segment_id
3. Merge results from all segments
4. Deduplicate by chunk_id, keep most recent
5. Apply WAL to RAM results (exclude deleted/updated)
```

### Compaction Algorithm

Triggered by heuristics (background):
- Stale vectors > 30% of total (configurable)
- OR WAL size > threshold
- OR segment count > threshold

Process:
1. Read all segments and WAL
2. Identify stale vectors (chunk_ids with DEL flag or missing from SQLite)
3. Create new segment with only live vectors (sliced by 1M)
4. Replace old segments with new ones
5. Clear WAL

### Config

```yaml
vectors:
  engine: usearch
  wal:
    max_segment_vectors: 1000000      # Vectors per segment after compaction
    compaction_stale_threshold: 30    # % stale vectors to trigger compaction
    search_threads: 4                 # Parallel search threads
```

## Files Modified

- `crates/vectors/src/usearch_engine.rs` — WAL integration, parallel search
- `crates/db/src/migrations.rs` — WAL table schema
- `crates/config/src/preset.rs` — WAL config struct
- `crates/vectors/src/lib.rs` — WAL config passthrough

## Non-goals

- Changing VectorIndex trait
- Modifying LanceEngine
- MCP tool surface changes
