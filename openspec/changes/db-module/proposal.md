# Proposal: db-module

## Why

Следующий модуль миграции после `config-module` (завершён и заархивирован): слой хранения данных — `crates/db` (rusqlite-соединения, миграции, DAO, FTS5-запросы). Без него не могут начаться модули `ingestion`, `graph`, `search` — это фундамент всего приложения. Схема v5 (структурный контракт) уже зафиксирована squash-миграцией в спайке S1 (`.archive/spikes/migrations/1-init/up.sql`); теперь нужен полноценный DAO-слой с поведением, совпадающим с оракулом по семантике.

## What Changes

- **Новый крейт `crates/db`** (tier 0, зависит только от `config`): соединение, миграции, транзакции, DAO, FTS5-поиск.
- **Соединение:** одно общее `Arc<Mutex<Connection>>` за `spawn_blocking` (ноутбук, один бинарь; пул — оверкилл). PRAGMA-parity с оракулом: WAL, synchronous=NORMAL, cache_size=-64000, mmap_size=268435456, foreign_keys=ON, busy_timeout=5000.
- **Миграции:** `rusqlite_migration 2.6` (from-directory) + `include_dir 0.7` (compile-time embedding); один squashed init `migrations/1-init/up.sql` (переносится из `.archive/spikes/migrations/`); `PRAGMA user_version` — единственный источник истины; `_schema_migrations` НЕ создаётся; legacy Go knowledge.db НЕ открывается/НЕ мигрируется (решения 2026-08-18, D6).
- **Транзакции:** нативный API rusqlite (`Connection::transaction()` / `Transaction::commit()` / `rollback()`, авто-rollback через `Drop`), closure-паттерн `exec_tx` (аналог Go TxManager); абстракция `DbExecutor` (trait + `ConnectionOrTx` enum) — DAO работают единообразно с соединением и транзакцией. Никаких ручных `BEGIN`/`COMMIT` строк.
- **DAO (10 Go-модулей → 8 Rust-модулей):** app_kv, document, chunk (+FTS5 SearchFTS с bm25 + json_each domain-фильтр), entity (атомарный GetOrCreate через `ON CONFLICT` — **исправляет TOCTOU-гонку Go**), fact (CreateOrIgnore атомарный), chunk_entity, entity_link, entity_source, fact_source, utils (Normalize/EscapeLike — локальный модуль).
- **vec0 полностью исключён** из db-крейта (SearchVector/UpsertVector/FormatVector/DeleteVectorsByChunkIDs и пр.) — векторы пересобираются в change `vectors` (ADR 0003, lance).
- **Палитра:** `rusqlite_migration 2.6` + `include_dir 0.7` (версии доказаны в спайке S1); `rusqlite 0.40` уже в палитре.
- **Parity:** FTS5 bm25-тест на фикстуре `fixtures/knowledge.db` (запрос 'knowledge' → 17 хитов, top-3 chunk_ids как в спайке S1); PRAGMA-проверки; поведение DAO — по семантике оракула (не 1:1-копия).

## Capabilities

### New Capabilities
- `db-storage`: слой хранения данных — соединение и миграции (user_version), транзакции, DAO-операции над таблицами v5-схемы (documents, chunks, entities, facts, связи, app_kv), FTS5-поиск по чанкам с bm25-ранжированием и domain-фильтром.

### Modified Capabilities
- (нет — data-schema остаётся эталоном; db-модуль реализует её, не меняя)

## Impact

- **Код:** новый `crates/db` (src/connection.rs, src/executor.rs, src/error.rs, src/utils.rs, src/test_util.rs, src/app_kv.rs, src/document.rs, src/chunk.rs, src/entity.rs, src/fact.rs, src/chunk_entity.rs, src/entity_link.rs, src/entity_source.rs, src/fact_source.rs, src/lib.rs), `migrations/1-init/up.sql` (копия из `.archive/spikes/migrations/`), тесты.
- **Зависимости:** `config` (db_path/cache_db_path, DatabaseConfig.pragma); новые в палитре: rusqlite_migration, include_dir.
- **Контракты:** data-schema (v5) — структурный контракт, не меняется; vec0-таблицы исключены из squash (решение 2026-08-18) — не контракт. BREAKING vs оракул: атомарный GetOrCreate (устранение гонки), один Connection вместо пула, vec0-операции вне db.
- **Non-goals:** миграция данных из Go knowledge.db; чтение старого vec0; пул соединений; shared utils-крейт; FTS5-доменный фильтр в change `search` (здесь — только chunk-DAO); изменение схемы v5.