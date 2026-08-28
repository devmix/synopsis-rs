# Tasks: ship-config-presets

Зависимости: config, graph, ingestion — готовы и проходят gates. Оракул (read-only):
`../synopsis/configs/onnx.yaml`, `../synopsis/configs/config.default.yaml`,
`../synopsis/configs/prompts/{entity-linker,ner}/*.tmpl`. Дизайн: design.md D1–D5.
Гейты каждой задачи: `cargo fmt --check`, `cargo clippy -p <crate> --all-targets -- -D warnings`,
`cargo test -p <crate>`; workspace-гейты гоняет ТОЛЬКО оркестратор. Директива (binding):
НЕ копировать Go 1:1; промпты переписываются под minijinja (не Go text/template).
Frozen-contract: `configs/*.yaml` — реализуется существующий контракт, НЕ меняется
(flags/defaults верны; prompts — minijinja по решению 2026-08-28).

- [x] 1.1 Исправить расположение встроенных промптов graph (корректировка пользователя)
  - Цель: убрать ссылку на `fixtures/templates` из продакшн-кода graph, сделать как в NER.
  - Scope файлов: `crates/graph/src/prompts.rs` (две строки `include_str!`; файл НЕ переименовывать),
    `crates/graph/fixtures/templates/entity-linker/{system,user}.tmpl` (перенос),
    `crates/graph/src/templates/entity-linker/{system,user}.tmpl` (новые).
  - Содержание: перенести `crates/graph/fixtures/templates/entity-linker/system.tmpl`
    и `user.tmpl` → `crates/graph/src/templates/entity-linker/` (создать каталог).
    В `prompts.rs` заменить
    `include_str!("../fixtures/templates/entity-linker/system.tmpl")` →
    `include_str!("templates/entity-linker/system.tmpl")` (и аналогично user). Путь в
    `include_str!` резолвится относительно файла `crates/graph/src/prompts.rs`, т.е.
    относительно `crates/graph/src/` → `crates/graph/src/templates/entity-linker/system.tmpl`.
    `prompts.rs` остаётся плоским файлом: НЕ делать `prompts/mod.rs` и НЕ создавать
    `src/prompts/templates/` (корректировка пользователя — rename избыточен). Удалить
    опустевший `crates/graph/fixtures/templates/entity-linker/`.
  - Revision history: (2026-08-28) Пользователь отклонил переименование
    `prompts.rs`→`prompts/mod.rs`. Требуется перенести шаблоны в `crates/graph/src/templates/`
    и оставить `prompts.rs` плоским файлом. `include_str!("templates/...")` из плоского
    `prompts.rs` резолвится в `src/templates/...` — rename не нужен.
  - Тесты: `cargo test -p graph` (существующие тесты в prompts.rs уже используют
    встроенные промпты через `load_entity_linker_prompts("/nonexistent/prompts-path")`
    и temp-overrides — они НЕ ссылаются на fixture-файлы; убедиться, что ни один
    тест/модуль не ссылается на `fixtures/templates`).
  - Критерии приёмки: `cargo clippy -p graph --all-targets -- -D warnings` и
    `cargo test -p graph` зелёные; `grep -rn "fixtures/templates" crates/graph/` пуст;
    `include_str!` резолвится (сборка успешна).

- [x] 1.2 Поставить `configs/onnx.yaml` (verbatim из оракула)
  - Цель: бинарь без `--config` находит реестр моделей/рантайма.
  - Scope файлов: `configs/onnx.yaml` (новый, repo root),
    `crates/config/src/onnx.rs` (чтение — без изменений, только путь по умолчанию
    `configs/onnx.yaml`).
  - Содержание: скопировать `../synopsis/configs/onnx.yaml` в repo-root `configs/onnx.yaml`
    побайтово (структура: `runtime.platforms[*]` + `models.entries[*]` для bge-m3-int8,
    bge-small-en-v1.5, paraphrase-multilingual-MiniLM-L12-v2). НЕ менять содержимое.
    Убедиться, что `paths.onnx_config` по умолчанию (`configs/onnx.yaml`) резолвится
    относительно CWD/exe при запуске бинаря из repo root.
  - Тесты: `cargo test -p config` зелёный; `synopsis onnx-runtime status --config configs/config.default.yaml`
    (после 1.3) видит платформы.
  - Критерии приёмки: `configs/onnx.yaml` существует и совпадает со структурой оракула;
    `cargo test -p config` зелёный; бинарь без `--config` находит файл (проверка
    `onnx-runtime status` не падает с "read config file configs/onnx.yaml: No such file").

- [x] 1.3 Поставить `configs/config.default.yaml` (порт + default model=bge-m3-int8)
  - Цель: бинарь работает с пресетом default без `--config`.
  - Scope файлов: `configs/config.default.yaml` (новый, repo root).
  - Содержание: портировать `../synopsis/configs/config.default.yaml`, НО установить
    `embeddings.local.model_name: "bge-m3-int8"` и `vector_dim: 1024` (Rust-default по
    frozen stack; оракул использует bge-small-en-v1.5 — сознательное отклонение,
    зафиксировано в design.md D3). Остальные секции (database, ingestion, ner, linker,
    search, graph, auto_update, scheduler, logging, paths, server) — верны структуре
    оракула; `paths.data_dir: "data"`, `paths.onnx_config: "configs/onnx.yaml"`,
    `paths.global_config_path: "data/ontology"`, `paths.prompts_path: "configs/prompts"`.
  - Тесты: `cargo test -p config` зелёный.
  - Критерии приёмки: `synopsis --version` и `synopsis onnx-runtime status --config configs/config.default.yaml`
    работают без абсолютных путей; `cargo test -p config` зелёный; default model=bge-m3-int8.

- [x] 1.4 Поставить `configs/prompts/*` (minijinja) + README provenance
  - Цель: `prompts_path` по умолчанию `configs/prompts` резолвится; шаблоны — minijinja.
  - Scope файлов: `configs/prompts/entity-linker/{system,user}.tmpl` (из
    `crates/graph/src/templates/entity-linker/*.tmpl` после 1.1),
    `configs/prompts/ner/{system,user}.tmpl` (из `crates/ingestion/src/ner/templates/*.tmpl`),
    `configs/README.md` (provenance).
  - Содержание: скопировать встроенные minijinja-шаблоны graph и NER в
    `configs/prompts/{entity-linker,ner}/`. НЕ копировать Go text/template версии
    (они не парсятся minijinja). `configs/README.md`: таблица — `onnx.yaml`,
    `config.default.yaml` = порт оракула (config.default.yaml model_name отклонён на
    bge-m3-int8); `prompts/**` = Rust-minijinja переписаны (причина: движок minijinja,
    не Go text/template).
  - Тесты: `cargo test -p graph` и `cargo test -p ingestion` зелёные (override-загрузка
    из `configs/prompts` работает).
  - Критерии приёмки: `configs/prompts/entity-linker/{system,user}.tmpl` и
    `configs/prompts/ner/{system,user}.tmpl` существуют и являются валидными minijinja
    (совпадают со встроенными); `configs/README.md` есть; `cargo test -p graph -p ingestion` зелёный.
