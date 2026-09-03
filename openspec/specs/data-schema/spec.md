# data-schema Specification

## Purpose

Схема данных SQLite и правила миграций. Фиксирует, что Rust-бинарь продолжает работать с тем же knowledge.db, который создаёт Go оригинал (5 shipped-миграций: `../synopsis/migrations/*.sql`). Это контракт непрерывности данных при переходе между реализациями.
## Requirements
### Requirement: Совместимость со схемой v5

Rust-бинарь открывает существующий knowledge.db, созданный Go оригиналом (схема после миграций 001–005), без повторного применения миграций и без потери данных. Таблицы `documents`, `chunks`, `entities`, `chunk_entities`, `facts`, `fact_sources`, `entity_sources`, `entity_links`, `app_kv`, FTS5-таблица `chunks_fts` и индексы оракула присутствуют и используются идентично: те же запросы дают те же результаты.

#### Scenario: Открытие старой БД
- **WHEN** Rust-сервер стартует с knowledge.db, созданным Go (sync завершён)
- **THEN** миграции не переписывают данные; catalog_overview возвращает счётчики, равные ответам Go бинаря на той же БД

#### Scenario: Differential-запросы
- **WHEN** к обоим бинарям (одна и та же БД-копия) послать одинаковые read-запросы через MCP tools
- **THEN** результаты идентичны (кроме ANN-полей, см. recall-гейты)

### Requirement: Дисциплина миграций

Миграции применяются при старте в нумерованном порядке и идемпотентны (IF NOT EXISTS). Shipped-файлы 001–005 никогда не редактируются; изменения схемы в Rust-версии добавляются только новыми файлами с продолжением нумерации (006+), которые корректно применяются к БД версии v5.

#### Scenario: Новая миграция
- **WHEN** в Rust-репо добавлена migration 006 и бинарь стартует на БД v5
- **THEN** 006 применяется один раз, данные не повреждены; повторный старт — безоперационен

### Requirement: Цикл статусов facts

Таблица `facts` несёт колонку `status` со значениями `draft`, `pending`, `approved`, `rejected` (CHECK-констрейнт оракула сохранён). Только `approved` факты участвуют в read-расширениях (search expansion, dossiers) — поведение совпадает с оракулом.

#### Scenario: CHECK-констрейнт
- **WHEN** попытаться записать fact со status 'weird'
- **THEN** запись отклоняется констрейнтом

### Requirement: Хранение векторов и пересборка

Векторы эмбеддингов НЕ переносятся из старой vec0-таблицы `chunks_vec` в новое хранилище. Старая таблица в legacy-файле игнорируется (или отключается) без ошибок. Векторный индекс Rust-версии — внешнее HNSW-хранилище, пересобираемое по тексту чанков из `chunks` при rebuild; размерность индекса соответствует модели эмбеддингов из конфигурации.

#### Scenario: Пересборка векторов
- **WHEN** Rust-бинарь запускает rebuild векторов на БД v5 (без vec0-данных)
- **THEN** индекс построен по тексту чанков; семантический поиск возвращает recall@10 ≥ 0.95 относительно brute-force ground truth на той же модели эмбеддингов

### Requirement: Таблица document_jobs (очередь индексации)

Rust-бинарь ведёт очередь операций над документами в таблице `document_jobs` (knowledge DB), создаваемой consolidated init-миграцией `migrations/knowledge/1-init/up.sql` (таблица свёрнута в init-миграцию при squash, design D6; отдельной forward-only миграции `2-document-jobs` не существует). Таблица — единая state-machine для вотчера, стартового сканирования и фонового worker'а; она не влияет на совместимость с Go-оракулом (оракул эту таблицу не использует). Колонки: `path TEXT PRIMARY KEY`, `source_path TEXT NOT NULL`, `op TEXT NOT NULL DEFAULT 'index'` (`index` | `delete`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `content_hash TEXT`, `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER`, `updated_at INTEGER`. Индекс `idx_document_jobs_due (status, next_attempt_at)` для due-запросов. Миграция идемпотентна (`IF NOT EXISTS`); повторный старт — безоперационен.

#### Scenario: Применение миграции
- **WHEN** бинарь стартует на knowledge.db без таблицы `document_jobs`
- **THEN** init-миграция `1-init` создаёт таблицу и индекс один раз; повторный старт не меняет схему

#### Scenario: Состояние задания
- **WHEN** документ не проиндексировался после `max_attempts` повторов
- **THEN** строка имеет `status='error'`, `attempts=max_attempts`, `last_error` заполнен; `index reset-retries` переводит её в `pending` с `attempts=0`

### Requirement: search_text column (explicit v5 deviation)
The `chunks` table carries a `search_text TEXT NOT NULL` column (default = `chunk_text`) holding the search-oriented text: for Markdown chunks the heading breadcrumb (multi-line heading path) followed by the chunk body, or the body alone when the chunk has no breadcrumb. The FTS5 `chunks_fts` index is built over `search_text` (not `chunk_text`), and its `ai/ad/au` triggers reference `search_text`. This is an explicit, justified deviation from the oracle v5 shape: the Rust database is always built from scratch (no legacy `knowledge.db` is opened or migrated), and the deviation improves RAG retrieval quality by giving both search legs the section context. The invariant-preserving `chunk_text` column and the byte offsets are unchanged.

#### Scenario: Fresh build includes search_text
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a `search_text` column, the `chunks_fts` index indexes `search_text`, and `PRAGMA user_version` = 1

#### Scenario: FTS index tracks search_text
- **WHEN** a chunk is inserted, updated, or deleted
- **THEN** the `chunks_fts` index is kept in sync via the `ai`/`au`/`ad` triggers over `search_text`

#### Scenario: chunk_text invariant preserved
- **WHEN** a chunk is stored
- **THEN** `chunk_text` remains the pure source slice and `start_offset`/`end_offset` still satisfy `content[start_offset..end_offset] == chunk_text`

### Requirement: chunk metadata_json column (explicit v5 deviation)
The `chunks` table SHALL carry a `metadata_json TEXT` (nullable) column holding the per-chunk metadata bag as raw JSON: the chunk-specific keys the chunker computed (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …). It SHALL be stored as raw text (the `documents.metadata_json` pattern), parsed on demand, and `NULL` SHALL mean "no chunk metadata". This is an explicit, justified deviation from the oracle v5 shape (the Go `chunks` table has no metadata column): the Rust database is always built from scratch (no legacy `knowledge.db` is opened or migrated), and the column restores a field that was in the original Rust design and surfaces the section context in search. The invariant-preserving `chunk_text`, the byte offsets, and the `search_text` re-point are unchanged; `PRAGMA user_version` stays 1.

#### Scenario: Fresh build includes metadata_json
- **WHEN** a fresh knowledge database is built from the consolidated init migration
- **THEN** the `chunks` table has a nullable `metadata_json` column and `PRAGMA user_version` = 1

#### Scenario: Round-trip
- **WHEN** a chunk is created with a `metadata_json` value
- **THEN** a row read returns the same value, and a chunk created without one returns `NULL`

