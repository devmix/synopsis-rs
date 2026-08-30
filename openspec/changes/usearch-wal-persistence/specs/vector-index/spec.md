# vector-index Specification (delta)

## MODIFIED Requirements

### Requirement: Жизненный цикл индекса

Крейт `vectors` обеспечивает создание, открытие, персистентность и пересоздание ANN-индекса в каталоге данных. Индекс создаётся под фиксированную размерность (по умолчанию 1024 для bge-m3); открытие несуществующего индекса — различимая ошибка; пересоздание атомарно заменяет содержимое. Повторное открытие существующего индекса не перестраивает его. UsearchEngine поддерживает WAL (Write-Ahead Log) для durability вставок между rebuild.

#### Scenario: Создание нового индекса
- **WHEN** создаётся индекс в пустом каталоге данных
- **THEN** создаётся пустое хранилище с заданной размерностью, готовое к вставке векторов

#### Scenario: Открытие существующего индекса
- **WHEN** открывается ранее сохранённый индекс
- **THEN** хранилище открывается без перестроения, все ранее вставленные векторы доступны поиску

#### Scenario: Открытие несуществующего индекса
- **WHEN** открывается индекс, отсутствующий в каталоге данных
- **THEN** возвращается явная ошибка «индекс не существует»

#### Scenario: Пересоздание индекса
- **WHEN** выполняется пересоздание индекса
- **THEN** прежнее содержимое полностью заменяется новым набором векторов без накопления мусора

#### Scenario: WAL flush при превышении порога
- **WHEN** размер RAM-индекса превышает `wal.ram_threshold_mb` (default 512 MB)
- **THEN** RAM-индекс сохраняется на DISK как read-only mmap слой, RAM очищается

#### Scenario: Поиск с WAL фильтрацией
- **WHEN** выполняется поиск по индексу с DISK-слоями
- **THEN** результаты фильтруются по WAL каждого слоя (пропуск удалённых/обновлённых ID)

#### Scenario: Компактификация
- **WHEN** количество DISK-слоёв превышает `wal.disk_count_threshold` (default 5)
- **THEN** последние два слоя мёрджатся в один, старые удаляются

#### Scenario: Persistence при restart
- **WHEN** индекс перезапускается после краша
- **THEN** WAL replay восстанавливает consistency между RAM и DISK слоями

### Requirement: Производственные гейты

Гейты ADR 0003 (p95 латентности поиска < 10 ms, recall@10 ≥ 0.95 против exact-L2 brute force, RSS-delta ≤ ~2 GB) подтверждаются машинно на корпусах до класса N≈250K × 1024-dim включительно (спайк s3b_lance и интеграционные гейты CI-масштаба). На экстраполяционной точке N=1M измеренное отклонение принято человеком как задокументированный worst-case (решение 2026-08-21, вариант 1): p95 32–55 ms, recall@10 0.911–0.933 при дефолтных параметрах; recall при этом лимитируется efSearch (луч HNSW внутри партиции), а не IVF-покрытием. efSearch/nprobes остаются runtime-настройками для баланса «точность/латентность» без перестроения индекса. Индекс квантованный и disk-backed (u8 scalar quantization внутри IvfHnswSq; исходные f32 — в таблице), параметры: M=16, efConstruction=100, num_partitions=256, метрика L2.

#### Scenario: Гейт recall на синтетике
- **WHEN** на seeded синтетическом корпусе CI-масштаба выполняется пакет запросов с известным brute-force ground truth
- **THEN** recall@10 ≥ 0.95

#### Scenario: Гейт латентности
- **WHEN** измеряется латентность поиска на прогретом индексе в release-профиле
- **THEN** p95 < 10 ms

### Requirement: Конфигурация индекса

Параметры индекса конфигурируются: структура конфигурации в крейте `vectors` с дефолтами ADR 0003 (M=16, efConstruction=100, num_partitions=256, nprobes=32, efSearch=200, L2, размерность 1024); опциональная секция `vectors:` в config preset (аддитивное расширение config-format, решение человека 2026-08-21) прокидывает переопределения; пресет без секции даёт дефолты. Секция `vectors.wal:` содержит параметры WAL persistence.

#### Scenario: Дефолты без секции
- **WHEN** пресет конфигурации не содержит секцию `vectors`
- **THEN** используются дефолты ADR 0003

#### Scenario: Переопределение runtime-параметров
- **WHEN** секция `vectors` задаёт efSearch/nprobes
- **THEN** поиск использует переопределённые значения без перестроения индекса

#### Scenario: WAL config defaults
- **WHEN** секция `vectors.wal` отсутствует в конфиге
- **THEN** применяются дефолты: ram_threshold_mb=512, disk_count_threshold=5, wal_size_threshold_pct=50

#### Scenario: WAL config override
- **WHEN** секция `vectors.wal` задаёт `ram_threshold_mb: 1024`
- **THEN** используется значение 1024 MB для порога сброса

#### Scenario: Invalid WAL config
- **WHEN** `wal.ram_threshold_mb: 0` или `wal.wal_size_threshold_pct: 101`
- **THEN** возвращается ошибка валидации

### Requirement: Выбор ANN-движка (lance | usearch)

Крейт `vectors` поддерживает два ANN-движка за одним трейтом `VectorIndex` с идентичной семантикой:
`LanceEngine` (IvfHnswSq, ADR 0003) и `UsearchEngine` (usearch v2.26.1, C++/cxx FFI, bf16-квантование,
disk-backed). Выбор — гибридный: compile-time через Cargo-фичи `engine-lance` и `engine-usearch`
(default), и runtime через поле `vectors.engine` (`"lance"` | `"usearch"`) в конфиге.
`open_vectors_engine()` диспетчеризует через `enum VectorEngine` и возвращает `Arc<dyn VectorIndex>`
без изменения сигнатуры, поэтому `search`/`ingestion`/`mcp` не зависят от конкретного движка.
Если обе фичи включены, а `vectors.engine` не задан — используется `usearch` (default);
невалидное значение — явная ошибка.

#### Scenario: Дефолт без поля engine
- **WHEN** пресет не задаёт `vectors.engine`, а включены обе фичи
- **THEN** инстанцируется `UsearchEngine`

#### Scenario: Явный выбор usearch
- **WHEN** `vectors.engine = "usearch"` и включена фича `engine-usearch`
- **THEN** инстанцируется `UsearchEngine` с bf16-квантованием и L2sq-метрикой

#### Scenario: Невалидное значение
- **WHEN** `vectors.engine = "foo"`
- **THEN** `open_vectors_engine` возвращает явную ошибку конфигурации

#### Scenario: Фича не включена
- **WHEN** `vectors.engine = "usearch"`, но фича `engine-usearch` выключена
- **THEN** `open_vectors_engine` возвращает ошибку «движок недоступен в данной сборке»
