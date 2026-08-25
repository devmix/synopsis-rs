# Tasks: ingestion-pipeline

Зависимости change'а: ingestion-sources, ingestion-ner (архивы), crates/db DAOs,
crates/vectors engine, crates/graph build_entity_links, crates/embedding trait — все готовы.
Оракул (read-only): `../synopsis/internal/ingestion/{ingester,types,progress}.go`,
`../synopsis/internal/ingestion/runner/runner.go`, `../synopsis/internal/gc/`,
`../synopsis/internal/ingestion/e2e_test.go`. Дизайн: design.md D1–D8.
Гейты каждой задачи: `cargo fmt --check`, `cargo clippy -p <crate> --all-targets -- -D warnings`,
`cargo test -p <crate>`; `cargo test --workspace` и cargo doc гоняет ТОЛЬКО оркестратор
(диск: линковка гигабайтных debug-бинарников вешала агентов).
Директива (binding): НЕ копировать Go 1:1 — функциональная копия, архитектура для Rust
(DRY/KISS/SOLID/YAGNI); баги оракула исправлять или фиксировать отклонения.

- [x] 3.1 Прогресс-трекинг
  - Цель: статистика и индикация прогона.
  - Scope файлов: `crates/ingestion/src/progress.rs` (новый), `crates/ingestion/src/lib.rs` (+mod),
    `crates/ingestion/Cargo.toml` (+indicatif workspace).
  - Содержание: порт `../synopsis/internal/ingestion/progress.go`: ProgressStats
    (files_processed, chunks_created, embeddings_created, entities_extracted,
    facts_created, fact_sources_created, documents_created/updated/skipped, errors)
    + ProgressTracker над indicatif ProgressBar (total files, инкременты, elapsed).
    Tracker — тонкая обёртка; Stats() снапшот.
  - Тесты: счётчики, снапшот, elapsed > 0.
  - Критерии приёмки: гейты зелёные.

- [x] 3.2 GC-модуль в db-крейте
  - Цель: каскадное удаление документа + сироки-документы.
  - Scope файлов: `crates/db/src/gc.rs` (новый), `crates/db/src/lib.rs` (+mod).
  - Содержание: GcDao над ConnectionOrTx (форма существующих DAO):
    `full_clear_doc_by_id(doc_id)` — удаление чанков документа + chunk_entities +
    entity_sources + fact_sources + facts документа (порядок/SQL сверить с оракулом
    `../synopsis/internal/gc/`; FK-ограничения нашей схемы учесть);
    `delete_orphaned_documents()` — документы без чанков, entity_sources и fact_sources.
    Дополнить существующие delete_orphaned_entity_ids/delete_orphaned_facts при
    необходимости (не дублировать).
  - Тесты: in-memory SQLite паттерн crates/db: каскад удаляет всё и только своё;
    сироки-документы удаляются, связанные живут; пустой документ.
  - Критерии приёмки: гейты db зелёные.

- [x] 3.3 Хелперы Ingester: hash, quotes, source_type
  - Цель: чистые функции документного конвейера.
  - Scope файлов: `crates/ingestion/src/ingester/helpers.rs` (новый; модуль ingester/).
  - Содержание: compute_content_hash (sha256 hex); extract_quote_from_chunk по design D7
    (case-insensitive первое вхождение субъекта ИЛИ объекта — ранний индекс побеждает,
    окно ±60 rune, fallback 120 rune, trim_to_line_boundary, суффикс «...», rune-aware);
    source_type_from_metadata («unknown» для отсутствующего/пустого).
  - Тесты: ПОЛНЫЙ паритет кейсов extractQuoteFromChunk из
    `../synopsis/internal/ingestion/ingester_test.go` + hash/source_type кейсы.
  - Критерии приёмки: гейты ingestion зелёные.

- [x] 3.4 Ingester: каркас + документный цикл
  - Цель: полный пер-документный конвейер без фактов.
  - Scope файлов: `crates/ingestion/src/ingester/mod.rs` (новый),
    `crates/ingestion/Cargo.toml` (+vectors, embedding workspace).
  - Содержание: Ingester по design D2/D3 (инъекция коллабораторов: &dyn Source,
    &dyn EmbeddingProvider, Option<&dyn NerProvider>, &Resolver, vectors-хэндл);
    ingest(): stat path, count files → tracker → backup (задача 3.6, хук) → rebuild-clear
    (хук) → parse через domain-enrichment (metadata["domain"] от вызывающего? нет —
    enrichment в Runner; здесь просто Source) → на документ: hash-dedup (skip) → chunk
    (empty skip) → эмбеддинги батчами (default 100, mismatch = err) → NER per chunk
    (None/disabled → пропуск) → ОДНА транзакция exec_tx: document create/update
    (+GcDao::full_clear_doc_by_id при update), вставки чанков, resolver.add_entities +
    ChunkEntityDao::link. Векторы — insert_batch ПОСЛЕ коммита (design D5).
    Ошибки документа → stats.errors, продолжение.
  - Тесты: mock EmbeddingProvider/NerProvider (стабы): dedup skip, update path
    (full_clear), empty doc skip, batch split, mismatch error, per-doc error isolation,
    vectors после коммита.
  - Критерии приёмки: гейты ingestion зелёные; сценарии покрыты.

- [x] 3.5 Ingester: факты
  - Цель: запись фактов с синтетическими сущностями и цитатами.
  - Scope файлов: `crates/ingestion/src/ingester/facts.rs` (новый).
  - Содержание: store_entities-фактовая часть по оракулу: уникальные (name,type,domain)
    концовок фактов → synthetic NerEntity → resolver.lookup_or_create_with_stats →
    ChunkEntityDao::link; entity_map (кортежный ключ, НЕ '\0'-склейка оракула);
    FactDao::validate_fact_domain (кросс-домен → warn+skip); create_or_ignore с
    metadata JSON; FactSourceDao::create с quote (3.3) + extracted_at RFC3339;
    recompute_weights затронутых фактов; счётчики tracker.
  - Тесты: факт happy-path; кросс-домен skip; неразрешённая концовка skip;
    дедуп синтетических сущностей; веса пересчитаны; цитаты записаны.
  - Критерии приёмки: гейты ingestion зелёные.

- [x] 3.6 Backup + rebuild
  - Цель: снимок БД и очистка источника.
  - Scope файлов: `crates/ingestion/src/ingester/backup.rs` (новый).
  - Содержание: create_backup по design D6: PRAGMA database_list → путь файла;
    in-memory/unnamed → Ok(false) (пропуск); VACUUM INTO '<dir>/backups/<base>_backup_<ts>.db'
    (ts %Y-%m-%dT%H-%M-%S-<ms>); ошибка → warn-семантика (Ok(false)/лог на вызывающем).
    clear_source_data(source_root): список документов, prefix-match очищенных путей,
    удаление DocumentDao::delete в ОДНОЙ транзакции.
  - Тесты: файловая БД во temp-dir → файл снимка создан; in-memory → пропуск;
    rebuild-prefix удаляет только свои документы.
  - Критерии приёмки: гейты ingestion зелёные.

- [x] 3.7 Runner: сборка и мультиисточниковый прогон
  - Цель: оркестрация источников.
  - Scope файлов: `crates/ingestion/src/runner/mod.rs` (новый).
  - Содержание: Runner по design D4: Mutex; конструктор принимает Db, Config,
    Registry (sources change), доменные конфиги, global config, embed/vectors/linker-
    коллабораторы (D3), опциональный LlmNerCache; detect_source_type (wiki|mediawiki →
    mediawiki, webpage → webpages, иначе unstructured); domain-enriched parser wrapper
    (metadata["domain"] = src.domain); NER provider assembly per source (стадии из
    GlobalNerConfig.methods, домены по имени, warn+skip на недоступный домен, деградация
    в no-NER при ошибке построения); ingest_all (enabled sources последовательно,
    ошибки собираются) → cleanup → links; ingest_source / sync_source /
    ingest_source_by_path (exact abs match, иначе longest prefix); SummaryStats.
  - Тесты: стабы вместо реальных провайдеров: порядок источников, сбор ошибок,
    detect_source_type кейсы, find_source_for_path (exact/prefix/deleted-file),
    domain enrichment, сериализация (Mutex) smoke.
  - Критерии приёмки: гейты ingestion зелёные.

- [x] 3.8 Runner: очистка, prune, линковка
  - Цель: пост-конвейерные операции.
  - Scope файлов: `crates/ingestion/src/runner/cleanup.rs` (новый).
  - Содержание: cleanup_orphaned_data: одна транзакция — GcDao::delete_orphaned_entity_ids,
    delete_orphaned_facts, delete_orphaned_documents + векторная реконсиляция (design D5:
    engine.chunk_ids vs живые chunk id → delete_by_chunk_ids); OrphanCleanupStats.
    prune_deleted: документы под enabled-корнями с исчезнувшими файлами → full_clear +
    delete в транзакции на документ; возвращает число. build_entity_links: без конфига —
    skip; инкрементальное окно из AppKv (ключ как в оракуле relations.KVKeyLastLinkingRun),
    вызов graph::build_entity_links, запись timestamp; ошибки → в stats.
  - Тесты: сироки всех видов удаляются; prune удаляет исчезнувшие и оставляет живые;
    links skip без конфига; app_kv roundtrip.
  - Критерии приёмки: гейты ingestion зелёные.

- [x] 3.9 E2E + финальная сборка
  - Цель: сквозной тест и публичный API.
  - Scope файлов: `crates/ingestion/tests/pipeline_e2e.rs` (новый),
    `crates/ingestion/src/lib.rs`, `crates/ingestion/src/runner/mod.rs`.
  - Содержание: e2e против сценариев оракула e2e_test.go (адаптированно): temp-source
    с markdown/json файлами → ingest_all с mock-эмбеддингами + regex-NER → проверки БД
    (документы/чанки/сущности/факты/цитаты) → повторный прогон (все skipped) → изменение
    файла (updated + старое удалено) → удаление файла → prune_deleted → cleanup_orphaned.
    Ре-экспорт Pipeline API из корня (Ingester, Runner, ProgressStats, SummaryStats,
    OrphanCleanupStats), crate docs, compile-time root-API тест.
  - Критерии приёмки: `RUSTDOCFLAGS="-D warnings" cargo doc -p ingestion --no-deps` чисто;
    гейты зелёные; workspace-тест прогоняет оркестратор.
