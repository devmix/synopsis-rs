# Tasks: add-usearch-ann-engine

- [x] 1.1 Spike: usearch cross-compilation + API verification (add dep, features, prove cxx FFI builds on 5 CI targets + core API works)
- [x] 1.2 Implement `UsearchEngine` behind `VectorIndex` trait (bf16 quantization, L2sq, disk-backed)
- [x] 1.3 Feature gating + `vectors.engine` config field + `open_vectors_engine` dispatch (enum → `Arc<dyn VectorIndex>`)
- [x] 1.4 Parity-harness A/B benchmark (recall@k + p50/p95 for both engines, build/size delta)
- [x] 1.5 Engine-tagged on-disk layout + lifecycle integration (`vectors/lance/` vs `vectors/usearch/`, recreate + db clear)

---

## 1.1 Spike: usearch cross-compilation + API verification

**Goal:** Prove `usearch` (cxx FFI) compiles on all 5 CI targets and that its core API
(create/add/search/save/restore) works, before investing in the full engine.

**Scope файлов:**
- `Cargo.toml` — добавить `usearch = { version = "2.26", default-features = false }` и `cxx = "1"` в
  workspace-палитру (под `engine-usearch` фичу через `crates/vectors/Cargo.toml`).
- `crates/vectors/Cargo.toml` — добавить фичи `[features] engine-lance = [], engine-usearch = ["dep:usearch","dep:cxx"]`
  (engine-lance default), `usearch`/`cxx` как `optional = true` dep; `cxx-build` как build-dep.
- `crates/vectors/src/usearch_engine.rs` — минимальный stub модуля под `#[cfg(feature = "engine-usearch")]`
  с тестом: `Index::new`, `add`, `search`, `save`, `Index::restore(path, view=true)`.
- `crates/vectors/src/lib.rs` — `mod usearch_engine;` под cfg-гейтом.

**Dependencies:** нет.

**Критерии приёмки (машинные):**
- `cargo zigbuild --release --target x86_64-unknown-linux-musl` успешен с `engine-usearch`.
- То же для `aarch64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`,
  `aarch64-apple-darwin` (если таргеты доступны в CI; локально — хотя бы musl + gnu linux).
- Локальный тест: create → add 100 векторов dim=1024 → search top-10 → save → restore(view) →
  search даёт те же ключи. `cargo test -p vectors --features engine-usearch` зелёный.
- `cargo clippy --all-targets -- -D warnings` и `cargo fmt --check` чисты (в т.ч. под обеими фичами).
- Если кросс-компиль проваливается на каком-то таргете — зафиксировать в отчёте и ВЕРНУТЬСЯ к
  планированию (не продолжать 1.2).

**Oracle refs:** N/A (внутренний движок; у Go-оригинала нет эквивалента). Паритетный шаблон тестов —
`crates/vectors/src/engine.rs` (LanceEngine) и `crates/vectors/src/lib.rs` (трейт `VectorIndex`,
строки ~179–211).

**Cross-compile resolution (verified 2026-08-30):** `x86_64-pc-windows-gnu` собирается только с
обходом case-sensitivity заголовков zig. Ядро usearch включает `<Windows.h>` (capital W), а zig 0.16.0
бандлит заголовки как `windows.h` (lowercase) в `/usr/lib/zig/libc/include/any-windows-any` — на
case-sensitive Linux FS include не резолвится. Рабочий способ (применять в CI для этого таргета):
сгенерировать overlay только с capital-case symlinks (`${f^}` для каждого `*.h`) в отдельный каталог,
и передать `CXXFLAGS="/path/to/cap-overlay:-I/usr/lib/zig/libc/include/any-windows-any"`
(overlay — ПЕРВЫМ, реальный zig-dir — ПОСЛЕ libc++, иначе теневой `errno.h` ломает `<cerrno>` cxx).
`CPATH` не работает (фильтруется cc-rs). Остальные 4 таргета (`x86_64/aarch64-unknown-linux-musl`,
`aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`) собираются без обхода.

---

## 1.2 Implement `UsearchEngine` behind `VectorIndex` trait

**Goal:** Полноценная реализация `UsearchEngine`, реализующая все 8 методов `VectorIndex` с
идентичной семантикой `LanceEngine`.

**Scope файлов:**
- `crates/vectors/src/usearch_engine.rs` (~250 LOC, под `#[cfg(feature = "engine-usearch")]`):
  - `pub struct UsearchEngine { index: Index, config: VectorIndexConfig, path: PathBuf }`.
  - `create(path, config)` / `open(path, config)` — `Index::new` (dim=config.dim, metric=L2sq,
    dtype=U8) или `Index::restore(path, view=true)`; при `NotFound` → `create`.
  - `insert` / `insert_batch` — chunked single `add` через `rayon` (индекс concurrent); квантование
    U8 выполняет usearch при `dtype=U8` (передаём f32, usearch сам нормализует/квантует для cos-метрик;
    для L2sq — передаём f32, usearch хранит U8). Уточнить у docs.rs/usearch поведение dtype для L2sq.
  - `search(query, k)` — `index.search(query, k)` → `Vec<(u32,f32)>`, отсортированные по distance asc.
  - `delete_by_chunk_ids` — `index.remove(ids)` (идемпотентно).
  - `chunk_ids` — итерация ключей индекса → `Vec<u32>`.
  - `count` — `index.size()` (u64).
  - `build_index` — для usearch HNSW строится инкрементально при add; `build_index` может быть no-op
    или вызовом `index.save` (уточнить API). Семантика: после insert индекс готов к search.
  - `rebuild(rows)` — `index.clear()` (или новый Index в том же path) + batch add + save; атомарно
    заменяет содержимое, пустой `rows` → пустой индекс.
  - Параметры HNSW: `connectivity=16, expansion_add=100, expansion_search=200` (из config или константы).
- `crates/vectors/src/lib.rs` — `pub mod usearch_engine;` + `pub use usearch_engine::UsearchEngine;`
  под cfg-гейтом; док-комментарий о U8/L2sq.

**Dependencies:** 1.1.

**Критерии приёмки (машинные):**
- Все 8 методов `VectorIndex` реализованы и компилируются под `engine-usearch`.
- Юнит-тесты (зеркальные LanceEngine, см. `crates/vectors/src/engine.rs` + `lib.rs` тесты):
  create → insert → search возвращает top-k по возрастанию distance; delete_by_chunk_ids идемпотентен;
  rebuild не накапливает (повторный rebuild с теми же rows = те же ключи); пустой rebuild очищает;
  dim-mismatch (insert/search неверной размерности) → `VectorsError::DimensionMismatch`; reopen из
  сохранённого path восстанавливает данные.
- `cargo test -p vectors --features engine-usearch` зелёный; `clippy -D warnings`, `fmt --check` чисты.

**Oracle refs:** N/A. Шаблон семантики — трейт `VectorIndex` (`crates/vectors/src/lib.rs:179`) и
тесты `LanceEngine` (`crates/vectors/src/engine.rs`).

---

## 1.3 Feature gating + `vectors.engine` config field + `open_vectors_engine` dispatch

**Goal:** Связать фичи, конфиг и фабрику: runtime-выбор движка через `vectors.engine`.

**Scope файлов:**
- `Cargo.toml` — workspace `[features]` при необходимости (если vectors-фичи нужно экспортировать);
  убедиться, что `engine-lance` default в `crates/vectors/Cargo.toml`.
- `crates/vectors/Cargo.toml` — `usearch`/`cxx` под `engine-usearch`; `lancedb` под `engine-lance`.
- `crates/config/src/lib.rs` (или `preset.rs`) — поле `engine: Option<String>` в `VectorsConfig`
  (сериализация: `#[serde(default)]`); валидация значений `lance`/`usearch` (иначе ошибка).
- `crates/vectors/src/lib.rs` — `pub enum VectorEngine { Lance(LanceEngine), Usearch(UsearchEngine) }`
  (под cfg) + `pub fn open_vectors_engine(boot) -> Result<Arc<dyn VectorIndex>, ...>` (или перенос
  логики из `crates/cli/src/serve/bootstrap.rs:448` сюда, если чище) — диспетчеризация по
  `config.vectors.engine` с учётом включённых фич; невалидное/недоступное → `VectorsError`.
- `crates/cli/src/serve/bootstrap.rs` — `open_vectors_engine` (строка ~448) заменить вызовом фабрики;
  убрать прямой `LanceEngine::open`/`create`; `use vectors::{VectorEngine, VectorIndex, ...}`.

**Dependencies:** 1.2.

**Критерии приёмки (машинные):**
- Default-фичи = `engine-lance` только → бинарь собирается и работает как раньше (back-compat).
- `cargo build --no-default-features --features engine-usearch` успешен (без lancedb в графе — проверить
  `cargo tree -i datafusion` пусто).
- `cargo build --features engine-lance,engine-usearch` успешен.
- Конфиг без `vectors.engine` → lance; `vectors.engine: "usearch"` (фича вкл) → UsearchEngine;
  `vectors.engine: "foo"` → ошибка; `vectors.engine: "usearch"` без фичи → ошибка.
- `open_vectors_engine` возвращает `Arc<dyn VectorIndex>` (сигнатура не изменилась).
- `cargo clippy --all-targets -- -D warnings` и `cargo fmt --check` чисты для комбинаций
  `(default)`, `(engine-usearch)`, `(engine-lance,engine-usearch)`.

**Oracle refs:** N/A. Точка инстанцирования — `crates/cli/src/serve/bootstrap.rs:448`
(`open_vectors_engine`), трейт — `crates/vectors/src/lib.rs:179`.

---

## 1.4 Parity-harness A/B benchmark

**Goal:** Измерить recall@k + p50/p95 для обоих движков на одних фикстурах и дельту размера/времени сборки.

**Scope файлов:**
- `crates/parity-harness/Cargo.toml` — фичи `engine-lance`/`engine-usearch` (наследуются от vectors).
- `crates/parity-harness/src/metrics.rs` — `recall@k` уже есть; добавить функцию A/B: принимает две
  реализации `Arc<dyn VectorIndex>` (или строит их по фиче), один набор запросов из `fixtures/vectors.bin`,
  считает recall@k и p50/p95 для каждой.
- `crates/parity-harness/tests/parity_test.rs` — интеграционный тест: строит индекс обоими движками
  (если фича включена), гонит 20+ запросов, assert: оба meet гейты (lance recall@10 ≥ 0.95; usearch
  recall@k в пределах 2% от lance, p50 ≤ 1.2×, p95 ≤ 1.3×). Выводит сравнительную таблицу
  (recall@k, p50, p95, build-time, binary-size delta).

**Dependencies:** 1.3.

**Критерии приёмки (машинные):**
- Тест собирается и под `engine-lance`, и под `engine-usearch`, и под обе.
- recall@k и p50/p95 выводятся для обоих движков на одних фикстурах (`fixtures/vectors.bin`, SYNX).
- Дельта размера бинаря и времени сборки (usearch-only vs lance) замеряется/выводится.
- Тест зелёный, когда оба движка meet пороги решения (см. design.md). `clippy -D warnings`, `fmt` чисты.

**Oracle refs:** `crates/parity-harness/fixtures/vectors.bin` (формат SYNX, контракт оракул↔harness),
существующий паттерн p50/p95-гейта в `crates/parity-harness/tests/parity_test.rs`.

---

## 1.5 Engine-tagged on-disk layout + lifecycle integration

**Goal:** Разнести движки по подкаталогам и сделать `recreate_vectors_engine`/`db clear` корректными для обоих.

**Scope файлов:**
- `crates/config/src/lib.rs` (или `preset.rs`) — helper `DatasetConfig::vectors_engine_path(engine)`
  → `<vectors_path>/<engine>/` (lance | usearch).
- `crates/vectors/src/lib.rs` — фабрика резолвит подкаталог по выбранному движку.
- `crates/cli/src/serve/bootstrap.rs` — `open_vectors_engine` использует engine-тегнутый путь;
  `recreate_vectors_engine` (если есть) чистит только подкаталог активного движка.
- `crates/cli/src/db.rs` — `clear_dataset`/`clear_dataset_tables` учитывает оба подкаталога
  (`vectors/lance/` и `vectors/usearch/`) при удалении state-каталога (уже удаляет весь state dir —
  проверить, что подкаталоги попадают).

**Dependencies:** 1.3.

**Критерии приёмки (машинные):**
- LanceEngine хранит по `vectors/lance/`, UsearchEngine — по `vectors/usearch/`.
- `recreate_vectors_engine` чистит только подкаталог активного движка (второй нетронут).
- Повторное открытие движка по пути другого движка → `VectorsError::NotFound` (не падение).
- Детект несовпадения размерности работает для обоих движков.
- `cargo test -p vectors -p cli --features engine-lance,engine-usearch` зелёный; `clippy -D warnings`,
  `fmt --check` чисты.

**Oracle refs:** N/A. Жизненный цикл — `crates/cli/src/serve/bootstrap.rs` (`open_vectors_engine`,
`recreate_vectors_engine`) и `crates/cli/src/db.rs` (`clear_dataset`).
