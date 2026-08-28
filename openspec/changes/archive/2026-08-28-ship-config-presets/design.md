# Design: ship-config-presets

## D1 — Config resolution expects repo-root `configs/`
`crates/cli/src/config_resolver.rs` ищет `configs/config.{preset}.yaml` относительно
exe/CWD. Сейчас бинарь падает без `--config`, т.к. файлов нет в repo root (есть только
test-фикстуры в `crates/config/tests/data/`). Поставка `configs/` в repo root делает
бинарь рабочим «из коробки».

## D2 — `onnx.yaml` port verbatim
Копируется побайтово из `../synopsis/configs/onnx.yaml`: `runtime.platforms[*]`
(linux-amd64 → `libonnxruntime.so.1.28.0` и т.д.) + `models.entries[*]`
(bge-m3-int8, bge-small-en-v1.5, paraphrase-multilingual-MiniLM-L12-v2). Чтение через
`crates/config/src/onnx.rs::load_onnx_config` (без изменений).

## D3 — `config.default.yaml`: model_name=bge-m3-int8 (отклонение)
Оракул по умолчанию использует `bge-small-en-v1.5` (dim=384). Rust-default по frozen
stack — `bge-m3-int8` (dim=1024, см. AGENTS.md «frozen stack»). Сознательное отклонение:
`embeddings.local.model_name: "bge-m3-int8"`, `vector_dim: 1024`. Остальные секции —
верны структуре оракула. `paths.onnx_config: "configs/onnx.yaml"`,
`paths.prompts_path: "configs/prompts"`, `paths.data_dir: "data"`.

## D4 — Prompts: minijinja, НЕ Go text/template
`configs/prompts/**` — Rust-minijinja переписаны (движок minijinja, не Go text/template).
Источник: `crates/graph/src/templates/entity-linker/*.tmpl` (после 1.1) и
`crates/ingestion/src/ner/templates/*.tmpl`. Go-версии НЕ копируются (не парсятся
minijinja). `configs/README.md` фиксирует provenance.

## D5 — Graph embedded-prompt location fix (корректировка 2026-08-28)
`crates/graph/src/prompts.rs` грузит встроенные промпты через
`include_str!("../fixtures/templates/entity-linker/*.tmpl")` — семантически «тестовые
фикстуры». Перенести шаблоны в `crates/graph/src/templates/entity-linker/*.tmpl` и
использовать `include_str!("templates/entity-linker/*.tmpl")`. Путь `include_str!`
резолвится относительно файла `crates/graph/src/prompts.rs` (т.е. относительно
`crates/graph/src/`), поэтому `templates/...` → `crates/graph/src/templates/...`.
`prompts.rs` остаётся плоским файлом — НЕ переименовывать в `prompts/mod.rs` и НЕ создавать
`src/prompts/templates/` (корректировка пользователя: rename избыточен). Тесты в prompts.rs
уже ссылаются на встроенные промпты (`/nonexistent/prompts-path` + temp-overrides) — они НЕ
ссылаются на fixture-файлы; после переноса `grep "fixtures/templates" crates/graph/` пуст.

## Oracle references
- `../synopsis/configs/onnx.yaml`
- `../synopsis/configs/config.default.yaml`
- `../synopsis/configs/prompts/entity-linker/*.tmpl`
- `../synopsis/configs/prompts/ner/*.tmpl`

## Non-goals
- Не менять семантику замороженного контракта `configs/*.yaml` (flags/defaults верны).
- Не портировать Go `text/template` промпты verbatim.
