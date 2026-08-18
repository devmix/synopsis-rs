# Proposal — scaffold-rust-project

## Why

Synopsis переписывается на Rust (решение принято: боль — латентность vec0 brute-force при N до 1M × 1024-dim на ноутбуке; CGO-сборка Go оригинала — постоянный налог; реализацию пишет ИИ, человек ревьюит контракты). До того как ИИ начнёт массово писать модули, нужен фундамент: workspace, CI и **замороженные внешние контракты** — против них каждая последующая задача проверяется независимо. Без этого паритетные проверки не имеют эталона, а ревью превращается в чтение кода без критериев.

## What Changes

- Новый Rust workspace `synopsis-rs`: корневой Cargo.toml (workspace members), каталоги по доменным crate'ам (layout зафиксирован в design.md)
- CI: clippy + fmt + test (linux), проверка кросс-компиляции под целевую матрицу (linux/darwin/windows × amd64/arm64)
- Четыре контрактные спеки, фиксирующие поведение Go оригинала (`../synopsis`) как есть — источник истины для всех паритетных проверок:
  - `mcp-contract` — 12 MCP tools: имена, JSON-схемы параметров/ответов, транспорт (MCP Streamable HTTP; legacy SSE оракула намеренно не сохраняется — design D8), `/health`
  - `cli-surface` — подкоманды serve/sync/model/onnx-runtime, глобальные и per-command флаги, порядок аргументов
  - `data-schema` — SQLite-схема из 5 миграций Go оригинала: таблицы, индексы, constraints; правило «shipped-миграции не редактируются»
  - `config-format` — форматы config presets (YAML), onnx.yaml (реестр моделей + рантайм), ontology XML (`global.xml`, `domains/*.xml`)
- README + AGENTS.md нового репо: стек, команды сборки/тестов, правила паритета
- **НЕ входит** в этот change (non-goals ниже): какое-либо прикладное поведение, спайки нативных швов, фикстура-экспортер

## Capabilities

### New Capabilities

- `mcp-contract`: внешний контракт MCP API — инструменты и схемы (эталон паритета — поведение Go оригинала), транспорт Streamable HTTP через официальный SDK rmcp (design D8)
- `cli-surface`: внешний контракт CLI — подкоманды, флаги, порядок аргументов, коды поведения (exit codes)
- `data-schema`: схема данных SQLite и правила миграций; гарантирует читаемость старого knowledge.db бинарью Rust
- `config-format`: форматы конфигурационных файлов (presets YAML, onnx.yaml, ontology XML); гарантирует совместимость с существующими configs/

### Modified Capabilities

(пусто — репо новый, изменённых спеки нет)

## Impact

- Новый репозиторий `../synopsis-rs` (git init уже выполнен); Go оригинал не затрагивается
- Зависимости: только dev-time toolchain (rustfmt, clippy, cargo-zigbuild/cross для CI) — runtime-зависимости появятся в спайках/модулях
- Последующие change'ы (`native-seam-spikes`, модульные) ссылаются на спеки этого change как на эталон паритета
