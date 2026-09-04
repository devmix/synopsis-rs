# ADR 0001 — SQLite/FTS5 seam in Rust (bundled, source-compiled)

**Status:** GO on the seam; all recorded bm25 expectations confirmed (the original `knowledge OR RAG` → 3 hits was corrected to 18 by human decision — see "Open questions").
**Date:** 2026-08-18 · **Change:** native-seam-spikes, task 1.1 (spike S1: `crates/spikes/src/bin/s1_sqlite.rs`, `crates/spikes/migrations/1-init/up.sql`)

## Question

Can a Rust binary own the full SQLite/FTS5 path without CGO flags: a source-compiled bundled SQLite, initialization of a FRESH database by a single init migration, applying the PRAGMAs from `configs/config.default.yaml` (including WAL), and bm25 results identical to the recorded expectations? Which migration mechanism to freeze for the production `db` crate?

## Options

1. **rusqlite + libsqlite3-sys `bundled`** — SQLite is compiled from source directly into the binary; FTS5 ships with every bundled build (there is no separate feature anymore); there are no CGO flags anywhere, and an entire class of silent degradation ("no fts5 module → silently degrade") is excluded by construction.
2. rusqlite with system SQLite (pkg-config) — rejected: build reproducibility across the 5 CI targets (musl / windows-gnu / darwin) is lost, and the "one local binary, no external dependencies" contract is violated.
3. SQLite vector extensions (vec0 and the like) — outside the S1 seam: legacy vec0 data is not read at all in Rust (vectors are rebuilt from chunk text; data-schema / design D2).

## Measurements (spike s1_sqlite, run 2026-08-18)

- Bundled SQLite: **3.53.2** (source id `d6e03d8c…`); the single libsqlite3-sys instance in Cargo.lock.
- Fresh DB via `rusqlite_migration::from_directory` (include_dir, SQL embedded at compile time): `PRAGMA user_version == 1` ✓; the `_schema_migrations` table is NOT created.
- Schema diff fresh vs fixture (`fixtures/knowledge.db`, v5, provenance and sha256 — fixtures/README.md), legacy artifacts excluded by an explicit list: **14 tables (63 columns), 25 indexes, 3 triggers** — identical.
- PRAGMAs from `configs/config.default.yaml` applied without errors; read-back: `journal_mode=wal` ✓, `synchronous=NORMAL(1)`, `cache_size=-64000 KiB`, `mmap_size=268435456`.
- bm25 parity (270 chunks copied read-only from the fixture into the fresh DB; FTS content populated by the sync triggers fixed in the init DDL):

| Query | Recorded expectation | Measured | Verdict |
|---|---|---|---|
| `knowledge` | 17 hits; top-3 = (247, −4.4453), (30, −3.573), (106, −3.4522) | 17 hits; (247, −4.4453), (30, −3.5730), (106, −3.4522) | ✓ exact chunk_id/order match, scores within relative tolerance 1e-3 |
| `RAG` | 1 hit | 1 hit (chunk 88, score −2.2573) | ✓ |
| `"knowledge graph"` (phrase) | 1 hit | 1 hit (chunk 1, score −5.2473) | ✓ |
| `knowledge OR RAG` (operators) | **3 hits** (corrected to 18 by human decision, task 1.1 revision 4) | **18 hits** (= union of 17 + 1; no chunk contains both terms) | ✓ expectation corrected — "3 hits" was an artifact of a measurement with `LIMIT 3` (first rows: 1, 30, 88); see "Open questions" |

## Decision (GO)

The seam is proven: a source-compiled bundled SQLite with FTS5/bm25 works fully in Rust without CGO flags; the schema and ranking match the recorded expectations. **Verdict: GO** — the `db` crate is built on rusqlite + bundled libsqlite3-sys.

### Migration mechanism (frozen for the `db` crate)

- Mechanism = `rusqlite_migration` 2.6 (`from-directory`; SQL embedded into the binary at compile time via include_dir).
- A SINGLE init directory `migrations/1-init/up.sql` with the squashed DDL of the final v5 state: derived mechanically from the fixture schema (sqlite_master + PRAGMA table_info) minus an explicit list of legacy artifacts (`_schema_migrations`, the `chunks_vec*` vec0 family); the final v5 state corresponds to the legacy migration history 001–005 (003 drops `documents.domain` and its index; 002 adds the unique index `idx_documents_original_path`; 005 — `app_kv`).
- `PRAGMA user_version` — the ONLY source of truth for schema state (== 1 after init). The `_schema_migrations` table is neither created nor maintained; legacy-compatible tracking is not supported.
- Future migrations: new numbered directories `<id>-<slug>/up.sql`, forward-only, no down.sql needed, shipped files are never edited.

### PRAGMA/WAL notes

- `journal_mode=WAL` applies on the bundled SQLite without errors and reads back as `wal`; `-wal`/`-shm` sidecar files appear next to the DB — deployment assumes a writable data directory.
- `synchronous=NORMAL` — the correct pairing with WAL for local single-writer use (laptop, no external services).
- `cache_size=-64000` (negative value = KiB) and `mmap_size=256 MB` apply and read back without deviation.

### rusqlite pin deviation (explanation, task 0.1 revision 2)

The frozen entry `rusqlite (bundled + fts5)` in openspec/config.yaml is **not resolvable** verbatim: starting with rusqlite 0.40 / libsqlite3-sys 0.38 the default build links system SQLite via pkg-config, and source-compiled bundling moved into the `bundled` feature of **libsqlite3-sys** itself; cargo forbids enabling transitive features without a re-export. The real pin:

```toml
rusqlite = "0.40"
libsqlite3-sys = { version = "0.38", features = ["bundled"] }
```

FTS5 is not a separate feature — it ships with every bundled build (empirically: `bm25()` works, a single libsqlite3-sys 0.38.x instance in Cargo.lock, no CGO flags anywhere). The intent of the frozen entry (D3) is preserved; the config.yaml text is not edited by the design D8 precedent — the deviation is recorded here and in task 0.1 revision 2.

## Rejected alternatives (migration mechanism)

- **Copy the legacy migrations 001–005 as-is** (rejected by the human, task 1.1 revision 3): replaying dead history onto a clean DB is pointless; 002/004 are data migrations with nothing to apply on empty tables; the end state is fully described by a single init DDL.
- **Bridge/double-write `_schema_migrations` + user_version** (rejected by the human, design D6): legacy-compatible tracking is explicitly not required — the legacy knowledge.db is never opened, upgraded, or migrated (task 1.1 revision 2: Rust always builds the DB from scratch).

## Open questions (closed)

- ~~The recorded expectation `knowledge OR RAG` → **3 hits** contradicts the measured **18**: the fixture has exactly 17 chunks containing the term "knowledge" and exactly 1 (id 88) containing "rag", with no overlap; under FTS5 boolean semantics (`OR` = union) the result for this file is mathematically determined — 18.~~ — **Resolved by human decision on 2026-08-18 (task 1.1 revision 4):** the original "3 hits" was an artifact of a measurement with `LIMIT 3` (first rows: 1, 30, 88); the expectation was corrected to **18** (= union of 17 + 1, no overlap). Verified with two independent SQLite builds (bundled 3.53.2 and the system sqlite3 CLI) and via the search code path (the query is passed to `chunks_fts MATCH ?` unmodified); alternative interpretations (AND=0, NEAR=0, wildcard=19, distinct docs=10) do not yield 3. The other expectations match to 4 significant digits of the bm25 score — they were recorded from this very fixture file. Spike s1_sqlite now exits with code 0 on all checks.
