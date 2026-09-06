# entity-extraction Specification

## ADDED Requirements

### Requirement: Effective domain schema (two-layer)

The LLM NER prompt and JSON schema render from the effective domain schema:
the domain's own definitions plus the global ontology pool — entities
shadowed by id, relations by predicate, extraction rules by rule id — with
the domain winning silently. The regex NER stage receives the merged
extraction rules.

#### Scenario: Pool types appear in the prompt
- **WHEN** a domain has no `employee` entity and the global pool defines one
- **THEN** the rendered system prompt and JSON schema include `employee` as an extractable type for that domain

#### Scenario: Shadowing
- **WHEN** the domain and the pool define an entity with the same id
- **THEN** only the domain's definition is used, without a warning
