# utils Specification

## MODIFIED Requirements

### Requirement: Shared utility crate

The `crates/utils` crate is the home for cross-crate helpers; a leaf
dependency (it depends on no internal crates, and any crate may use it). The
`temporal` module is the single source of date/time handling; hand-written
implementations of calendar math and timestamp parsing are forbidden. The
`text` module is the single source of script-based word stemming: given a
word it detects the dominant script (Latin → English Porter stem, Cyrillic →
Russian Snowball stem, any other script → the lowercased word unchanged) and
returns the Snowball stem; multi-word names are stemmed word by word and
rejoined with single spaces. Stemming input is always lowercased before
stemming. No other crate implements stemming or script detection locally.

#### Scenario: Single entry point
- **WHEN** any crate needs date conversion/parsing/formatting
- **THEN** `utils::temporal` is used, not local helpers

#### Scenario: Stemming entry point
- **WHEN** any crate (ingestion resolution, linker candidate generation) needs to stem a word or name
- **THEN** `utils::text` is used, not a local stemmer or a direct dependency on the stemming library

#### Scenario: Script fallback
- **WHEN** a word is written in a script without a configured stemmer (e.g. CJK)
- **THEN** it is returned lowercased and unchanged (identity), not an error
