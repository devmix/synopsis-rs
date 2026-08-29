# data-schema Specification

## ADDED Requirements

### Requirement: Таблица document_jobs (очередь индексации)

Rust-бинарь ведёт очередь операций над документами в таблице `document_jobs` (knowledge DB), создаваемой forward-only миграцией `migrations/knowledge/2-document-jobs/up.sql`. Таблица — единая state-machine для вотчера, стартового сканирования и фонового worker'а; она не влияет на совместимость с Go-оракулом (оракул эту таблицу не использует). Колонки: `path TEXT PRIMARY KEY`, `source_path TEXT NOT NULL`, `op TEXT NOT NULL DEFAULT 'index'` (`index` | `delete`), `status TEXT NOT NULL DEFAULT 'pending'` (`pending` | `processing` | `done` | `error`), `content_hash TEXT`, `attempts INTEGER NOT NULL DEFAULT 0`, `max_attempts INTEGER NOT NULL DEFAULT 3`, `last_error TEXT`, `next_attempt_at INTEGER NOT NULL DEFAULT 0`, `created_at INTEGER`, `updated_at INTEGER`. Индекс `idx_document_jobs_due (status, next_attempt_at)` для due-запросов. Миграция идемпотентна (`IF NOT EXISTS`); повторный старт — безоперационен.

#### Scenario: Применение миграции
- **WHEN** бинарь стартует на knowledge.db без таблицы `document_jobs`
- **THEN** миграция `2-document-jobs` создаёт таблицу и индекс один раз; повторный старт не меняет схему

#### Scenario: Состояние задания
- **WHEN** документ не проиндексировался после `max_attempts` повторов
- **THEN** строка имеет `status='error'`, `attempts=max_attempts`, `last_error` заполнен; `index reset-retries` переводит её в `pending` с `attempts=0`
