# Tasks — native-seam-spikes

Порядок = граф зависимостей. Задачи 1.x–3.x независимы после 0.x → оркестратор может выделить их РАЗНЫМ свежим агентам параллельно (каждая самодостаточна). Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс).

**Ревизия change (2026-08-18, одобрено человеком):** (а) задача 4.1 (S4 «шов MCP-транспорта») **удалена**: транспорт закрыт решением scaffold D8 — rmcp over Streamable HTTP, legacy SSE оракула намеренно не поддерживается; работоспособность транспорта уже доказана round-trip тестами parity-harness (scaffold task 3.1). ADR по MCP-транспорту не требуется, нумерация ADR: 0001–0003. (б) S2 спайкается на **`ort`** (pykeio) вместо `onnxruntime-rs` — детали в истории ревизий задачи 2.1. (в) **Порядок исполнения 0.x: сначала 0.2, затем 0.1** — rusqlite-проба из критериев приёмки 0.1 живёт в `crates/spikes`, который создаётся в 0.2 (фактическая зависимость обратная нумерации).

## 0. Подготовка

- [x] 0.1 Fixture knowledge.db из оракула + фиксация provenance
  - **Цель:** реальный v5-файл БД с данными для S1 и будущих parity-проверок.
  - **Scope файлов:** `fixtures/knowledge.db` (gitignored — добавить `fixtures/` в .gitignore) + `fixtures/README.md` (команда генерации, дата, масштаб, счётчики строк по таблицам).
  - **Зависимости:** нет (первая задача). Только СУЩЕСТВУЮЩИЕ инструменты оракула — ноль изменений кода в ../synopsis.
  - **Критерии приёмки:** файл существует и открывается; README фиксирует provenance; rusqlite-проба (одноразовый bin внутри спайк-crate'а) подтверждает таблицы + FTS5-содержимое.
  - **Референс:** ../synopsis/site/docs/guides/load-testing.mdx (load-test --scale medium или sync на выборке — выбрать по докам), ../synopsis/AGENTS.md (make build/sync).
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, план одобрён человеком): источник фикстуры = **копия существующего `../synopsis/data/knowledge.db`** (2.2 MB; 270 chunks / 25 documents / 179 facts; v5-схема, миграции 001–005 применены, FTS5 работает). Генерацию через `load-test --scale medium` намеренно НЕ делаем: сетевые/CPU-затраты (~2.3 GB модель + 1–3 ч векторизации), а масштабные фикстуры change откладывает к первому модульному change (non-goals). В БД есть legacy `chunks_vec ... vector FLOAT[384]` (vec0) — это реальный кейс для контракта data-schema «старые vec0-таблицы игнорируются без ошибки»; rusqlite-проба должна подтвердить, что open + FTS5-запросы работают в её присутствии. Provenance в README: команда копирования, путь источника, дата генерации и версия/дата бинаря оракула, счётчики строк по таблицам, sha256 файла, примечание про legacy 384-dim и команду регенерации в масштабе (`load-test --scale medium`).
    - Ревизия 2 (2026-08-18, deviation при имплементации, подтверждено человеком на коммите задачи): workspace-пин `rusqlite = { version = "0.40", features = ["bundled","fts5"] }` в корневом Cargo.toml **нерезолвябель** — таких фичей в rusqlite 0.40.x не существует (проверено по crates.io sparse index: default теперь линкует системный SQLite через pkg-config, бандлинг переехал во фичу `bundled` самого libsqlite3-sys; FTS5 входит в каждую bundled-сборку, отдельной фичи fts5 больше нет). Исправлено на `rusqlite = "0.40"` + прямая зависимость `libsqlite3-sys = { version = "0.38", features = ["bundled"] }` (cargo запрещает транзитные feature-спеки без re-export). Намерение design D3 сохранено и эмпирически подтверждено: единственная инстанция libsqlite3-sys 0.38.2 в Cargo.lock, source-compiled SQLite, `bm25()` работает, CGO-флагов нигде нет. Замороженный entry `rusqlite (bundled + fts5)` в openspec/config.yaml НЕ редактируется — отклонение фиксируется здесь и объясняется в ADR 0001 (задача 1.1).

- [x] 0.2 Crate `crates/spikes` — скелет
  - **Цель:** единая точка сборки прототипов S1–S3 (design D1).
  - **Scope файлов:** `crates/spikes/Cargo.toml` + `src/bin/s1_sqlite.rs`, `s2_onnx.rs`, `s3a_usearch.rs`, `s3b_lance.rs` — заглушки (println "not implemented"); регистрация в workspace-членах корня.
  - **Зависимости:** scaffold-rust-project заведён (workspace существует).
  - **Критерии приёмки:** `cargo build --workspace`, clippy, fmt, test — зелёные; бинари запечатаны заглушками.
  - **Референс:** layout — design.md D1.

## 1. S1 — шов SQLite/FTS5 (rusqlite bundled+fts5)

- [x] 1.1 Spike: fresh-БД через rusqlite_migration, init v5-схемы, bm25-паритет
  - **Цель:** доказать шов SQLite/FTS5 в Rust: source-compiled bundled SQLite (rusqlite 0.40 + libsqlite3-sys `bundled` — см. ревизию 2 задачи 0.1), инициализация СВЕЖЕЙ базы через rusqlite_migration одним init-миграцией, и что FTS5 bm25-запросы на тех же данных дают результаты, идентичные записанным ожиданиям оракула. Открытие legacy Go-БД НЕ является целью (см. историю ревизий).
  - **Scope файлов:** корневой `Cargo.toml` (+workspace-зависимость), `crates/spikes/Cargo.toml`, `crates/spikes/migrations/1-init/up.sql`, `crates/spikes/src/bin/s1_sqlite.rs` (замена заглушки), `docs/adr/0001-sqlite-fts5.md`. Шаги внутри спайка:
    1. Вывести init-DDL: дамп полной схемы `fixtures/knowledge.db` (sqlite_master + PRAGMA table_info) с исключением Go-artifacts явным списком с комментарием (`_schema_migrations`, vec0-virtual-таблица и её shadow-таблицы); кросс-чек семантики против ../synopsis/migrations/001..005.sql (read-only). Результат → `crates/spikes/migrations/1-init/up.sql` — итоговое v5-состояние: product-таблицы + индексы + FTS5, без колонки domain, с поздними колонками из ALTER'ов.
    2. Свежая временная БД: rusqlite_migration (`from-directory`, include_dir) применяет init-миграцию; assert `PRAGMA user_version` == 1.
    3. Схемный дифф fresh vs fixture по всем product-таблицам/колонкам/индексам (Go-artifacts исключены тем же явным списком).
    4. Применить PRAGMA из ../synopsis/configs/config.default.yaml (journal_mode=WAL, synchronous=NORMAL, cache_size=-64000, mmap_size=268435456); проверить отсутствие ошибок + `PRAGMA journal_mode` == wal на bundled SQLite.
    5. Прочитать тексты чанков из fixture read-only и вписать их в fresh-БД (включая FTS-контент по механизму, зафиксированному в init-DDL); выполнить фиксированные FTS5 bm25 MATCH-запросы и сравнить с записанными ожиданиями оракула: `knowledge` → 17 хитов, top-3 = (chunk_id 247, score ≈ -4.4453), (30, ≈ -3.573), (106, ≈ -3.4522); `RAG` → 1 хит; `"knowledge graph"` (фраза) → 1 хит; `knowledge OR RAG` (операторы) → 18 хитов (объединение 17+1, пересечения нет).
  - **Зависимости:** 0.1, 0.2; новая workspace-зависимость `rusqlite_migration = { version = "2.6", features = ["from-directory"] }` (MSRV 1.95 ≤ пин 1.96 ✓; требует rusqlite ^0.40 → унифицируется с установленным 0.40.2).
  - **Критерии приёмки:** `cargo run -p spikes --bin s1_sqlite` завершается 0 с PASS-строкой на каждый чек (схема, user_version, PRAGMA, bm25); top-k chunk_id и порядок ТОЧНО совпадают с ожиданиями; отклонение score < 1e-3 относит. (больше — зафиксировать в ADR как NO-GO-риск); ADR `docs/adr/0001-sqlite-fts5.md` с GO/NO-GO + заметки по PRAGMA/WAL + объяснение deviation pина rusqlite (ревизия 2 задачи 0.1) + решение о механизме миграций и отклонённые альтернативы; clippy/fmt чистые в spikes (гейт -D warnings).
  - **Референс:** ../synopsis/migrations/*.sql (только кросс-чек семантики — файлы НЕ копируются), ../synopsis/configs/config.default.yaml, fixtures/knowledge.db (+ README).
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, решение человека): механизм миграций = `rusqlite_migration` 2.6 (`from-directory`, include_dir встраивает SQL на compile-time); `PRAGMA user_version` — ЕДИНСТВЕННЫЙ источник истины о состоянии схемы; таблица `_schema_migrations` не создаётся и не ведётся, Go-совместимость отслеживания не требуется (frozen-текст openspec/config.yaml по прецеденту D8 не редактируется).
    - Ревизия 2 (2026-08-18, решение человека): legacy knowledge.db от Go НЕ открывается, НЕ апгрейдится, данные из неё НЕ мигрируются — Rust всегда строит свою БД с нуля. Fixture `fixtures/knowledge.db` = dev-time ground truth только (read-only) для дифференциальных проверок; «old knowledge.db stays readable» из AGENTS.md перестаёт действовать (обновление доков — bookkeeping после коммита).
    - Ревизия 3 (2026-08-18, решение человека): файлы миграций оракула НЕ копируются — реплей мёртвой истории 5 шагов для чистой БД бессмыслен; вместо них ОДИН init-файл со squashed DDL финального v5-состояния. Будущие миграции — новые нумерованные каталоги `<id>-<slug>/up.sql` (формат лодера rusqlite_migration: id = часть имени до первого дефиса, парсится в usize); shipped не редактируются; forward-only, down.sql не нужны.
    - Ревизия 4 (2026-08-18, отклонение при имплементации, подтверждено человеком): записанное ожидание `knowledge OR RAG` → 3 хита **не воспроизводится** — измерено ровно 18 хитов (17 от `knowledge` + 1 от `RAG`, пересечения нет; FTS5-результат детерминирован для данного файла). Ранняя запись «3 хита» была артефактом замера с `LIMIT 3` (первые строки 1, 30, 88). Проверено двумя независимыми сборками SQLite (bundled 3.53.2 и системный CLI) + код-путём оракула (chunk_dao.go SearchFTS передаёт запрос в MATCH ? без преобразований); альтернативные интерпретации (AND=0, NEAR=0, wildcard=19, distinct docs=10) значения 3 не дают. Ожидание исправлено на 18.

## 2. S2 — шов ONNX Runtime (ort)

- [x] 2.1 Spike: загрузка ВСЕХ моделей реестра onnx.yaml из data/, эмбеддинги, детерминизм, RSS
  - **Цель:** доказать загрузку ORT 1.28 по механизму onnx.yaml; токензацию HF tokenizer.json; валидные эмбеддинги + детерминизм + замер throughput/peak-RSS (бюджет ноутбука) для КАЖДОЙ из 3 моделей реестра.
  - **Scope файлов:** `crates/spikes/src/bin/s2_onnx.rs` + зависимости (`ort`, tokenizers). Шаги: разобрать реестр из ../synopsis/configs/onnx.yaml — модели `bge-m3-int8` (vector_dim=1024), `bge-small-en-v1.5` (vector_dim=384; **default** реестра и config.default.yaml), `paraphrase-multilingual-MiniLM-L12-v2` (vector_dim=384); обеспечить наличие рантайма и ВСЕХ трёх моделей в data/ (скачать существующим механизмом или предзагрузить с пометкой — у оракула все три уже предзагружены в ../synopsis/data/models/); для каждой модели: сессия через ort (runtime ORT 1.28, совпадает с версией из onnx.yaml), эмбеддинг N=50 фиксированных текстов (тексты зашиты в исходник спайка; у каждой модели — её собственный pipeline входа/выхода по объявленным I/O, как оракул: selectOutputName sentence_embedding при наличии, иначе token_embeddings), вывод JSONL-векторов + тайминги (ms/текст) + peak RSS.
  - **Зависимости:** 0.2. Сеть: модель bge-m3-int8 ~2,3 ГБ (зафиксировано в design); остальные две — 133 МБ и 470 МБ (по реестру onnx.yaml).
  - **Критерии приёмки:** для КАЖДОЙ из 3 моделей: N=50 векторов dim по реестру (1024/384/384), конечные; два запуска бит-идентичны (in-process и cross-run); unit-norm в пределах eps; ОПЦИОНАЛЬНО mean-cos ≥ 0.999 против Go-дампа (не блокирует — design D5); ADR `docs/adr/0002-onnx-runtime.md` с выбором bindings crate и подходом загрузки рантайма + выбранным batch size + таблицей измерений по всем трём моделям.
  - **Референс:** ../synopsis/internal/onnx (механизм загрузки), configs/onnx.yaml (реестр: name/vector_dim/files/size_bytes каждой модели), configs/config.default.yaml (default-модель); ort — https://ort.pyke.io/.
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, решение человека): bindings crate заменён `onnxruntime-rs` → **`ort`** (pykeio). Факты: onnxruntime-rs неактивен («now-inactive» — по проекту преемника); ort 2.0.0-rc.13 обёртывает ровно ORT 1.28 (версия из нашего onnx.yaml), maintainer декларирует production-ready и рекомендует новым проектам. Замороженный entry `onnxruntime-rs` в openspec/config.yaml НЕ редактируется — отклонение фиксируется здесь + в ADR 0002 (конвенция D8: frozen-текст не меняется внутри задач).
    - Ревизия 2 (2026-08-18, решение человека): спайк проверял ТОЛЬКО `bge-m3-int8`; **расширен на все 3 модели реестра onnx.yaml** — `bge-small-en-v1.5` (384-dim) является default-моделью реестра и config.default.yaml, `paraphrase-multilingual-MiniLM-L12-v2` (384-dim) — третья реестровая модель; пропуск двух моделей означал бы непроверенность основного (default) пути оракула. Каждая модель — со своим pipeline по объявленным I/O и своими критериями (N=50, dim по реестру, детерминизм, unit-norm, тайминги, RSS).

## 3. S3 — выбор ANN-движка (usearch vs lance)

- [ ] 3.1 Spike S3a: измерения usearch на синтетической фикстуре 1M×1024
  - **Цель:** воспроизводимые метрики usearch HNSW при RAM-бюджете ноутбука.
  - **Scope файлов:** `crates/spikes/src/bin/s3a_usearch.rs` + зависимость usearch. Внутри спайка: генерация seeded синтетики (seed зашит, воспроизводимо); варианты индекса f32 и int8-квантизация (если поддерживается), M=16, sweep efConstruction {100, 200}; замер peak-RSS-delta при поиске, p50/p95 top-k=50 по 200 held-out запросам, recall@10 против brute-force ground truth (считается в спайке), размер файла на диске.
  - **Зависимости:** 0.2 (БД-фикстура не нужна — design D2).
  - **Критерии приёмки:** таблица метрик; воспроизводимость двух прогонов (<5% разброса); результаты в `docs/adr/spike-s3-results.md` (общий appendix, решение ещё нет).
  - **Референс:** ограничения — openspec/config.yaml context (RAM ноутбука, p95/recall-гейты).

- [ ] 3.2 Spike S3b: измерения lance на той же фикстуре + ADR-решение
  - **Цель:** те же метрики для lance HNSW/IVF; сравнение и запись решения.
  - **Scope файлов:** `crates/spikes/src/bin/s3b_lance.rs` + зависимость lancedb/lance; затем `docs/adr/0003-ann-engine.md`. Протокол идентичен S3a (одна фикстура, те же запросы).
  - **Зависимости:** 3.1 (общая фикстура и протокол для честного сравнения).
  - **Критерии приёмки:** ADR с решением «движок + конфигурация (M/efConstruction/efSearch/квантизация)» при выполнении p95 < 10 ms, recall@10 ≥ 0.95, RSS-delta ≤ ~2 ГБ; если ОБА движка не проходят — ADR помечает NO-GO и задача эскалируется пользователю до модульных change'ов.
  - **Референс:** метрики из docs/adr/spike-s3-results.md; ограничения — openspec/config.yaml context.

## 5. Завершение

- [ ] 5.1 Удалить crates/spikes, проверить workspace, зафиксировать решения
  - **Цель:** спайки одноразовы (design D1); ADR остаются единственным источником решений.
  - **Scope файлов:** удаление `crates/spikes/` + записи в корневом Cargo.toml; проверка сборки оставшегося workspace.
  - **Зависимости:** все задачи 1.x–3.x завершены, ADR 0001–0003 (+ appendix s3-results) записаны. MCP-транспорт закрыт решением D8 scaffold (rmcp Streamable HTTP; доказан round-trip тестами parity-harness) — отдельного спайка и ADR не требует.
  - **Критерии приёмки:** `cargo build/test/clippy/fmt --workspace` зелёные без spikes; docs/adr содержит все решения; сводка change (для архивации) перечисляет решения и остаточные риски (в т.ч. оговорку про синтетику S3).
  - **Референс:** design.md D1/D3.
