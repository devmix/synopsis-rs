# Design: db-module

## Context

См. proposal.md — Why. Текущее состояние: `config`-модуль завершён (пути db_path/cache_db_path, DatabaseConfig.pragma); squash-миграция v5 существует в `.archive/spikes/migrations/1-init/up.sql` (152 строки, выведена механически из фикстуры, кросс-чек против 001..005 оракула); фикстура `fixtures/knowledge.db` (v5, 270 чанков, 207 сущностей, 179 фактов; sha256 edfc2310...) — ground truth для FTS5-parity; `rusqlite 0.40` (bundled+fts5) уже в палитре; `rusqlite_migration 2.6` + `include_dir 0.7` доказаны в спайке S1 (ADR 0001). Оракул: `../synopsis/internal/database/{database.go, schema.go}` + `dao/` (10 модулей, ~6300 строк).

## Goals / Non-Goals

**Goals:**
- Полноценный DAO-слой над v5-схемой с семантикой оракула (не 1:1-копия)
- FTS5-поиск с bm25-parity на фикстуре
- Атомарные GetOrCreate/CreateOrIgnore (устранение TOCTOU-гонки Go)
- Инфраструктура: соединение, миграции, транзакции, тест-утилиты

**Non-Goals:**
- Миграция данных из Go knowledge.db; чтение старого vec0; пул соединений; shared utils-крейт; FTS5-доменный фильтр в change `search` (здесь — только chunk-DAO); изменение схемы v5.

## Decisions

### D1. Одно соединение `Arc<Mutex<Connection>>` за `spawn_blocking`
Одно общее соединение rusqlite в `Arc<Mutex<>>`; все DAO-вызовы выполняются в `spawn_blocking` (sync-драйвер в async-контексте). WAL допускает конкурентных читателей; писатели сериализуются мутексом — достаточно для ноутбука (один бинарь, ограниченная конкурентность).
*Альтернативы:* r2d2_sqlite/deadpool-sqlite (оверкилл для одного процесса, лишняя зависимость); несколько соединений (не нужно — WAL + мутекс покрывает сценарий). **Решение человека 2026-08-19.**

### D2. Транзакции — нативный API rusqlite + closure-паттерн
`exec_tx(f)` строит транзакцию через `Connection::transaction()` (никаких ручных `BEGIN`/`COMMIT` строк): успех → `tx.commit()`, ошибка → `tx.rollback()`, паника/ранний return → авто-rollback через `Drop` (DropBehavior::Rollback по умолчанию). Вложенность — через `Transaction::savepoint()` при необходимости (в оракуле вложенных транзакций нет). Абстракция `DbExecutor` (sealed trait: execute/query/query_row) + `ConnectionOrTx<'a>` enum — DAO работают единообразно с соединением и транзакцией (аналог Go DBTX interface, удовлетворяемого *sql.DB и *sql.Tx).
*Альтернативы:* ручные BEGIN/COMMIT строки (отклонено — небезопасно, дублирует нативный механизм); trait-объекты `dyn DbExecutor` (отклонено — enum дешевле и типобезопаснее).

### D3. Миграции — rusqlite_migration 2.6 + include_dir 0.7
`migrations/1-init/up.sql` копируется из `.archive/spikes/migrations/` (squashed v5 DDL, уже проверен в спайке S1); встраивается в бинарь через `include_dir!` (compile-time); применяется через `rusqlite_migration::from_directory`; `PRAGMA user_version` — единственный источник истины (=1 после init); `_schema_migrations` НЕ создаётся; legacy knowledge.db НЕ открывается/НЕ мигрируется. Будущие миграции — `<id>-<slug>/up.sql`, forward-only, shipped не редактируются.
*Альтернативы:* копировать 001..005 оракула (реплей мёртвой истории — отклонено, решение 2026-08-18 D6); ручной `user_version`-менеджмент (дублирует rusqlite_migration — отклонено).

### D4. DAO-декомпозиция: 10 Go-модулей → 8 Rust-модулей
Порядок = граф зависимостей: (1) инфраструктура (connection/executor/error/utils/test_util), (2) app_kv, (3) document, (4) chunk+FTS5, (5) entity, (6) fact, (7) chunk_entity+entity_link+entity_source, (8) fact_source+сборка lib.rs. Каждая задача ≤ ~500 строк диффа, самодостаточна для свежего агента.
*Альтернативы:* 10 задач по одному Go-модулю (избыточно — мелкие модули группируются); 4 крупные задачи (превышают лимит контекста/диффа).

### D5. Атомарные GetOrCreate/CreateOrIgnore через ON CONFLICT
Вместо Go-паттерна select-then-insert (TOCTOU-гонка при конкурентном доступе) — `INSERT ... ON CONFLICT (unique-колонки) DO NOTHING` + возврат ID (RETURNING или повторный SELECT). UNIQUE-констрейнты уже есть в v5-схеме: entities(type,name,domain), facts(subject_entity_id,object_entity_id,predicate). **Исправляет баг Go.**
*Альтернативы:* сохранить select-then-insert (отклонено — гонка); транзакция вокруг select+insert (отклонено — избыточно, ON CONFLICT атомарен).

### D6. utils — локальный модуль db::utils
`Normalize` (trim + lowercase + collapse whitespace) и `EscapeLike` (escape `\`, `%`, `_` для LIKE) — приватный/публичный модуль внутри db-крейта. Отдельный shared-крейт не нужен (KISS/YAGNI) — только db использует эти функции; при появлении второго потребителя — вынести.
*Альтернативы:* shared crate `utils` (отклонено — преждевременная абстракция).

### D7. vec0 полностью исключён
Все vec0-операции оракула (SearchVector, UpsertVector, FormatVector, DeleteVectorsByChunkIDs, DeleteOrphanedVectors) НЕ переносятся в db-крейт — векторный поиск переезжает в change `vectors` (ADR 0003, lance); векторы пересобираются из текста чанков. В squash-миграции vec0-таблиц нет (исключены решением 2026-08-18).
*Альтернативы:* thin-обёртки-заглушки (отклонено — мёртвый код, YAGNI).

### D8. PRAGMA-parity
При открытии применяются: journal_mode=WAL, synchronous=NORMAL, cache_size=-64000, mmap_size=268435456, foreign_keys=ON, busy_timeout=5000 — как в Go database.go/schema.go. Проверяется тестом (PRAGMA-запросы после open).

### D9. Параметр-лимит SQLite (32766)
Batch-операции (GetByIDs, LinkBatch, DeleteByIDs) используют плейсхолдеры с батчами ≤ 500 строк (LinkBatch — 500×2=1000 параметров; GetByIDs — разбиение на чанки по 500). Лимит задокументирован в коде; тест на границе батча.

## Risks / Trade-offs

- **FTS5 bm25-скоринги: bundled SQLite vs Go CGO** → bm25() — часть FTS5-спеки, ожидается идентично; parity-тест на фикстуре (17 хитов, top-3 chunk_ids) подтверждает.
- **RETURNING-клауза** (нужна для атомарного GetOrCreate) → зависит от bundled SQLite ≥ 3.35; проверить в задаче 1.1; fallback — повторный SELECT после INSERT.
- **include_dir API** (embed_dir! возвращает &'static str или Vec<u8>) → проверить в задаче 1.1; rusqlite_migration принимает оба.
- **json_each domain-фильтр** дорог на больших данных → приемлемо для ноутбука (личное использование); оптимизация — в change `search`.
- **Мёртвый код в тестах** (fixture read-only) → фикстура открывается в mode=ro/immutable, тесты не пишут в неё.

## Migration Plan

Каждая задача самодостаточна и коммитится отдельно (инфраструктура → DAO по одному). Откат — revert файлов задачи. Крейт db не влияет на другие крейты (только зависит от config). После всех задач — архив change с parity-результатами.

## Open Questions

- (нет — все решения приняты; детали API rusqlite_migration/include_dir проверяются реализатором в задаче 1.1 без изменения плана)