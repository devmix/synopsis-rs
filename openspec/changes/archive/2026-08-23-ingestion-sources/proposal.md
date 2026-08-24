# ingestion-sources — Proposal

## Why

Ingestion — конвейер превращения документов в знания; его основа — разбор источников на документы и чанки. По решению человека (2026-08-23) модуль мигрирует серией из трёх change'ов; этот — первый: источники, парсеры и чанкеры (трейты + 5 форматов). Без него NER (change 2) и пайплайн-оркестратор (change 3) не имеют входа. Оракул: `internal/ingestion/{sources,parsers,chunkers}` (~3.6K строк с тестами).

## What Changes

- Новый крейт `crates/ingestion` (tier-2 D1): трейты `Parser` (обход пути → документы), `Chunker` (контент → чанки со смещениями), `Source` (композит Parser+Chunker — «самодостаточная единица ингестии» одного типа).
- Парсеры и чанкеры пяти форматов: **markdown** (structure-aware: секции заголовков, max_chunk_size/overlap из конфига) и **json** первыми; затем **mediawiki**, **webpage**, **unstructured** — по тому же паттерну (решение человека 2026-08-23).
- Реестр источников: определение типа по конфигурации global.xml (`<source type=…>`)/расширениям файлов.
- Чанк несёт text/sequence_num/start-end offsets/metadata — БЕЗ NerResult (осознанное отклонение: NER-результат присоединяется на этапе NER в change 2, а не живёт в структуре чанка).
- Паритет-фикстуры из testdata оракула для дифференциальных тестов.

**BREAKING:** нет. Новая зависимость крейта ingestion от config/db только.

## Capabilities

### New Capabilities
- `parsing-and-chunking`: контракт разбора документов — обнаружение файлов, извлечение контента 5 форматов, structure-aware чанкинг с офсетами, реестр источников.

### Modified Capabilities

(нет)

## Impact

- **Код:** `crates/ingestion` (зависимости config; db понадобится change'ам 2–3); корневой Cargo.toml при необходимости.
- **Замороженные контракты:** config-format не меняется (ChunkingConfig существует); data-schema не затрагивается (чанки пишутся в БД в change 3). Паритет — дифференциально против фикстур оракула (число чанков, текст, границы офсетов).
- **Потребители:** ingestion-ner (change 2), ingestion-pipeline (change 3).

## Non-goals

- NER любых методов (change 2), резолвер сущностей (change 2).
- Ингестер/Runner/каскад удаления/прогресс (change 3).
- Запись чанков в SQLite (change 3); watcher/scheduler (отдельные будущие change'и).
- Эмбеддинги (вызов embedding-крейта — change 3).
