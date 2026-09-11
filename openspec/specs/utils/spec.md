# utils Specification

## Purpose

A shared utility crate for cross-crate helpers that do not belong to a domain crate; a leaf in the dependency graph. The temporal module is the project's single point of date/time handling, on top of jiff.

## Requirements

### Requirement: Shared utility crate

The `crates/utils` crate is the home for cross-crate helpers; a leaf dependency (it depends on no internal crates, and any crate may use it). The `temporal` module is the single source of date/time handling; hand-written implementations of calendar math and timestamp parsing are forbidden. The `text` module is the single source of script-based word stemming: given a word it detects the dominant script (Latin → English Porter stem, Cyrillic → Russian Snowball stem, any other script → the lowercased word unchanged) and returns the Snowball stem; multi-word names are stemmed word by word and rejoined with single spaces. Stemming input is always lowercased before stemming. No other crate implements stemming or script detection locally.

#### Scenario: Single entry point
- **WHEN** any crate needs date conversion/parsing/formatting
- **THEN** `utils::temporal` is used, not local helpers

#### Scenario: Stemming entry point
- **WHEN** any crate (ingestion resolution, linker candidate generation) needs to stem a word or name
- **THEN** `utils::text` is used, not a local stemmer or a direct dependency on the stemming library

#### Scenario: Script fallback
- **WHEN** a word is written in a script without a configured stemmer (e.g. CJK)
- **THEN** it is returned lowercased and unchanged (identity), not an error

### Requirement: Temporal API on top of jiff

The temporal module wraps jiff thinly (default-features off, std only): `now_rfc3339`, `format_rfc3339(SystemTime)`, `format_backup_stamp(SystemTime)` (template `%Y-%m-%dT%H-%M-%S-<ms>` UTC for VACUUM snapshot names, infallible with a clamp to the epoch), `normalize_to_rfc3339(&str)` (accepts RFC3339 and the SQLite layout `YYYY-MM-DD HH:MM:SS` as UTC), `parse_epoch_seconds(&str) -> Option<i64>`. Invalid/empty input → None without panics; the shape gate preserves the strict acceptance set of the replaced hand-written parser.

#### Scenario: Two accepted formats
- **WHEN** `2026-08-25T10:00:00Z` and `2026-08-25 10:00:00` are given as input
- **THEN** both normalize to one RFC3339 output / one epoch result

#### Scenario: Garbage input
- **WHEN** an empty string or a non-date is given as input
- **THEN** None is returned, with no panic

### Requirement: Full migration coverage

All project code uses utils::temporal for dates; no hand-written implementations remain anywhere in the workspace (search enrich/rerank, ingestion parsers/backup, embedding library — migrated, output strings are byte-identical, consumer assertions were not changed).

#### Scenario: Grep gate for hand-written code
- **WHEN** the workspace is checked for identifiers of hand-written implementations (civil_from_days, rfc3339_from_secs, etc.)
- **THEN** there are no matches in the code
