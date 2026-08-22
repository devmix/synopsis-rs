# vectors — Tasks

Порядок = граф зависимостей: 1.1 → {1.2, 1.3, 1.5} → 1.4 → {1.6, 1.7} → 1.8. Каждая задача выполняется СВЕЖИМ агентом без памяти предыдущих — тело самодостаточно. Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс / история ревизий). **Принцип миграции (binding, повторяется в каждой задаче):** НЕ транскрибировать Go 1:1 — функциональная копия, не кодовая; архитектурно правильно для Rust (DRY, KISS, SOLID, YAGNI); внутренняя совместимость с оракулом не требуется; баги Go исправлять или фиксировать осознанные отклонения. CI без сети: тесты с тяжёлыми прогонами — `#[ignore]`/release-only.

Общие факты для всех задач (источники истины): ADR `docs/adr/0003-ann-engine.md` (GO: lancedb 0.37/lance 10, IvfHnswSq u8-SQ, M=16, efConstruction=100, num_partitions=256, nprobes=32, **efSearch=200**, L2; гейты p95<10ms, recall@10≥0.95, RSS≤~2GB); design этого change'а D1–D8; формат SYNX — archive native-seam-spikes design D4 (`magic "SYNX" · version u32 LE=1 · dim u32 LE · count u64 LE · rows [u32 LE chunk_id][f32 LE × dim] × count`, по возрастанию chunk_id). Гейты каждой задачи: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -p <crate>`, `cargo test --workspace`; линты workspace: missing_docs=deny, unsafe_code=forbid, unwrap_used/expect_used → deny на гейте (тест-модули могут отключать локально). Toolchain пин 1.96.0; Cargo.lock коммитится. Пресуществующие untracked `.idea/`, `.opencode/opencode.json`, `.opencode/plans/` не трогать.

---

- [x] 1.1 Скаффолдинг крейта + feasibility-гейт lancedb
  - **Цель:** создать каркас `crates/vectors` (trait `VectorIndex`, `VectorIndexConfig` с дефолтами ADR 0003, `VectorsError`) и снять главный риск — тяжёлая зависимость lancedb: host-сборка, спот-кросс-компиляция, размер бинаря.
  - **Scope файлов:** `crates/vectors/Cargo.toml`, `crates/vectors/src/lib.rs`; корневой `Cargo.toml` (workspace-пин `lancedb`); `Cargo.lock`.
  - **Детали:** trait dyn-совместимый Send+Sync, методы sync (design D2): `insert`, `search`, `delete_by_chunk_ids`, `chunk_ids`, `count`, `flush`/`build_index`, `rebuild` — точную форму уточнить по задачам 1.3/1.4, но сигнатуры зафиксировать здесь (параметры `(chunk_id: u32, vector: &[f32])`, поиск → `Vec<(u32, f32)>`). `VectorIndexConfig { dim=1024, m=16, ef_construction=100, num_partitions=256, nprobes=32, ef_search=200 }` (design D4/D7). Зависимость: `lancedb = { version = "0.37", default-features = false }` + минимальные фичи для локального filesystem (точный состав верифицировать по исходникам registry `~/.cargo/registry/src/*/lancedb-0.37*/Cargo.toml` — нужны local/oss storage, НЕ aws/azure/gcp/huggingface); выделенный tokio runtime внутри движка появится в 1.3 — здесь только dep-компиляция.
  - **Feasibility-гейт (результат — в отчёте задачи, эскалация пользователю при провале):** (а) `cargo build -p vectors` зелёный (MSRV транзитивных deps vs 1.96.0); (б) `cargo zigbuild --release -p synopsis --target x86_64-pc-windows-gnu` и `--target aarch64-apple-darwin` завершаются (спот-проверка двух самых рискованных таргетов CI-матрицы); (в) размер `target/release/synopsis` до/после добавления dep — дельта записана (ориентир <50 МБ, жёсткого гейта нет — решение по результату за человеком).
  - **Критерии приёмки:** все гейты зелёные; trait+config+error задокументированы (missing_docs=deny); юнит-тесты дефолтов конфига и валидации (dim>0, k>0); feasibility-пункты (а)(б)(в) выполнены и записаны; при провале (а)/(б) — СТОП и эскалация до задач движка.
  - **Зависимости:** нет (первая задача change'а).
  - **Референс:** `docs/adr/0003-ann-engine.md`; design D1/D2/D5; заголовок текущего `crates/vectors/src/lib.rs` (комментарий D1 tier 0 — сохранить намерение); `.archive/spikes/Cargo.toml` (пин lancedb спайков).
  - **Ревизия 2 (2026-08-21, решение человека по эскалации feasibility (б)):** провал aarch64-apple-darwin доказан lancedb-специфичным (lance-arrow cdylib → core-foundation-sys → `-framework CoreFoundation`; zig 0.16 не шипует darwin framework-стабы, zig#1349). Выбран вариант **A**: вендорить минимальный `CoreFoundation.tbd`-стаб в репо + framework search path только для darwin-ноги CI. Scope расширен: `.github/workflows/ci.yml`, новый каталог `ci/darwin-sdk/` (стаб). Механизм: `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS` с `-C link-arg=-F<repo>/ci/darwin-sdk` (затрагивает только darwin-таргет); точный состав символов стаба — из ошибок линковки (~8 CF-символов iana-time-zone: CFTimeZoneCopySystem, CFTimeZoneGetName, CFStringGetBytes, CFStringGetCStringPtr, CFStringGetLength, CFRelease, …). Критерий (б)-darwin перевыполняется ЛОКАЛЬНО тем же механизмом, что в CI (env-var + zigbuild) — это условие закрытия задачи.

- [x] 1.2 Модуль synx: формат фикстур vectors.bin (SYNX)
  - **Цель:** потоковый reader/writer бинарного формата SYNX (контракт оракул ↔ harness) в крейте vectors.
  - **Scope файлов:** `crates/vectors/src/synx.rs`, `crates/vectors/src/lib.rs` (регистрация модуля).
  - **Детали:** writer принимает итератор `(chunk_id, &[f32])`, сортирует стабильно по возрастанию chunk_id перед записью (требование формата), пишет потоково; reader отдаёт итератор строк БЕЗ полной загрузки файла в память (целевой файл ~4 ГБ); ошибки: неверный magic/version, обрыв посреди строки, dim=0 — различимые варианты `VectorsError`. Endianness строго LE.
  - **Критерии приёмки:** roundtrip-тест (запись→чтение идентичны, порядок по chunk_id); golden-bytes тест на маленькой фикстуре (ручной hex-эталон заголовка + первой строки); тесты обрыва файла/неверного magic/неверной version; потоковость reader подтверждена тестом на файле больше буфера (или review-проверкой отсутствия collect()); гейты зелёные.
  - **Зависимости:** 1.1.
  - **Референс:** archive native-seam-spikes design D4 (формат дословно); design D6 этого change'а (почему модуль живёт в vectors).

- [x] 1.3 Движок: таблица, стриминговая вставка, индекс IvfHnswSq, kNN-поиск
  - **Цель:** `LanceEngine` — реализация ядра trait'а на lancedb: создание/открытие таблицы, батчевая вставка, построение индекса, поиск top-k.
  - **Scope файлов:** `crates/vectors/src/engine.rs`, `crates/vectors/src/lib.rs` (ре-экспорт).
  - **Детали:** схема таблицы: `chunk_id u32` + `vector FixedSizeList<Float32, dim>` (design D4); выделенный tokio Runtime внутри движка (worker_threads≤2), sync-фасад через `block_on` (design D2 — вызывать только вне async-контекста, задокументировать); вставка Arrow RecordBatch'ами (~1000 строк/батч); `build_index()` — IvfHnswSq с параметрами конфига; `search(&[f32], k)` → `Vec<(u32, f32)>` по `_distance` asc, с nprobes/efSearch из конфига; ошибки: открытие несуществующего индекса, несовпадение dim, пустой индекс → пустой результат (не ошибка).
  - **Критерии приёмки:** юнит-тесты на tempdir: create→insert(2–5K seeded векторов ×1024)→build_index→search возвращает отсортированный top-k; повторное open без перестроения видит данные; dim-mismatch → ошибка; пустой индекс → пусто; `num_partitions` в тестах уменьшен относительно дефолта (мелкий корпус); гейты зелёные; API lancedb 0.37 верифицирован по исходникам registry (не угадывать: `Index::IvfHnswSq`, колонки, query builder `.nprobes()/.ef()`).
  - **Зависимости:** 1.1.
  - **Референс:** design D2/D4; ADR 0003 (таблица решения); `.archive/spikes/src/bin/s3b_lance.rs` (рабочий код создания таблицы/индекса/поиска — перепроектировать, не транскрибировать).

- [ ] 1.4 Движок: каскадное удаление, перечисление, персистентность, пересоздание
  - **Цель:** операции жизненного цикла + примитивы синхронизации с SQLite (решение человека 2026-08-21).
  - **Scope файлов:** `crates/vectors/src/engine.rs`, `crates/vectors/src/lib.rs` (доки протокола).
  - **Детали:** `delete_by_chunk_ids(&[u32])` — батчево, идемпотентно к отсутствующим id; `chunk_ids()` — перечисление всех id индекса (для reconciliation «индекс − SQLite»); `count()`; `rebuild(vectors)` — атомарная замена содержимого (drop+create или эквивалент lance); персистентность: данные живут в каталоге data_dir, reopen видит всё. В module docs lib.rs — протокол каскада (design D3): удаление векторов ДО строк чанков в SQLite; толерантность поиска — обязанность потребителя; предельный ремонт — полная пересборка.
  - **Критерии приёмки:** тесты: delete убирает из результатов поиска; повторный delete тех же id — ok; chunk_ids()/count() точны после insert/delete; rebuild заменяет без накопления; цикл close→reopen сохраняет всё; гейты зелёные.
  - **Зависимости:** 1.3.
  - **Референс:** design D3 (трёхслойный протокол + анализ режимов сбоя); `../synopsis/internal/gc/documents_gc.go` FullClearDocByID шаг 5 (порядок «векторы → чанки» сохранён из оракула).

- [ ] 1.5 Конфиг: опциональная секция `vectors:` в preset
  - **Цель:** аддитивное расширение config-format (решение человека 2026-08-21): секция с полями индекса, дефолты ADR 0003.
  - **Scope файлов:** `crates/config/src/preset.rs` (+ модуль при необходимости), тесты config.
  - **Детали:** `#[serde(default, skip_serializing_if = "Option::is_none")]`-семантика: отсутствие секции → дефолты (dim=1024, m=16, ef_construction=100, num_partitions=256, nprobes=32, ef_search=200); секция хранит сырые поля — БЕЗ типов крейта vectors (направление зависимостей, design D7); маппинг — wiring в будущих change'ах.
  - **Критерии приёмки:** тесты: пресет без секции → дефолты; с секцией → переопределения; YAML-roundtrip; существующие тесты config зелёные; гейты зелёные.
  - **Зависимости:** 1.1 (дефолты зафиксированы), независимо от 1.2–1.4.
  - **Референс:** design D7; openspec/specs/config-format/spec.md («Неизвестные ключи не ломают старт» — аддитивность безопасна).

- [ ] 1.6 Интеграционные гейты: recall@10 и латентность на синтетике
  - **Цель:** машинные гейты ADR 0003 на CI-масштабе (без сети, секунды прогона).
  - **Scope файлов:** `crates/vectors/tests/integration_gates.rs` (+ фикстуры в tests/ при необходимости).
  - **Детали:** seeded синтетика ~20K×1024 (гauss-смесь кластеров как в спайке, seed зашит); ground truth — exact-L2 brute force в процессе; **recall@10 ≥ 0.95** — выполняется всегда (корректность не зависит от профиля сборки); **p95 < 10 ms** — тест помечен `#[ignore]`, запуск `cargo test -p vectors --release -- --ignored` (урок спайка: debug-тайминги бессмысленны, design D8); сценарии удаления/персистентности из 1.4 прогоняются на этом корпусе.
  - **Критерии приёмки:** recall-тест зелёный в обычном `cargo test`; latency-тест зелёный в release-прогоне (исполнитель запускает и приводит числа в отчёте); полный suite стабилен (повторный запуск — тот же recall ±шум); гейты зелёные.
  - **Зависимости:** 1.4.
  - **Референс:** design D8 (уровень 1); `docs/adr/spike-s3-results.md` (протокол: warmup, held-out запросы, два прогона).

- [ ] 1.7 parity-harness: загрузчик SYNX вместо TODO
  - **Цель:** закрыть TODO vectors.bin в parity-harness через `vectors::synx`.
  - **Scope файлов:** `crates/parity-harness/Cargo.toml` (+dep vectors), соответствующий модуль harness'а.
  - **Детали:** API загрузки фикстуры (файл → векторы/итератор) + расчёт recall@k против переданного ground truth уже частично есть — дополнить недостающим; тест на маленькой сгенерированной фикстуре (без сети).
  - **Критерии приёмки:** TODO удалён; `cargo test -p parity-harness` зелёный; гейты зелёные.
  - **Зависимости:** 1.2 (synx), независимо от 1.3–1.6.
  - **Референс:** AGENTS.md («vectors.bin fixture format: stubbed with a TODO»); design D6.

- [ ] 1.8 Полный бенчмарк N=1M + повтор на реальной фикстуре + appendix к ADR
  - **Цель:** закрыть открытые вопросы ADR 0003 №2: полный масштаб и реальная геометрия bge-m3.
  - **Scope файлов:** `crates/vectors/tests/full_benchmark.rs` (или bin, `#[ignore]`), `docs/adr/0003-ann-engine.md` (appendix), опционально `docs/adr/vectors-benchmark.md`.
  - **Детали:** протокол s3b_lance (warmup 10, 100 held-out запросов, top-k=50, два прогона <5% разброса, peak-RSS-delta, размер на диске) на (а) синтетике N=1M×1024, (б) реальной фикстуре vectors.bin через synx-загрузчик — когда экспорт оракула доступен (если недоступен — пункт (б) помечается отложенным и это НЕ блокер архивации change'а). Только `--release`.
  - **Критерии приёмки:** команды запуска задокументированы; прогон (а) выполнен исполнителем на этой машине, результаты (p50/p95/recall/RSS/диск) внесены в appendix ADR с датой и версией lancedb; гейты ADR подтверждены ИЛИ отклонение эскалировано пользователю (митигация ADR: поднять efSearch/nprobes — параметры runtime, перестройка не требуется); пункт (б) выполнен или явно отложен.
  - **Зависимости:** 1.6 (+1.2 для пункта (б)).
  - **Референс:** design D8 (уровень 2); `docs/adr/spike-s3-results.md`; ADR 0003 раздел «Открытые вопросы».
