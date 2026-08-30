# vector-index Specification (delta)

## ADDED Requirements

### Requirement: Выбор ANN-движка (lance | usearch)

Крейт `vectors` поддерживает два ANN-движка за одним трейтом `VectorIndex` с идентичной семантикой:
`LanceEngine` (IvfHnswSq, ADR 0003) и `UsearchEngine` (usearch v2.26.1, C++/cxx FFI, U8-квантование,
disk-backed mmap). Выбор — гибридный: compile-time через Cargo-фичи `engine-lance` (default) и
`engine-usearch`, и runtime через поле `vectors.engine` (`"lance"` | `"usearch"`) в конфиге.
`open_vectors_engine()` диспетчеризует через `enum VectorEngine` и возвращает `Arc<dyn VectorIndex>`
без изменения сигнатуры, поэтому `search`/`ingestion`/`mcp` не зависят от конкретного движка.
Если обе фичи включены, а `vectors.engine` не задан — используется `lance` (обратная совместимость);
невалидное значение — явная ошибка. В период сравнения default-фичи включают оба движка; после
решения default сужается до выбранного.

#### Scenario: Дефолт без поля engine
- **WHEN** пресет не задаёт `vectors.engine`, а включены обе фичи
- **THEN** инстанцируется `LanceEngine`

#### Scenario: Явный выбор usearch
- **WHEN** `vectors.engine = "usearch"` и включена фича `engine-usearch`
- **THEN** инстанцируется `UsearchEngine` с U8-квантованием и L2sq-метрикой

#### Scenario: Невалидное значение
- **WHEN** `vectors.engine = "foo"`
- **THEN** `open_vectors_engine` возвращает явную ошибку конфигурации

#### Scenario: Фича не включена
- **WHEN** `vectors.engine = "usearch"`, но фича `engine-usearch` выключена
- **THEN** возвращается явная ошибка (движок недоступен в данной сборке)

## MODIFIED Requirements

### Requirement: Производственные гейты

Гейты ADR 0003 (p95 латентности поиска < 10 ms, recall@10 ≥ 0.95 против exact-L2 brute force,
RSS-delta ≤ ~2 GB) подтверждаются машинно и остаются обязательными для `LanceEngine`. Для
параллельного `UsearchEngine` действуют относительные пороги сравнения (решение человека 2026-08-29):
recall@k в пределах **2%** от базовой lance, p50 поиска в пределах **20%**, p95 в пределах **30%** на
тех же корпусах/фикстурах; дельта размера релизного бинаря при сборке только usearch < 5 МБ. Оба
движка реализуют один и тот же трейт `VectorIndex` с идентичной семантикой (L2-ранжирование,
каскадный протокол D3).

#### Scenario: Гейт recall на синтетике
- **WHEN** на seeded синтетическом корпусе CI-масштаба выполняется пакет запросов с известным brute-force ground truth
- **THEN** recall@10 ≥ 0.95 (для LanceEngine; для UsearchEngine — в пределах 2% от базовой lance)

#### Scenario: Гейт латентности
- **WHEN** измеряется латентность поиска на прогретом индексе в release-профиле
- **THEN** p95 < 10 ms (для LanceEngine; для UsearchEngine — в пределах 30% от lance)

#### Scenario: Гейт recall usearch относительно lance
- **WHEN** на общих фикстурах parity-harness считает recall@k для обоих движков
- **THEN** recall@k usearch отличается от lance не более чем на 2% (абсолютно)
