# config-format Specification (delta)

## ADDED Requirements

### Requirement: Секция vectors.usearch

Секция `vectors:` дополняется опциональным объектом `usearch`, содержащим параметры двухслойной persistence UsearchEngine (ADR 0004): `max_segment_vectors` (usize, default 1000000), `compaction_stale_threshold` (u8, 1..=100, default 30), `search_threads` (usize, default 4). Поле необязательно: при отсутствии применяется `UsearchConfig::default()`. Невалидные значения (`max_segment_vectors: 0`, `compaction_stale_threshold` вне 1..=100, `search_threads: 0`) отклоняются на уровне парсинга с явной ошибкой.

#### Scenario: Отсутствие секции usearch
- **WHEN** пресет содержит секцию `vectors` без поля `usearch`
- **THEN** движок получает `UsearchConfig::default()` (1000000 / 30 / 4)

#### Scenario: Явная настройка usearch
- **WHEN** пресет задаёт `vectors.usearch.max_segment_vectors: 500000`
- **THEN** flush RAM-слоя выполняется при достижении 500000 векторов

#### Scenario: Невалидное значение
- **WHEN** пресет задаёт `vectors.usearch.max_segment_vectors: 0`
- **THEN** конфигурация отклоняется с явной ошибкой «must be > 0»

#### Scenario: Частичная настройка
- **WHEN** пресет задаёт `vectors.usearch.compaction_stale_threshold: 50` без остальных полей
- **THEN** `compaction_stale_threshold=50`, остальные — дефолты (1000000, 4)
