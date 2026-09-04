# db-storage Specification

## Purpose

Слой хранения данных Synopsis: соединение с SQLite (WAL, зафиксированный набор PRAGMA), миграции через `PRAGMA user_version` (единственный источник истины), транзакции, DAO-операции над таблицами v5-схемы и FTS5-поиск по чанкам с bm25-ранжированием.

## Requirements

### Requirement: Соединение и миграции

Крейт `db` открывает SQLite-базу с PRAGMA-настройками: WAL, synchronous=NORMAL, cache_size=-64000, mmap_size=268435456, foreign_keys=ON, busy_timeout=5000. Схема создаётся одной squashed init-миграцией (финальное v5-состояние), встроенной в бинарь на compile-time; `PRAGMA user_version` — единственный источник истины о состоянии схемы (=1 после init); таблица `_schema_migrations` НЕ создаётся; legacy knowledge.db НЕ открывается и НЕ мигрируется. Будущие миграции — нумерованные каталоги `<id>-<slug>/up.sql`, forward-only, shipped-миграции не редактируются.

#### Scenario: Инициализация свежей базы
- **WHEN** db открывает несуществующий файл базы
- **THEN** создаётся полная v5-схема (все таблицы, индексы, FTS5-индекс и триггеры), `PRAGMA user_version` = 1, `_schema_migrations` отсутствует

#### Scenario: PRAGMA-parity
- **WHEN** db открывает базу
- **THEN** journal_mode=wal, synchronous=NORMAL, foreign_keys=ON, busy_timeout=5000, cache_size=-64000, mmap_size=268435456 (проверяется тестом)

#### Scenario: Повторное открытие
- **WHEN** db открывает уже инициализированную базу (user_version=1)
- **THEN** миграции не перезапускаются, схема не пересоздаётся, данные сохраняются

### Requirement: Транзакции

Транзакции выполняются через нативный API rusqlite (`Connection::transaction()`), closure-паттерн с автоматическим rollback при ошибке или панике внутри блока; ручные `BEGIN`/`COMMIT` строки не используются. DAO-методы работают единообразно с соединением и транзакцией через общую абстракцию исполнителя.

#### Scenario: Успешная транзакция
- **WHEN** closure-блок транзакции завершается успешно
- **THEN** изменения фиксируются (COMMIT)

#### Scenario: Ошибка в транзакции
- **WHEN** closure-блок возвращает ошибку
- **THEN** все изменения откатываются (ROLLBACK), база остаётся в исходном состоянии

#### Scenario: Паника в транзакции
- **WHEN** closure-блок паникует
- **THEN** транзакция откатывается автоматически (Drop-семантика rusqlite), паника распространяется наружу

### Requirement: DAO-операции над v5-схемой

DAO-слой покрывает таблицы v5-схемы: documents, chunks, entities, facts, связи (chunk_entities, entity_links, entity_sources, fact_sources), app_kv. Поведение операций зафиксировано по семантике: CRUD, пагинация с фильтрами (domain через json_each, source_type, name), batch-операции (IN-списки с плейсхолдерами, батчи ≤ 500 строк), orphan-cleanup (не удаляет EntityType и факт-референсы), GetOrCreate/CreateOrIgnore — атомарные через UNIQUE-констрейнты и `ON CONFLICT` (исправление TOCTOU-гонки). Параметр-лимит SQLite (32766) не нарушается (батчи ≤ 500×2 параметров).

#### Scenario: CRUD документа
- **WHEN** DAO создаёт, читает, обновляет и удаляет документ
- **THEN** все операции возвращают корректные данные; повторное чтение удалённого документа даёт None

#### Scenario: Пагинация с фильтрами
- **WHEN** DAO запрашивает страницу документов/сущностей с фильтрами domain/source_type/name
- **THEN** возвращаются только элементы, удовлетворяющие фильтрам, в зафиксированном порядке, с корректным offset/limit

#### Scenario: Атомарный GetOrCreate
- **WHEN** два вызова GetOrCreate с одинаковыми ключами (type, name, domain) выполняются конкурентно
- **THEN** создаётся ровно одна запись, оба вызова возвращают один и тот же ID (без гонки)

#### Scenario: Batch-операции
- **WHEN** DAO выполняет batch-операцию (GetByIDs, LinkBatch, DeleteByIDs) с большим списком
- **THEN** операция выполняется корректно без превышения параметр-лимита SQLite (батчи ≤ 500 строк)

#### Scenario: Orphan-cleanup
- **WHEN** DAO удаляет осиротевшие сущности/факты
- **THEN** EntityType и сущности/факты, на которые ссылаются другие записи, не удаляются

### Requirement: FTS5-поиск по чанкам

Поиск по чанкам использует FTS5-индекс (встроенный в bundled SQLite, без cgo) с ранжированием bm25 и опциональным domain-фильтром через json_each. Результаты возвращаются с корректными bm25-скорами и chunk_id, отсортированные по релевантности. Поведение зафиксировано (проверяется на фикстуре knowledge.db).

#### Scenario: FTS5-поиск без фильтра
- **WHEN** выполняется поиск 'knowledge' по всем чанкам
- **THEN** возвращается 17 хитов, top-3 chunk_id совпадают с записанной фикстурой (проверка на фикстуре knowledge.db)

#### Scenario: FTS5-поиск с domain-фильтром
- **WHEN** выполняется поиск с ограничением по домену
- **THEN** возвращаются только чанки документов указанного домена, ранжированные по bm25

#### Scenario: Синхронизация индекса
- **WHEN** чанк создаётся, обновляется или удаляется
- **THEN** FTS5-индекс синхронизируется автоматически (триггеры ai/ad/au), поиск отражает актуальное состояние

### Requirement: Конкурентный доступ

Крейт `db` поддерживает конкурентные чтения и не блокирует их write-транзакциями: пул соединений + WAL (несколько соединений разделяют одну БД). Чтения выполняются параллельно; write-транзакция на одном соединении не блокирует чтения на других. Вложенный `exec_tx` (транзакция внутри транзакции на том же потоке) возвращает явную ошибку, а не деадлок и не молчаливую независимую транзакцию.

#### Scenario: Параллельные чтения
- **WHEN** несколько потоков одновременно выполняют read-запросы через `with_conn`
- **THEN** все запросы завершаются корректно, без деадлоков и без взаимной блокировки

#### Scenario: Чтение во время write-транзакции
- **WHEN** один поток выполняет write-транзакцию через `exec_tx`, а другой поток выполняет чтение
- **THEN** чтение не блокируется на время транзакции (WAL + отдельное соединение пула)

#### Scenario: Вложенная транзакция
- **WHEN** `exec_tx` вызывается внутри closure другого `exec_tx` на том же потоке
- **THEN** возвращается `DbError::NestedTransaction`, деадлока нет

### Requirement: vec0 исключён

The `db` crate SHALL contain no vec0-table operations (SearchVector, UpsertVector, FormatVector, DeleteVectorsByChunkIDs, etc.) — vector search lives in the `vectors` crate (ADR 0003/0004, usearch engine); vectors are rebuilt from chunk text, the old vec0 is never read.

#### Scenario: Отсутствие vec0-кода
- **WHEN** the db crate source is checked
- **THEN** it contains no references to vec0 tables or vec0 operations (grep check in CI)

### Requirement: ChunkDao and FTS over search_text
The `Chunk` row type exposes both `chunk_text` and `search_text`. `ChunkDao::create` and `ChunkDao::update` accept a `search_text` value and store it alongside `chunk_text`. The FTS5 index operates on `search_text`, so full-text matches are made against the section-context text, while row reads still return `chunk_text` as the chunk body and the byte offsets are unchanged.

#### Scenario: Create stores both fields
- **WHEN** a chunk is created with a `chunk_text` and a `search_text`
- **THEN** both are stored and a subsequent read returns both values

#### Scenario: FTS matches on search_text
- **WHEN** a full-text query matches a term that appears only in a chunk's `search_text` (e.g. a heading term absent from `chunk_text`)
- **THEN** that chunk is returned by the FTS search

#### Scenario: Row read returns chunk_text
- **WHEN** a chunk row is read
- **THEN** the returned body is `chunk_text` (the pure source slice), with `search_text` available as a separate field