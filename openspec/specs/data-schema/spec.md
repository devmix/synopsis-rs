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

Rust-бинарь ведёт очередь операций над документами в таблице `document_jobs` (knowledge DB), создаваемой forward-only миграцией `migrations/knowledge/2-document-jobs/up.sql`. Таблица — единая state-machine для вотчера, стартового сканирования и фонового worker'а; она не влияет на совместимость с Go-оракулом (оракул эту таблицу не использует). Колонки: `path TEXT PRIMARY KEY`, `source_path TEXT NOT NULL`, `op TEXT NOT NULL DEFAULT 'index'` (`index` | `delete`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `content_hash TEXT`, `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER`, `updated_at INTEGER`. Индекс `idx_document_jobs_due (status, next_attempt_at)` для due-запросов. Миграция идемпотентна (`IF NOT EXISTS`); повторный старт — безоперационен.

#### Scenario: Применение миграции
- **WHEN** бинарь стартует на knowledge.db без таблицы `document_jobs`
- **THEN** миграция `2-document-jobs` создаёт таблицу и индекс один раз; повторный старт не меняет схему

#### Scenario: Состояние задания
- **WHEN** документ не проиндексировался после `max_attempts` повторов
- **THEN** строка имеет `status='error'`, `attempts=max_attempts`, `last_error` заполнен; `index reset-retries` переводит её в `pending` с `attempts=0`

