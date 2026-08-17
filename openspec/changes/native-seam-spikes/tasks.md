# Tasks — native-seam-spikes

Порядок = граф зависимостей. Задачи 1.x–4.x независимы после 0.x → оркестратор может выделить их РАЗНЫМ свежим агентам параллельно (каждая самодостаточна). Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс).

## 0. Подготовка

- [ ] 0.1 Fixture knowledge.db из оракула + фиксация provenance
  - **Цель:** реальный v5-файл БД с данными для S1 и будущих parity-проверок.
  - **Scope файлов:** `fixtures/knowledge.db` (gitignored — добавить `fixtures/` в .gitignore) + `fixtures/README.md` (команда генерации, дата, масштаб, счётчики строк по таблицам).
  - **Зависимости:** нет (первая задача). Только СУЩЕСТВУЮЩИЕ инструменты оракула — ноль изменений кода в ../synopsis.
  - **Критерии приёмки:** файл существует и открывается; README фиксирует provenance; rusqlite-проба (одноразовый bin внутри спайк-crate'а) подтверждает таблицы + FTS5-содержимое.
  - **Референс:** ../synopsis/site/docs/guides/load-testing.mdx (load-test --scale medium или sync на выборке — выбрать по докам), ../synopsis/AGENTS.md (make build/sync).

- [ ] 0.2 Crate `crates/spikes` — скелет
  - **Цель:** единая точка сборки прототипов S1–S4 (design D1).
  - **Scope файлов:** `crates/spikes/Cargo.toml` + `src/bin/s1_sqlite.rs`, `s2_onnx.rs`, `s3a_usearch.rs`, `s3b_lance.rs`, `s4_mcp.rs` — заглушки (println "not implemented"); регистрация в workspace-членах корня.
  - **Зависимости:** scaffold-rust-project заведён (workspace существует).
  - **Критерии приёмки:** `cargo build --workspace`, clippy, fmt, test — зелёные; бинари запечатаны заглушками.
  - **Референс:** layout — design.md D1.

## 1. S1 — шов SQLite/FTS5 (rusqlite bundled+fts5)

- [ ] 1.1 Spike: открытие v5-БД, идемпотентность миграций, bm25-паритет
  - **Цель:** доказать, что Rust открывает knowledge.db оракула; миграции 001–005 повторно применяются как no-op; FTS5 bm25-запросы дают идентичные строки.
  - **Scope файлов:** `crates/spikes/src/bin/s1_sqlite.rs` + зависимости (rusqlite bundled+fts5). Шаги внутри спайка: открыть fixture с PRAGMA из ../synopsis/configs/config.default.yaml (WAL, mmap_size, synchronous, cache_size); применить SQL-файлы ../synopsis/migrations по порядку и проверить нулевое изменение данных (счётчики до/после); 3–5 фиксированных FTS5 bm25-запросов сравнить с референсным выводом sqlite3 CLI на том же файле.
  - **Зависимости:** 0.1, 0.2.
  - **Критерии приёмки:** идентичные результаты запросов; ADR `docs/adr/0001-sqlite-fts5.md` (GO/NO-GO + заметки по PRAGMA/WAL); clippy/fmt чистые в spikes.
  - **Референс:** ../synopsis/migrations/*.sql, ../synopsis/internal/database (PRAGMA), configs/config.default.yaml.

## 2. S2 — шов ONNX Runtime (onnxruntime-rs)

- [ ] 2.1 Spike: загрузка bge-m3 int8 из data/, эмбеддинги, детерминизм, RSS
  - **Цель:** доказать загрузку ORT 1.28 по механизму onnx.yaml; токензацию HF tokenizer.json; валидные эмбеддинги + детерминизм + замер throughput/peak-RSS (бюджет ноутбука).
  - **Scope файлов:** `crates/spikes/src/bin/s2_onnx.rs` + зависимости (onnxruntime-rs, tokenizers). Шаги: разобрать реестр из ../synopsis/configs/onnx.yaml; обеспечить наличие рантайма и модели в data/ (скачать существующим механизмом или предзагрузить с пометкой); сессия bge-m3 int8; эмбеддинг N=50 фиксированных текстов (тексты зашиты в исходник спайка); вывод JSONL-векторов + тайминги (ms/текст) + peak RSS.
  - **Зависимости:** 0.2. Сеть: модель ~2,3 ГБ (зафиксировано в design).
  - **Критерии приёмки:** 50 векторов dim=1024, конечные; два запуска бит-идентичны; unit-norm в пределах eps; ОПЦИОНАЛЬНО mean-cos ≥ 0.999 против Go-дампа (не блокирует — design D5); ADR `docs/adr/0002-onnx-runtime.md` с подходом загрузки рантайма и выбранным batch size.
  - **Референс:** ../synopsis/internal/onnx (механизм загрузки), configs/onnx.yaml.

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

## 4. S4 — шов MCP-транспорта (axum SSE)

- [ ] 4.1 Spike: SSE-сервер с одним tool `search`, паритет JSON-схемы
  - **Цель:** доказать транспорт HTTP SSE и идентичность схемы tool'а `search` с оракулом.
  - **Scope файлов:** `crates/spikes/src/bin/s4_mcp.rs` + зависимости (axum; rmcp или mcp-types — попробовать оба, если быстро). Шаги: сервер на эфемерном порту с GET /sse + POST /message; регистрация ОДНОГО tool'а `search` со схемой, транскрибированной из ../synopsis/internal/mcp/tools.go (имена/типы/обязательность — дословно); handler возвращает фикстуру-ответ (реальный поиск не нужен); тестовый клиент делает SSE roundtrip; JSON-diff tools/list против ЖИВОГО Go-сервера с нормализацией описаний.
  - **Зависимости:** 0.2 + prerequisite: собранный Go-бинарь (`make build` в ../synopsis) и сервер с данными (fixture из 0.1).
  - **Критерии приёмки:** SSE roundtrip успешен; JSON-diff схемы `search` пуст после нормализации; ADR `docs/adr/0004-mcp-transport.md` (выбор rmcp vs mcp-types+axum и детали транспорта).
  - **Референс:** ../synopsis/internal/mcp/tools.go, transport по AGENTS.md оракула (`GET /sse`, `POST /message`).

## 5. Завершение

- [ ] 5.1 Удалить crates/spikes, проверить workspace, зафиксировать решения
  - **Цель:** спайки одноразовы (design D1); ADR остаются единственным источником решений.
  - **Scope файлов:** удаление `crates/spikes/` + записи в корневом Cargo.toml; проверка сборки оставшегося workspace.
  - **Зависимости:** все задачи 1.x–4.x завершены, ADR 0001–0004 (+ appendix s3-results) записаны.
  - **Критерии приёмки:** `cargo build/test/clippy/fmt --workspace` зелёные без spikes; docs/adr содержит все решения; сводка change (для архивации) перечисляет решения и остаточные риски (в т.ч. оговорку про синтетику S3).
  - **Референс:** design.md D1/D3.
