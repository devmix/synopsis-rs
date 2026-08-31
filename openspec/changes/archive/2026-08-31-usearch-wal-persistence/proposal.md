# Proposal: usearch-wal-persistence

## Problem

UsearchEngine inserts are RAM-only — the serving/ingestion path never calls `build_index()` (= `save()`), so inserts are lost on restart. LanceEngine commits eagerly per batch, so this gap exists only for usearch. The first implementation pass (tasks 2.1–2.4) left the DISK layer as dead code, the WAL unwired in production, compaction that would erase data if triggered, and the db crate test suite red — it is being reworked per ADR 0004.

## Solution

Two-layer RAM/DISK persistence architecture for UsearchEngine (full rationale, crash semantics and rejected alternatives — `docs/adr/0004-usearch-lsm-segments.md`):

1. **RAM layer**: in-memory usearch index (`restore_from_buffer`), persisted by explicit `save()` on overflow (RAM ≥ `max_segment_vectors` → flushed to a new DISK[n]) and at shutdown.
2. **DISK layers**: read-only mmap views (`restore_view`) of immutable segment files, each with a binary key-manifest sidecar (`.keys`); segment ids are monotonic, never renumbered.
3. **Per-segment WAL in SQLite** (`usearch_vectors_log`, migrations 3+4, schema unchanged): DEL-only records — "key is invalid in this segment" (deleted or superseded); supersession writes are atomic per operation (one transaction), WAL-first.
4. **Parallel search** across all layers (dedicated rayon pool, `search_threads`) via usearch `filtered_search` with a versioned in-memory stale-set cache (zero SQL in steady state); merge: duplicate chunk id → the freshest layer wins.
5. **Compaction (vacuum)**: background thread when stale vectors > `compaction_stale_threshold` % of disk total; repacks live vectors into new monotonic-id segments (sliced by `max_segment_vectors`), atomic directory swap, WAL cleaned after success.

## Frozen contracts touched

- **Config format:** the `vectors.usearch:` section (`max_segment_vectors`, `compaction_stale_threshold`, `search_threads`) is added by this change (not yet in main specs) — it stays, all three fields gain real meaning. No change to other config surfaces.
- **Data schema:** `usearch_vectors_log` table (migrations 3+4) is new Rust-operational, no Go-oracle equivalent — no parity required. Schema unchanged by the rework; only db-crate tests are fixed (user_version 3 → 4).
- **MCP tools / CLI surface:** untouched.
- **VectorIndex trait:** one additive default method `maybe_compact()` (all existing implementations keep compiling) — explicit decision, ADR 0004 §9.

## Non-goals

- Breaking changes to the VectorIndex trait contract (additive method only)
- Modifying LanceEngine behavior
- Changing the MCP tool surface
- Vector replay from WAL ("WAL without vectors": payloads live in the chunks table; the RAM crash-loss window is documented, repaired by consumer reconciliation / `rebuild`)

## Decision rationale

Alternatives considered (full analysis in ADR 0004 "Варианты"):
1. **Global cumulative WAL** (original design of this change: ADD/DEL/UPD + `segment_id <= n` filtering + WAL rebinding on flush) — rejected: WAL grows to corpus size, fragile rebinding, cumulative reads on the query path, UPD does not cover re-insert
2. **Self-contained per-segment WAL (DEL-only) + key-manifest sidecars** — chosen (ADR 0004)
3. **Binary per-segment WAL files** — rejected: human decision 2026-08-31 keeps the WAL in SQLite for transactional integrity
4. **Periodic full save** — rejected: O(N²) write amplification
