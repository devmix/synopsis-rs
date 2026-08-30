# config-format Specification (delta)

## ADDED Requirements

### Requirement: Поле vectors.engine

Секция `vectors:` (аддитивное расширение config-format, решение 2026-08-21) дополняется опциональным
полем `engine` (`"lance"` | `"usearch"`), выбирающим ANN-движок в `crates/vectors` (change
`add-usearch-ann-engine`, 2026-08-29). Поле необязательно: при отсутствии используется `lance`
(обратная совместимость). Невалидное значение вызывает ошибку разбора/валидации конфигурации.
Поле не влияет на другие секции конфига и не меняет формат пресета.

#### Scenario: Отсутствие поля
- **WHEN** пресет содержит секцию `vectors` без поля `engine`
- **THEN** используется движок `lance` по умолчанию

#### Scenario: Явное значение
- **WHEN** пресет задаёт `vectors.engine: "usearch"`
- **THEN** `vectors` инстанцирует `UsearchEngine` (при включённой фиче `engine-usearch`)

#### Scenario: Невалидное значение
- **WHEN** пресет задаёт `vectors.engine: "foo"`
- **THEN** конфигурация отклоняется с явной ошибкой
