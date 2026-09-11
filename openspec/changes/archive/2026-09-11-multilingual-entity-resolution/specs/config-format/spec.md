# config-format Specification

## ADDED Requirements

### Requirement: Dataset alias map (ontology `<aliases>`)

`global.xml` and each `domain_*.xml` MAY carry an optional top-level
`<aliases>` block of `<alias name="..." canonical="..."/>` entries: alias
surface name → canonical name (both non-empty strings). The block is optional
— absent everywhere means no aliases and is valid. The config crate SHALL
parse the block together with the ontology and reject the dataset
configuration on an empty `name` or `canonical` or on a duplicate `name`
(one alias may map to exactly one canonical name across all files of the
dataset). The effective dataset alias map is the flat union of the global
block and all domain blocks; the resolver consumes it as a name→canonical
map and the domain+type gate still applies at lookup. The canonical name does
NOT have to pre-exist as an entity: the first extraction of either the alias
or the canonical creates the entity, and later surface forms resolve to it.
The per-entity `<synonyms>` element is a type-level surface form and MUST NOT
be conflated with `<aliases>`.

#### Scenario: Block absent
- **WHEN** no ontology file of a dataset carries an `<aliases>` block
- **THEN** the configuration loads with an empty alias map and resolution behaves as if the feature were disabled

#### Scenario: Block present
- **WHEN** a dataset's ontology maps `alias-name` → `canonical-name`
- **THEN** the loaded configuration carries that mapping and the resolver uses it (a surface form equal to `alias-name` resolves to the entity named `canonical-name` of the same type and domain)

#### Scenario: Duplicate alias name
- **WHEN** the same `name` appears twice with different canonicals (within one file or across `global.xml` and a domain file)
- **THEN** the dataset configuration is rejected with a validation error

#### Scenario: Empty name or canonical
- **WHEN** an `<alias>` entry has an empty `name` or an empty `canonical`
- **THEN** the dataset configuration is rejected with a validation error

### Requirement: Ontology merge confidence threshold

The `<cross-domain-links>` element of `global.xml` SHALL accept an optional
`merge_confidence_threshold` value (float). When absent, the effective value
is 0.95. A value outside (0, 1] is rejected at ontology validation,
consistent with the existing `llm_confidence_threshold` handling. The
threshold is used by the within-domain cross-script merge action (see the
knowledge-graph spec): decisions at or above it merge the pair, decisions
between `llm_confidence_threshold` and it create a link only.

#### Scenario: Key absent
- **WHEN** the `<cross-domain-links>` element has no `merge_confidence_threshold`
- **THEN** the ontology validates and the effective merge threshold is 0.95

#### Scenario: Key set
- **WHEN** the ontology sets `merge_confidence_threshold` to a value in (0, 1]
- **THEN** the ontology validates and the within-domain merge action uses that value

#### Scenario: Key invalid
- **WHEN** the ontology sets `merge_confidence_threshold` to 0 or to a value above 1
- **THEN** ontology validation fails with an error naming the key
