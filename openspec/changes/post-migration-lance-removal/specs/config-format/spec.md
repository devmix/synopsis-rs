# config-format Specification (delta)

## MODIFIED Requirements

### Requirement: Поле vectors.engine

The `vectors:` section (additive config-format extension, decision 2026-08-21)
contains the optional `engine` field. The field SHALL accept only: absent (default)
or `"usearch"`. The value `"lance"` SHALL be rejected with an explicit validation
error stating that the lance engine was removed and that usearch is the only engine
(`post-migration-lance-removal`, user decision 2026-08-31). Any other value SHALL
be rejected with an explicit parse/validation error. The field does not affect other
config sections and does not change the preset format.

#### Scenario: Отсутствие поля
- **WHEN** the preset contains a `vectors` section without the `engine` field
- **THEN** the usearch engine is used (the only engine)

#### Scenario: Явное значение
- **WHEN** the preset sets `vectors.engine: "usearch"`
- **THEN** `vectors` instantiates `UsearchEngine`

#### Scenario: Удалённый движок
- **WHEN** the preset sets `vectors.engine: "lance"`
- **THEN** the configuration is rejected with an explicit error naming the removal
  and pointing to usearch

#### Scenario: Невалидное значение
- **WHEN** the preset sets `vectors.engine: "foo"`
- **THEN** the configuration is rejected with an explicit error
