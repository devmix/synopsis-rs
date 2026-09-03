## ADDED Requirements

### Requirement: chunk metadata_json column (explicit v5 deviation)
The `chunks` table SHALL carry a `metadata_json TEXT` (nullable) column holding the per-chunk metadata bag as raw JSON: the chunk-specific keys the chunker computed (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …). It SHALL be stored as raw text (the `documents.metadata_json` pattern), parsed on demand, and `NULL` SHALL mean "no chunk metadata". This is an explicit, justified deviation from the oracle v5 shape (the Go `chunks` table has no metadata column): the Rust database is always built from scratch (no legacy `knowledge.db` is opened or migrated), and the column restores a field that was in the original Rust design and surfaces the section context in search. The invariant-preserving `chunk_text`, the byte offsets, and the `search_text` re-point are unchanged; `PRAGMA user_version` stays 1.

#### Scenario: Fresh build includes metadata_json
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a nullable `metadata_json` column and `PRAGMA user_version` = 1

#### Scenario: Round-trip
- **WHEN** a chunk is created with a `metadata_json` value
- **THEN** a row read returns the same value, and a chunk created without one returns `NULL`
