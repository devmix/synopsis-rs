# graph — Design

Реализация knowledge graph: hybrid-хранилище (решение человека 2026-08-22) — SQLite источник истины, petgraph — производный in-memory индекс; CEL через крейт `cel` (замена cel-interpreter, там же). Оракул (`../synopsis/internal/graph`, `relations`, `expression`) — референс по поведению и контрактам, не по коду.

## D1. Hybrid: SQLite (истина) + petgraph (индекс)

**Решение:** таблицы `entities`/`entity_links` (замороженная схема v5, DAO в крейте db) остаются единственным хранилищем. Крейт graph строит при старте `petgraph::DiGraph` + индексы `HashMap<(domain, lowercase_name), NodeId>` и `HashMap<type, Vec<NodeId>>`. Запись — только через db-DAO; индекс — read-only производный.
**Почему не альтернатива:** embedded graph DB исключены исследованием (kuzu архивирован окт 2025; sqlitegraph GPL-3.0; остальные alpha <1K загрузок); SQLite-only на recursive CTE — многословный BFS с доменными границами и неудобная интеграция CEL-функций. petgraph: pure Rust, 0 нативных deps → zigbuild-гейт тривиален; 475M+ загрузок; DOT/BFS из коробки.
**Масштаб:** личный корпус — тысячи…сотни тысяч сущностей; индекс ≈ единицы МБ RAM.
**Референс:** internal/graph/graph.go (структура индексов — семантика сохранена, реализация идиоматичная).

## D2. cel-interpreter → cel (frozen stack, решение человека 2026-08-22)

**Решение:** workspace-пин `cel = "0.14"` вместо устаревшего cel-interpreter 0.10. Кастомные функции регистрируются через `add_function()`; типы значений — cel::Value.
**Почему:** cel-interpreter устарел (~531K загрузок против 1M+, активность прекращена); cel активен (обновлён авг 2026) и поддерживает регистрацию функций контракта. antlr4rust (зависимость парсера cel) — pure Rust, но включается в feasibility-гейт задачи 1.1 (урок lancedb/CoreFoundation).
**Риск/митигация:** тонкости API верифицируются по исходникам registry до написания кода; при блокере — эскалация (fallback: остаться на cel-interpreter и мигрировать позже).

## D3. Полный rebuild при старте; инкрементальность YAGNI

**Решение:** `load_on_startup=true` (GraphConfig, presence-семантика D13 уже в config-крейте) → полный rebuild индекса за один проход по entity_links/entities. Инкрементальный rebuild (по timestamp) не делается: на масштабе личного корпуса rebuild — секунды.
**Почему не альтернатива:** инкрементальность оракула (rebuild changed since) — сложность ради экономии миллисекунд; YAGNI. Вернуться можно без ломки контракта (внутренняя деталь).
**Референс:** config preset GraphConfig (load_on_startup, enable_graph); oracle NewGraphFromDB.

## D4. Доменные границы траверсала — контракт оракула дословно

**Решение:** fact-рёбра никогда не пересекают домены (безусловное правило); пересечение возможно только через entity-link рёбра и только при `FollowEntityLinks=true`. Параметры: max_depth default 5 / hard max 10; max_nodes default 1000, рёбра включаются только между вошедшими узлами (len(edges) ≤ len(nodes)); Direction {Outgoing, Incoming, Both} default Both. Реализация — собственный BFS поверх petgraph-графа с фильтром на шаге расширения (не готовый Bfs-визитер: нужен контроль доменов и лимитов).
**Референс:** internal/graph/traverser.go (Options/Normlize, семантика MaxNodes).

## D5. CEL-функции и scope cache

**Решение:** шесть функций контракта: `facts(e)`, `has_fact(e,k,v)`, `chunks(e)`, `chunk_contains(e,text)`, `neighbors(e)`, `path_exists(from,to,max_depth)`. Тяжёлые индексы (FactIndex — факты по сущностям; ChunkIndex — чанки/тексты; GraphIndex — слои достижимости для path_exists) строятся лениво при первом обращении и кэшируются в scope на время оценки правила (паттерн scope_cache.go). Доступ к SQLite — через уже открытый пул/коннект крейта db (sync, вызывающий код сам решает про spawn_blocking).
**Референс:** internal/expression/{engine.go,scope_cache.go}; internal/relations/expression_linker.go.

## D6. Кросс-доменный пайплайн: equals → expression → llm(стаб)

**Решение:** методы применяются в порядке конфигурации онтологии; каждый идемпотентен (ON CONFLICT DO NOTHING через EntityLinkDao — self-link rejection уже в DAO). `equals` — нормализованное совпадение имён между доменами. `expression` — оценка CEL-правил онтологии (приоритет/тип правила → атрибуты линка). `llm` — стаб: логирует пропуск, ничего не пишет; флаг `linker.disabled` исключает метод. Реальная LLM-линковка вернётся отдельным change'ом вместе с LLM-клиентом (решение человека 2026-08-21).
**Референс:** internal/relations/{cross_domain_linker.go, expression_linker.go, llm_linker.go, scope_builders.go}.

## D7. Concurrency

**Решение:** индекс за `Arc<RwLock<Graph>>`: построение — write-лок один раз при старте; MCP-обработчики читают параллельно (read-локи). CEL-оценка читает индекс и SQLite без блоков записи. Внутренние структуры индекса после построения неизменяемы (v1 без инкрементальности — D3), что делает read-путь lock-friendly.

## D8. SPARQL — отложено (решение человека 2026-08-22)

Отдельный будущий change: проекция SQLite → RDF-тройки → oxigraph in-memory Store (pure Rust backend, без RocksDB). Здесь не делается: нет потребителя в замороженном mcp-contract; GQL/Cypher сегодня без жизнеспособных embedded-реализаций (kuzu архивирован, остальные alpha).

## Отклонения от оракула (осознанные)

- 8+ самописных map → petgraph DiGraph + два HashMap-индекса (семантика поиска сохранена).
- cel-interpreter → cel (frozen-stack решение D2).
- Инкрементальный rebuild оракула → полный rebuild (D3, YAGNI).
- LLM-linker → стаб (D6, решение 2026-08-21).
