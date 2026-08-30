# Proposal: usearch-wal-persistence

## Problem

UsearchEngine inserts are RAM-only — the serving/ingestion path never calls `build_index()` (= `save()`), so inserts are lost on restart. LanceEngine commits eagerly per batch, so this gap exists only for usearch. During the comparison period this was acceptable (rebuild-from-text repairs), but for production use we need durable persistence.

## Solution

Implement a WAL (Write-Ahead Log) persistence layer for UsearchEngine with two-layer RAM/DISK architecture:

1. **RAM layer**: mutable usearch index (live inserts/updates/deletes)
2. **DISK layers**: read-only mmap views of persisted indexes
3. **WAL per layer**: binary log of operations (chunk_id | flags[DEL,ADD,UPD])

Flush RAM to DISK when RAM > configurable threshold (default 512 MB). Search fan-out: RAM → DISK_0 → DISK_1 → ... with WAL filtering. Compaction merges last two DISKs when count > threshold or WAL size > 50%.

## Non-goals

- Changing the VectorIndex trait contract
- Modifying LanceEngine behavior
- Changing the MCP tool surface

## Decision rationale

Alternatives considered:
1. **Periodic save hook**: simpler but causes write amplification (full index save every N seconds)
2. **WAL + LSM**: more complex but optimal for SSD wear and update frequency
3. **Accept rebuild-only**: unacceptable for production use

Chose WAL + LSM because it matches the original design intent (user proposal 2026-08-30) and provides the best balance of durability, performance, and SSD wear.
