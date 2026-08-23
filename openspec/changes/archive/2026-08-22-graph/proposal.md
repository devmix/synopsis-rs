# graph — Proposal

## Why

Крейт `graph` — knowledge-graph хранилище и линкеры (layout AGENTS.md: `internal/graph` + `internal/relations` + `internal/expression`). Без него MCP-инструменты графа (`get_entity_relations`, `get_entity_dossier`, статистика каталога) и кросс-доменная линковка невозможны. Оракул использует кастомное in-memory решение (8+ map в RAM, производное от SQLite); по решению человека (2026-08-22) хранилище выбирается заново — **hybrid**: SQLite остаётся источником истины (таблицы `entities`/`entity_links` из замороженной схемы v5), поверх — идиоматичный in-memory индекс на petgraph вместо самописных map. Это первый потребитель read-side крейтов db/config и фундамент для search/mcp.

## What Changes

- Новый крейт `crates/graph`: построение графа из SQLite (petgraph DiGraph + индексы name→ID, type→nodes), BFS-траверсал с контрактом оракула (max_depth default 5/max 10, max_nodes default 1000, direction outgoing/incoming/both), строгие доменные границы (fact-рёбра не пересекают домены; переход только через entity links при `FollowEntityLinks=true`), entity finder (exact O(1) / partial), метрики + DOT-export.
- CEL-движок: **замена cel-interpreter → cel** (frozen-stack решение человека 2026-08-22: cel-interpreter устарел, cel 0.14.x активен и поддерживает `add_function()` для кастомных функций) + 6 функций контракта: `facts()`, `has_fact()`, `chunks()`, `chunk_contains()`, `neighbors()`, `path_exists()` + scope cache (lazy FactIndex/ChunkIndex/GraphIndex).
- Кросс-доменный линкер: пайплайн методов `equals` → `expression` (CEL-правила из ontology.xml) → `llm`; LLM-linker — **стаб** (решение человека 2026-08-21, отложено). Идемпотентная запись линков через существующий EntityLinkDao.
- Полный rebuild индекса при старте (`load_on_startup`); инкрементальный rebuild — YAGNI для v1 (решение 2026-08-22).

**BREAKING (frozen stack, одобрено человеком 2026-08-22):** зависимость `cel-interpreter` заменена на `cel`; добавлен `petgraph` (pure Rust, 0 нативных deps). Оба — расширения frozen stack с явным решением; feasibility-гейт zigbuild — первая задача.

## Capabilities

### New Capabilities
- `knowledge-graph`: контракт графа знаний — построение/загрузка индекса, BFS-траверсал с доменными границами, поиск сущностей, метрики/DOT, CEL-линковка выражениями, кросс-доменный пайплайн.

### Modified Capabilities

(нет — data-schema/mcp-contract/config-format не меняются; GraphConfig/LinkerConfig уже существуют в config-крейте)

## Impact

- **Код:** новый `crates/graph` (зависимости: config, db — tier-2 D1); корневой `Cargo.toml` (workspace-пины petgraph, cel; удаление cel-interpreter если где-то числился).
- **Замороженные контракты:** не меняются. Паритет — дифференциально: те же BFS-запросы к обоим бинарям дают идентичные результаты (узлы/рёбра/порядок), CEL-правила ontology.xml дают те же линки.
- **Потребители (будущие change'и):** `search` (расширение результатов), `mcp` (get_entity_relations/get_entity_dossier/stats), ingestion (запись сущностей уже через db).
- **Отложено:** SPARQL/oxigraph проекция — отдельный change (решение 2026-08-22); LLM-linker — после решения по LLM-клиенту.

## Non-goals

- SPARQL/GQL/Cypher поддержка (отложено; GQL/Cypher сегодня без жизнеспособных embedded-реализаций, SPARQL — отдельный change через oxigraph-проекцию).
- LLM-linker (стаб вместо реализации; вернётся с LLM-решением).
- Инкрементальный rebuild индекса (полного на наших масштабах достаточно).
- MCP-инструменты поверх графа (change mcp), запись сущностей из ингестии (change ingestion).
- Персистентность самого индекса (SQLite — единственная истина; индекс производный).
