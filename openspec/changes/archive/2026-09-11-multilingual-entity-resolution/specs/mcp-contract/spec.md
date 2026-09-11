# mcp-contract Specification

## MODIFIED Requirements

### Requirement: Tool parameter and response schemas

For each tool, the field names, types, and optionality of the parameters from
the JSON schemas (tools/list) and the result structure (field names, typing,
presence/absence of optional fields for the same inputs) are fixed by this
contract and the recorded fixtures.

The entity payload returned by `get_entity_dossier` (the dossier entity),
`catalog_entities` (entity entries), and `search` (entities attached to
result chunks) SHALL include an additive `aliases` field: an array of
strings, empty when the entity has no aliases, listing every surface name
that has resolved to the entity (the entity's own name is not repeated in
the list). Parameter schemas are unchanged by this addition. Recorded
fixtures covering these tools are re-recorded to include the field.

#### Scenario: Tool JSON schemas
- **WHEN** a machine-diff of the Rust server's tools/list response against the fixture recorded once on the same knowledge.db (with description normalization) is run
- **THEN** the parameter schemas of each of the 12 tools are identical

#### Scenario: Response parity on identical data
- **WHEN** a Rust server tool is called with the arguments from a fixture (recorded once on the same knowledge.db)
- **THEN** the result matches the response recorded in the fixture in structure and content, except for fields explicitly marked approximate (ANN scores, see data/search specs), where a deviation within the recall gate is allowed

#### Scenario: Aliases field present
- **WHEN** an entity has recorded aliases and any of `get_entity_dossier`, `catalog_entities`, or `search` returns that entity
- **THEN** the entity payload carries the `aliases` array with the recorded surface names; for an entity without aliases the array is present and empty
