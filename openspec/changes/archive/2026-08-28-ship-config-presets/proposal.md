# Proposal: ship-config-presets

## Change name
`ship-config-presets`

## Why
Бинарь `synopsis` собирается и работает, но находит конфиги только через test-фикстуры
(`crates/config/tests/data/`). Для реального деплоя нужны поставляемые пресеты в
repo-root `configs/`. Дополнительно (корректировка пользователя 2026-08-28):
`crates/graph/src/prompts.rs` ссылается на промпты из `fixtures/templates`
(семантически «тестовые фикстуры») — надо сделать как в
`crates/ingestion/src/ner/prompts.rs` (встроенные промпты в `templates/`, тесты
ссылаются на встроенные).

## What changes
1. `configs/onnx.yaml` — порт оракула (verbatim): runtime-платформы + реестр моделей.
2. `configs/config.default.yaml` — порт оракула, default `model_name=bge-m3-int8`
   (1024-dim, сознательное отклонение от оракула, задокументировано).
3. `configs/prompts/{entity-linker,ner}/*.tmpl` — Rust-minijinja шаблоны (НЕ Go verbatim).
4. `crates/graph/src/prompts.rs` — перенос встроенных промптов в `src/prompts/templates/`.

## Non-goals
- Не менять семантику замороженного контракта `configs/*.yaml` (flags/defaults верны).
- Не портировать Go `text/template` промпты verbatim (minijinja — другой движок).

## Deviations
- `config.default.yaml` `model_name=bge-m3-int8` (оракул: `bge-small-en-v1.5`) — по frozen stack.
- `prompts/**` — minijinja (оракул: Go `text/template`) — движок отличается, переписано функционально.
