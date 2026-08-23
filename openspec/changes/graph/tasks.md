# graph — Tasks

Порядок = граф зависимостей: 1.1 → {1.2, 1.6} → {1.3, 1.4, 1.5, 1.7} → 1.8 → 1.9. Каждая задача выполняется СВЕЖИМ агентом без памяти предыдущих — тело самодостаточно. Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс / история ревизий). **Принцип миграции (binding):** НЕ транскрибировать Go 1:1 — функциональная копия; архитектурно правильно для Rust (DRY, KISS, SOLID, YAGNI); внутренняя совместимость с оракулом не требуется; баги Go исправлять или фиксировать осознанные отклонения. CI без сети.

Общие факты для всех задач (источники истины): design этого change'а D1–D8; замороженная схема v5 (таблицы `entities`, `entity_links` — DAO уже в крейте db: EntityLinkDao с self-link rejection и идемпотентной записью, EntityDao, FactDao); config-крейт: `GraphConfig { enable_graph, max_depth, max_nodes, load_on_startup }` (presence-семантика D13 готова, apply_defaults заполняет границы: depth default 5/max 10, nodes default 1000), `LinkerConfig { disabled, llm }`. Контракт траверсала (traverser.go): Direction {outgoing, incoming, both} default both; fact-рёбра НИКОГДА не пересекают домены; пересечение только через entity links при FollowEntityLinks=true; len(edges) ≤ len(nodes). CEL-функции контракта: facts(e), has_fact(e,k,v), chunks(e), chunk_contains(e,t), neighbors(e), path_exists(from,to,max_depth). Гейты каждой задачи: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -p graph`, `cargo test --workspace`; missing_docs=deny, unsafe_code=forbid; пин 1.96.0; Cargo.lock коммитится. Untracked `.idea/`, `.opencode/opencode.json`, `.opencode/plans/`, skills-lock.json не трогать.

---

- [x] 1.1 Feasibility-гейт (petgraph + cel) + скаффолдинг крейта
  - **Цель:** снять риски двух новых зависимостей до написания кода графа и создать каркас крейта.
  - **Scope файлов:** `crates/graph/Cargo.toml`, `crates/graph/src/lib.rs` (+`error.rs` по конвенции workspace), корневой `Cargo.toml`, `Cargo.lock`.
  - **Детали:** workspace-пины `petgraph = "0.8"` и `cel = "0.14"` (замена cel-interpreter — frozen-stack решение человека 2026-08-22, design D2). Feasibility: (а) host-сборка (MSRV vs 1.96.0); (б) `cargo zigbuild --release --target x86_64-pc-windows-gnu` и `--target aarch64-apple-darwin` (обе зависимости pure Rust — ожидается PASS; darwin-стаб уже вендорен в ci/darwin-sdk); (в) верификация API cel по исходникам registry (`~/.cargo/registry/src/*/cel-0.14*/`): регистрация кастомных функций (add_function/FunctionContext), типы значений (Value), разбор выражений — НЕ угадывать. Каркас: `GraphError` (thiserror, варианты уточнить по задачам), module docs с намерением крейта (design D1).
  - **Критерии приёмки:** все гейты зелёные; feasibility (а)(б)(в) выполнены и записаны в отчёте; минимальный smoke-тест: парсинг и оценка тривиального CEL-выражения + создание пустого DiGraph; при провале (а)/(б)/(в) — СТОП и эскалация.
  - **Зависимости:** нет.
  - **Референс:** design D1/D2; паттерн feasibility — archive vectors задача 1.1; `ci/darwin-sdk/README.md`.

- [x] 1.2 Построитель индекса: SQLite → petgraph DiGraph
  - **Цель:** Graph::from_db — загрузка entities/entity_links в DiGraph + индексы name→ID («domain:lowercase_name», O(1)) и type→nodes.
  - **Scope файлов:** `crates/graph/src/graph.rs` (или lib.rs при малом объёме), тесты.
  - **Детали:** узлы — сущности (вес: id, domain, name, type); рёбра — entity_links (тип связи в весе ребра) + fact-рёбра из фактов сущностей (пометить как fact-рёбра для доменных границ D4 — сверить с оракулом, как именно fact-связи попадают в граф: traverser.go различает их от entity links). Полный rebuild за один проход (D3). Семантика флагов: enable_graph=false / load_on_startup=false → индекс не строится, состояние «граф недоступен» (не ошибка). Пустая БД → пустой валидный индекс.
  - **Критерии приёмки:** юнит-тесты на in-memory SQLite (крейт db): загрузка N сущностей/M линков → счётчики узлов/рёбер точны; индексы name→ID/type→nodes корректны; пустая БД; повторный from_db даёт эквивалентный индекс; гейты зелёные.
  - **Зависимости:** 1.1.
  - **Референс:** design D1/D3; internal/graph/graph.go (структура индексов — семантика); db-крейт DAO (EntityDao, EntityLinkDao).

- [x] 1.3 Поиск сущностей: exact + partial
  - **Цель:** FindEntityExact (O(1), регистронезависимо, по домену+имени) и FindEntityPartial (prefix + substring в домене, результат сортирован).
  - **Scope файлов:** модуль графа из 1.2 (+тесты).
  - **Детали:** exact — через name→ID индекс (ключ «domain:lowercase_name»); partial — линейный скан по домену с нормализацией регистра (масштабы личного корпуса это позволяют; задокументировать O(n)).
  - **Критерии приёмки:** тесты: exact находит в любом регистре; miss → None/пусто; partial по префиксу и подстроке, сортировка результата; кросс-доменное имя не находится по чужому домену; гейты зелёные.
  - **Зависимости:** 1.2.
  - **Референс:** internal/graph/entity_finder.go (семантика match).

- [x] 1.4 BFS-траверсал с доменными границами
  - **Цель:** traverse(entity_id, opts) — контракт оракула дословно.
  - **Scope файлов:** модуль траверсала (+тесты).
  - **Детали:** собственный BFS поверх DiGraph (не готовый визитер — нужен контроль границ/лимитов): max_depth (default 5, hard max 10), max_nodes (default 1000, рёбра только между вошедшими узлами), Direction {Outgoing, Incoming, Both} default Both. Доменные границы: fact-ребро в чужой домен блокируется ВСЕГДА; entity-link ребро в чужой домен — только при FollowEntityLinks=true. Результат детерминирован (порядок расширения зафиксировать и задокументировать).
  - **Критерии приёмки:** тесты на графе с двумя доменами: fact-граница блокируется при обоих значениях флага; entity-link переход разрешён/запрещён по флагу; depth/node лимиты уважаются (len(edges) ≤ len(nodes)); все три направления дают корректную смежность; детерминизм — два прогона идентичны; гейты зелёные.
  - **Зависимости:** 1.2.
  - **Референс:** design D4; internal/graph/traverser.go (Options, Normalize, семантика MaxNodes/FollowEntityLinks).

- [ ] 1.5 Метрики + DOT-export
  - **Цель:** stats (node_count, edge_count, avg_degree) и to_dot() (валидный graphviz, атрибуция узлов доменом/типом).
  - **Scope файлов:** модуль метрик (+тесты).
  - **Детали:** DOT — средствами petgraph (Dot-эскейп имён); avg_degree = 2*edges/nodes (сверить с metrics.go, edge-семантика направленных рёбер).
  - **Критерии приёмки:** тесты: счётчики соответствуют построенному индексу; DOT непустой и парсится (минимальная проверка структуры: заголовок digraph, экранирование спецсимволов в именах); гейты зелёные.
  - **Зависимости:** 1.2.
  - **Референс:** internal/graph/metrics.go.

- [ ] 1.6 CEL-движок: обёртка над cel + scope cache
  - **Цель:** CelEngine — компиляция/оценка выражений, фреймворк регистрации кастомных функций, ленивые индексы (FactIndex/ChunkIndex/GraphIndex) со scope cache.
  - **Scope файлов:** `crates/graph/src/cel.rs` (+тесты).
  - **Детали:** API cel 0.14 верифицирован в 1.1 — использовать фактически существующие точки расширения (add_function/FunctionContext или актуальные). Scope cache: тяжёлые индексы строятся при первом обращении функции и живут до конца оценки набора правил (паттерн scope_cache.go). Доступ к данным — через db-DAO (sync).
  - **Критерии приёмки:** тесты: тривиальные выражения оцениваются; незнакомая функция → явная ошибка (не паника); ленивость — индекс не строится, если функция не вызвана (счётчик/флаг в тестовом двойнике); гейты зелёные.
  - **Зависимости:** 1.1.
  - **Референс:** design D5; internal/expression/{engine.go,scope_cache.go}.

- [ ] 1.7 CEL-функции данных: facts, has_fact, chunks, chunk_contains
  - **Цель:** четыре SQLite-backed функции контракта.
  - **Scope файлов:** cel.rs (+тесты).
  - **Детали:** facts(e) → список фактов сущности; has_fact(e,k,v) → bool; chunks(e) → чанки сущности; chunk_contains(e,text) → bool (подстрочный поиск по текстам чанков сущности — сверить семантику с оракулом: FTS или LIKE). Типы аргументов/возврата — маппинг cel::Value ↔ наши данные зафиксировать и задокументировать.
  - **Критерии приёмки:** тесты на фиксированной БД: каждая функция возвращает ожидаемое; отсутствующая сущность → согласованный результат (пусто/false, не ошибка — сверить с оракулом); гейты зелёные.
  - **Зависимости:** 1.6.
  - **Референс:** internal/expression/engine.go (функции), internal/relations/scope_builders.go (FactIndex/ChunkIndex).

- [ ] 1.8 CEL-графовые функции: neighbors, path_exists
  - **Цель:** две функции поверх in-memory индекса.
  - **Scope файлов:** cel.rs (+тесты).
  - **Детали:** neighbors(e) → смежные ID (оба направления; сверить с оракулом); path_exists(from,to,max_depth) → bool достижимости с ограничением глубины (лёгкий BFS; доменные границы — те же правила D4, сверить с оракулом). GraphIndex-слой лениво через scope cache.
  - **Критерии приёмки:** тесты: neighbors корректен для обоих направлений; path_exists true/false на связных/несвязных парах; max_depth уважается; гейты зелёные.
  - **Зависимости:** 1.4, 1.6.
  - **Референс:** internal/expression/engine.go; internal/graph/traverser.go (для семантики достижимости).

- [ ] 1.9 Линкеры: expression + кросс-доменный пайплайн (equals, llm-стаб)
  - **Цель:** оценка CEL-правил онтологии → линки; пайплайн методов equals → expression → llm(стаб); идемпотентная запись через EntityLinkDao.
  - **Scope файлов:** `crates/graph/src/linker.rs` (+тесты), интеграционный тест.
  - **Детали:** equals — нормализованное совпадение имён между разными доменами; expression — правила из ontology.xml (уже парсятся в config-крейте): приоритет/тип правила → атрибуты линка; порядок методов — из конфигурации онтологии; llm — стаб (логирует пропуск, ничего не пишет), linker.disabled исключает метод. Идемпотентность: повторный прогон не создаёт дубликатов. Интеграционный тест end-to-end: БД с двумя доменами + онтология с equals-парой и одним CEL-правилом → ожидаемые линки, повторный прогон идемпотентен.
  - **Критерии приёмки:** тесты: equals создаёт линк для совпадающих имён разных доменов и не для разных; CEL-правило истинно → линк с атрибутами правила; llm-стаб ничего не пишет; disabled исключает метод; идемпотентность второго прогона; self-link не создаётся (DAO); гейты зелёные.
  - **Зависимости:** 1.7, 1.8.
  - **Референс:** design D6; internal/relations/{cross_domain_linker.go, expression_linker.go, llm_linker.go, mock_linker.go}; ontology-парсер config-крейта.
