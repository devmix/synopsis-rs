# Design: add-usearch-ann-engine

## Контекст и мотивация
`lancedb` (→ `lance` → `datafusion` + `arrow` + `parquet` + `object_store`) — единственный источник
тяжёлой сборки: ~2296 crate-узлов, релизный бинарь 79 МБ, `target/` 42 ГБ. `datafusion` попадает в
граф ТОЛЬКО через `lancedb` (подтверждено `cargo tree -i datafusion`). Цель — дать альтернативу с
меньшим footprint, не ломая текущий lance-путь, и измерить оба движка на реальных корпусах.

## Решение: гибридный выбор движка (одобрено человеком 2026-08-29)
- **Compile-time**: Cargo-фичи `engine-lance` (default) и `engine-usearch` (opt-in). В период
  сравнения default-фичи = **обе**. После решения default = только выбранный движок → сборка
  ускоряется, бинарь уменьшается. Dev-сборка только usearch:
  `cargo build --no-default-features --features engine-usearch`.
- **Runtime**: поле `vectors.engine` (`"lance"` | `"usearch"`) в конфиге выбирает инстанцируемый
  движок. Если обе фичи включены, а поле не задано — дефолт `lance` (обратная совместимость);
  невалидное значение — ошибка.
- **Dispatch**: `enum VectorEngine::{Lance(LanceEngine), Usearch(UsearchEngine)}` с `match`,
  возвращающий `Arc<dyn VectorIndex>`. Сигнатура `open_vectors_engine()` не меняется, поэтому
  `search`/`ingestion`/`mcp` остаются нетронутыми (они держат `Arc<dyn VectorIndex>`).

## Почему usearch, а не альтернативы
- **usearch** v2.26.1 (Apache-2.0, обновлён 2026-08-22, ~980k загрузок): C++11-ядро через `cxx`
  (тянет только `cxx`+`cxx-build`, НЕ datafusion/arrow). Disk-backed через `Index::restore(path, view=true)`/
  `view()` (mmap, чтение с диска без загрузки в RAM). Квантование `i8`/`u8`/`bf16`/`f16`/`e5m2`/`e4m3`/
  `b1x8`. HNSW, метрики L2sq/IP/Cos. `filtered_search(predicate)` фильтрует при обходе графа.
- **hnsw-rs** (чистый Rust): нет нативного квантования/disk-backed (хранит f32 в RAM; при 1M×1024 f32
  ~4 ГБ — не вписывается в бюджет 16 ГБ вместе с остальным). Отклонён: не закрывает жёсткое
  ограничение «quantized disk-backed».
- **faiss** (C++ bindings): тяжёлый нативный build + BLAS/OpenMP, ещё тяжелее lance. Отклонён.
- **tantivy** (векторный поиск внутри поискового движка): дублирует наш FTS5 и тоже тяжёл. Отклонён.
- **Оставить lance + sccache**: смягчает боль пересборки, но НЕ уменьшает бинарь и не убирает
  datafusion из графа. Это запасной путь, не альтернатива замене движка.

## Параметры usearch (паритет с lance, выбрано u8)
- `dim = 1024`, метрика **L2sq** (`MetricKind::L2sq`) — эквивалентна L2 по ранжирову (монотонное
  преобразование); search-крат использует только порядок для RRF-фьюжна.
- Квантование **U8** (`ScalarKind::U8`) — выбрано человеком (ближе к u8-SQ lance по recall, чем i8).
  8-битные векторы: ~1 МБ на 1M×1024 вместо 4 ГБ f32.
- HNSW-параметры (аналоги lance M/efConstruction/efSearch; IVF-параметры lance `num_partitions`/
  `nprobes` к pure-HNSW usearch неприменимы): `connectivity = 16`, `expansion_add = 100`,
  `expansion_search = 200`.
- Batch-insert: usearch Rust-API не имеет batch — делаем chunked single `add` через rayon-пул
  (индекс concurrent). Штраф ~2–4× против Arrow-batch lance при N≥10K, но ingestion упёрт в ONNX-
  эмбеддинг, поэтому приемлемо.

## On-disk layout
Под `<dataset>/state/vectors/` создаются engine-тегнутые подкаталоги: `vectors/lance/` (LanceDB
`.lance` dirs) и `vectors/usearch/` (usearch `.usearch` file). `vectors_path()` возвращает базу;
движок резолвит свой подкаталог. `recreate_vectors_engine` и `db clear` чистят только активный
подкаталог. Детект несовпадения размерности работает для обоих.

## Изменение замороженного контракта (config-format)
Поле `vectors.engine` — аддитивное расширение секции `vectors:` (сама секция уже аддитивна по
решению 2026-08-21). Явное решение: человек одобрил (2026-08-29). Зафиксировано в delta-спеке
`config-format`.

## Пересмотр ADR 0003
ADR 0003 (2026-08-18) выбрал lance и отклонил usearch. Настоящий change НЕ отменяет ADR 0003
безусловно: usearch добавляется как параллельный кандидат, финальный выбор — после бенчмарка
(задача 1.4 + решение человека). Если usearch проходит пороги — follow-up change переключает default
на `engine-usearch` и удаляет `lancedb` (ADR 0003 обновляется). Иначе lance остаётся default,
usearch-добавление архивируется.

## unsafe_code
`crates/vectors` получает локальное `#![allow(unsafe_code)]` (или эквивалент через cfg), обосновано
cxx FFI. Workspace-wide `unsafe_code = "forbid"` сохраняется. AGENTS.md разрешает per-crate релакс
для native-seam работы.

## Референсы Go-оракула
Нет. ANN-движок — внутренний компонент Rust, у Go-оригинала (`../synopsis`) нет эквивалента
(там свой vec0/ANN). Контракт — наш `VectorIndex` трейт; паритет семантики проверяется юнит-тестами,
зеркальными `LanceEngine`, и бенчмарком recall@k/p50/p95 в parity-harness.

## Критерии решения (пороги, одобрены человеком)
- recall@k usearch в пределах **2%** от базовой lance (на тех же корпусах/фикстурах).
- p50 поиска usearch в пределах **20%** от lance; p95 — в пределах **30%**.
- Сборка только usearch: инкремент < 30с, clean < 5 мин; дельта размера бинаря < 5 МБ.
- `unsafe` локализован в `crates/vectors` (допустимо).

## Зависимости и порядок
См. tasks.md: 1.1 spike → 1.2 UsearchEngine → 1.3 feature-gating+config+dispatch → 1.4 parity A/B →
1.5 on-disk layout+lifecycle. Каждая задача самодостаточна для свежего агента (~100k токенов).
