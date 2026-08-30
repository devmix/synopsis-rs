# Design: usearch-wal-persistence

## Architecture

### Two-Layer RAM/DISK with WAL

```
┌─────────────────────────────────────────────────────────┐
│                    UsearchEngine                         │
├─────────────────────────────────────────────────────────┤
│  RAM Layer (mutable)                                    │
│  ├── usearch Index (live inserts/updates/deletes)      │
│  └── WAL_0 (binary: chunk_id | flags)                  │
├─────────────────────────────────────────────────────────┤
│  DISK Layer 0 (read-only mmap)                         │
│  ├── index.usearch.0 (persisted snapshot)              │
│  └── WAL_0.bin (operations since snapshot)             │
├─────────────────────────────────────────────────────────┤
│  DISK Layer 1 (read-only mmap)                         │
│  ├── index.usearch.1 (older snapshot)                  │
│  └── WAL_1.bin                                        │
└─────────────────────────────────────────────────────────┘
```

### WAL Format (Binary)

```
Record:
  [u32 LE chunk_id]   — 4 bytes
  [u8 flags]          — 1 byte
    bit 0: ADD (vector added)
    bit 1: DEL (vector deleted)
    bit 2: UPD (vector updated)
  [f32 LE × dim]      — 4 × dim bytes (only for ADD/UPD)
```

### Config Parameters

```yaml
vectors:
  engine: usearch
  wal:
    ram_threshold_mb: 512        # Flush RAM to DISK when exceeded
    disk_count_threshold: 5      # Compact when DISK layers > this
    wal_size_threshold_pct: 50   # Compact when WAL > % of index size
```

### Search Fan-Out

1. Search RAM layer (unfiltered)
2. Search DISK_0 (filter by WAL_0: skip deleted/updated IDs)
3. Search DISK_1 (filter by WAL_0 + WAL_1)
4. ... (fan-out to all DISK layers)
5. Deduplicate by chunk_id, keep most recent

### Flush (RAM → DISK)

Triggered when RAM index size > `ram_threshold_mb`:
1. Call `build_index()` on RAM index (= save to disk)
2. Create new DISK layer with sequential ID
3. Clear RAM WAL
4. Rename saved file to `index.usearch.<N>`

### Compaction

Triggered when:
- DISK layer count > `disk_count_threshold`, OR
- WAL total size > `wal_size_threshold_pct` of index size

Process:
1. Take last two DISK layers (N-1, N)
2. Merge using SQLite as f32 source (not usearch export, which is lossy)
3. Write merged index to new DISK layer
4. Delete old layers and their WALs

## D6: SQLite as Source of Truth

Vectors are rebuilt from chunk text, not exported from usearch. The usearch index is a cache加速器; SQLite is the source of truth. This means:
- `build_index()` is not called during serving
- Periodic rebuild from SQLite ensures consistency
- WAL provides durability between rebuilds

## Files Modified

- `crates/vectors/src/usearch_engine.rs` — WAL layer management
- `crates/config/src/preset.rs` — WAL config struct
- `crates/vectors/src/lib.rs` — WAL config passthrough

## Non-goals

- Changing VectorIndex trait
- Modifying LanceEngine
- MCP tool surface changes
