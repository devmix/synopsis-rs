# vector-index Specification

## Purpose

Локальный ANN-индекс эмбеддингов на disk-backed/квантованном движке: хранение векторов чанков, kNN-поиск в RAM-бюджете ноутбука, каскадная согласованность с SQLite-хранилищем чанков и формат фикстур для машинного паритета.

## Requirements

### Requirement: Жизненный цикл индекса

Крейт `vectors` обеспечивает создание, открытие, персистентность и пересоздание ANN-индекса в каталоге данных. Индекс создаётся под фиксированную размерность (по умолчанию 1024 для bge-m3); открытие несуществующего индекса — различимая ошибка; пересоздание атомарно заменяет содержимое. Повторное открытие существующего индекса не перестраивает его. UsearchEngine поддерживает двухслойную архитектуру RAM/DISK с per-segment WAL в SQLite для durability вставок между rebuild (ADR 0004).

#### Scenario: Создание нового индекса
- **WHEN** создаётся индекс в пустом каталоге данных
- **THEN** создаётся пустое хранилище с заданной размерностью, готовое к вставке векторов

#### Scenario: Открытие существующего индекса
- **WHEN** открывается ранее сохранённый индекс
- **THEN** RAM-слой восстанавливается из снапшота, DISK-слои — как read-only mmap-виды, все вставленные векторы доступны поиску

#### Scenario: Открытие несуществующего индекса
- **WHEN** открывается индекс, отсутствующий в каталоге данных
- **THEN** возвращается явная ошибка «индекс не существует»

#### Scenario: Пересоздание индекса
- **WHEN** выполняется пересоздание индекса
- **THEN** WAL очищается, файлы DISK-слоёв удаляются, RAM заменяется новым набором векторов — без накопления мусора

#### Scenario: Flush при переполнении RAM
- **WHEN** размер RAM-индекса достигает `vectors.usearch.max_segment_vectors`
- **THEN** RAM-слой сохраняется как новый DISK-сегмент (read-only mmap), WAL сегмента RAM очищается, RAM сбрасывается в пустой

#### Scenario: Поиск с WAL-фильтрацией
- **WHEN** выполняется поиск по индексу с DISK-слоями
- **THEN** результаты каждого слоя фильтруются по stale-множеству (DEL-записи WAL: удалённые и superseded ключи); дубликаты между слоями разрешаются в пользу свежего слоя

#### Scenario: Компактификация
- **WHEN** доля устаревших векторов в DISK-слоях превышает `vectors.usearch.compaction_stale_threshold`
- **THEN** фоновая компакция упаковывает live-векторы в новые сегменты (монотонные id), каталог сегментов меняется атомарно, WAL очищается после смены

#### Scenario: Persistence при restart
- **WHEN** индекс перезапускается после краша
- **THEN** WAL-строки несуществующих сегментов удаляются (self-healing), мусорные файлы каталога очищаются; вставки RAM с последнего flush/shutdown-save теряются (документированное окно, repair — consumer-реконсиляция или `rebuild`)

### Requirement: Вставка и kNN-поиск

Крейт `vectors` принимает готовые пары `(chunk_id, вектор)` — крейт НЕ вызывает модель эмбеддингов (запросный путь и путь вставки не загружают модель). Вставка стриминговая (батчами). Поиск возвращает top-k ближайших `(chunk_id, distance)` по метрике L2; ранжирование — по distance. Параметр поиска efSearch — runtime-настройка с дефолтом из ADR 0003 (efSearch=200); IVF-only-поле nprobes удалено вместе с lance-движком (`post-migration-lance-removal`, 2026-08-31).

#### Scenario: Вставка и поиск
- **WHEN** вставлен набор векторов и выполняется поиск по запросному вектору
- **THEN** возвращается до k пар `(chunk_id, distance)`, отсортированных по возрастанию distance

#### Scenario: Размерность не совпадает
- **WHEN** вставляется или ищется вектор размерности, отличной от размерности индекса
- **THEN** возвращается явная ошибка размерности

#### Scenario: Пустой индекс
- **WHEN** поиск выполняется по пустому индексу
- **THEN** возвращается пустой результат без ошибки

### Requirement: Каскадное удаление и синхронизация с SQLite

Векторы соответствуют чанкам SQLite один-к-одному по chunk_id; обе БД (SQLite, векторный индекс) должны быть синхронны всегда (решение человека 2026-08-21). Крейт `vectors` предоставляет удаление векторов по списку chunk_id (батчево, идемпотентно к отсутствующим id) и перечисление всех chunk_id индекса для сверки. Протокол каскада при удалении чанков: сначала удаляются векторы по chunk_id, затем строки чанков в SQLite — сбой между шагами оставляет восстановимое состояние (осиротевшие векторы вычищаются сверкой; недостающие векторы восстанавливаются ре-эмбеддингом).

#### Scenario: Удаление векторов чанка
- **WHEN** удаляются векторы по chunk_id удалённого чанка
- **THEN** векторы исчезают из индекса и не возвращаются поиском; повторный вызов с теми же id не является ошибкой

#### Scenario: Порядок каскада
- **WHEN** потребитель удаляет документ с чанками
- **THEN** удаление векторов выполняется ДО удаления строк чанков в SQLite (контракт порядка фиксируется в документации крейта)

#### Scenario: Сверка индекса с SQLite
- **WHEN** выполняется сверка (reconciliation)
- **THEN** перечисление chunk_id индекса позволяет найти осиротевшие векторы (id отсутствуют в SQLite) для их удаления

### Requirement: Производственные гейты

The ADR 0003 machine gates (search p95 latency < 10 ms, recall@10 ≥ 0.95 against exact-L2 brute force, RSS-delta ≤ ~2 GB) SHALL be confirmed by machine on corpora up to the N≈250K × 1024-dim class (s3b spike and CI-scale integration gates). At the extrapolation point N=1M the measured deviation was accepted by the human as a documented worst-case (decision 2026-08-21, option 1): p95 32–55 ms, recall@10 0.911–0.933 at default parameters; recall is limited by efSearch (the HNSW beam), not by partition coverage. efSearch remains a runtime setting for the accuracy/latency balance without index rebuild. The index is quantized and disk-backed (usearch HNSW, scalar quantization default bf16), parameters: M=16, efConstruction=100, metric L2sq.

#### Scenario: Гейт recall на синтетике
- **WHEN** a batch of queries with known brute-force ground truth is run on a seeded CI-scale synthetic corpus
- **THEN** recall@10 ≥ 0.95

#### Scenario: Гейт латентности
- **WHEN** search latency is measured on a warmed index in the release profile
- **THEN** p95 < 10 ms

### Requirement: Формат фикстур SYNX (vectors.bin)

Крейт `vectors` читает и пишет бинарный формат фикстур vectors.bin (контракт fixture-формата): magic "SYNX", version u32 LE = 1, dim u32 LE, count u64 LE, далее count строк `[u32 LE chunk_id][f32 LE × dim]`, отсортированных по возрастанию chunk_id. Чтение поддерживает потоковую обработку без полной загрузки в память; нарушение формата (magic/version/обрыв файла) — явная ошибка.

#### Scenario: Цикл записи и чтения
- **WHEN** набор векторов записан в формат SYNX и прочитан обратно
- **THEN** данные идентичны, строки отсортированы по возрастанию chunk_id

#### Scenario: Повреждённый файл
- **WHEN** файл имеет неверный magic, версию или обрыв посреди строки
- **THEN** возвращается явная ошибка формата с указанием причины

### Requirement: Конфигурация индекса

Index parameters SHALL be configurable: a config struct in the `vectors` crate with defaults (M=16, efConstruction=100, efSearch=200, scalar quantization default bf16, metric L2sq, dimension 1024); the optional `vectors:` section in the config preset (additive config-format extension, decision 2026-08-21) passes overrides; a preset without the section gets the defaults. The IVF-only fields `num_partitions`/`nprobes` were removed with the lance engine (they do not apply to pure HNSW). The `vectors.usearch:` section holds the two-layer persistence parameters of UsearchEngine (ADR 0004): `max_segment_vectors` (default 1000000), `compaction_stale_threshold` (default 30), `search_threads` (default 4).

#### Scenario: Дефолты без секции
- **WHEN** the config preset has no `vectors` section
- **THEN** the defaults above apply

#### Scenario: Переопределение runtime-параметров
- **WHEN** the `vectors` section sets efSearch
- **THEN** search uses the overridden value without rebuilding the index

#### Scenario: Usearch config defaults
- **WHEN** the `vectors.usearch` section is absent from the config
- **THEN** defaults apply: max_segment_vectors=1000000, compaction_stale_threshold=30, search_threads=4

#### Scenario: Usearch config override
- **WHEN** the `vectors.usearch` section sets `max_segment_vectors: 50000`
- **THEN** the RAM flush happens at 50000 vectors

#### Scenario: Invalid usearch config
- **WHEN** `vectors.usearch.max_segment_vectors: 0` or `compaction_stale_threshold: 101` or `search_threads: 0`
- **THEN** a validation error is returned

### Requirement: ANN engine (usearch only)

The `vectors` crate SHALL provide exactly one ANN engine: `UsearchEngine` (usearch 2.x, C++/cxx FFI, disk-backed HNSW, scalar quantization default bf16, L2sq metric, two-layer RAM/DISK persistence with per-segment WAL per ADR 0004). The engine SHALL be compiled unconditionally — there are no Cargo engine features. The optional `vectors.engine` config field selects the engine at runtime: absent or `"usearch"` instantiates `UsearchEngine`; `"lance"` SHALL return an explicit error stating that the lance engine was removed; any other value SHALL return an explicit configuration error. The factory keeps its public signature (`create_vector_engine(engine_name, path, config, wal_db) -> Arc<dyn VectorIndex>`), so `search`/`ingestion`/`mcp` remain engine-agnostic. The index directory stays engine-tagged (`<vectors_path>/usearch`) so existing dataset data is unaffected.

#### Scenario: Default without engine field
- **WHEN** the preset does not set `vectors.engine`
- **THEN** a `UsearchEngine` is instantiated

#### Scenario: Explicit usearch selection
- **WHEN** `vectors.engine = "usearch"`
- **THEN** a `UsearchEngine` is instantiated with bf16 quantization and the L2sq metric

#### Scenario: Removed engine value
- **WHEN** `vectors.engine = "lance"`
- **THEN** the factory (and config validation) returns an explicit error stating that the lance engine was removed and that usearch is the only engine

#### Scenario: Invalid value
- **WHEN** `vectors.engine = "foo"`
- **THEN** the factory (and config validation) returns an explicit configuration error
