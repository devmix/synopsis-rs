# db-storage Specification (delta)

## MODIFIED Requirements

### Requirement: vec0 исключён

The `db` crate SHALL contain no vec0-table operations (SearchVector, UpsertVector,
FormatVector, DeleteVectorsByChunkIDs, etc.) — vector search lives in the `vectors`
crate (ADR 0003/0004, usearch engine); vectors are rebuilt from chunk text, the old
vec0 is never read.

#### Scenario: Отсутствие vec0-кода
- **WHEN** the db crate source is checked
- **THEN** it contains no references to vec0 tables or vec0 operations (grep check in CI)
