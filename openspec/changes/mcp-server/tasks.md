# Tasks: mcp-server

Зависимости: search (архив), db DAOs, graph, config — все готовы. Замороженный
контракт: `openspec/specs/mcp-contract/spec.md` — РЕАЛИЗУЕТСЯ, не меняется.
Оракул (read-only): `../synopsis/internal/mcp/{tools.go,server.go}`,
`../synopsis/internal/mcp/handlers/*.go` + тесты.
Дизайн: design.md D1–D7. Гейты каждой задачи: `cargo fmt --check`,
`cargo clippy -p mcp --all-targets -- -D warnings`, `cargo test -p mcp`;
workspace-тест и cargo doc гоняет ТОЛЬКО оркестратор.
Директива (binding): НЕ копировать Go 1:1; DRY/KISS/SOLID/YAGNI; баги оракула
исправлять или фиксировать отклонения. Транспорт: rmcp 3.x Streamable HTTP
(заморожено D8), legacy SSE НЕ портировать.

- [x] 5.1 Каркас сервера: rmcp + axum + /health + реестр инструментов
  - Цель: транспорт и регистрация.
  - Scope файлов: `crates/mcp/Cargo.toml` (+rmcp, axum, tokio и пр. по необходимости),
    `crates/mcp/src/lib.rs`, `crates/mcp/src/server.rs`, `crates/mcp/src/health.rs`,
    `crates/mcp/src/error.rs` (новые).
  - Содержание: design D1/D5/D7 — сборка axum-роутера с rmcp Streamable HTTP
    сервисом + GET /health (status/version/counters, форма как у Go); реестр 12
    инструментов (схемы параметров из tools.go — макросом rmcp если выражает точно,
    иначе ручные Tool-объекты); McpError (thiserror); инъекция коллабораторов
    (Db, Searcher, Option<Graph>).
  - Тесты: schema-тест tools/list против замороженного контракта (ровно 12 имён);
    /health форма (ключи/типы); ошибки → MCP tool error.
  - Критерии приёмки: гейты mcp зелёные.

- [x] 5.2 Курсорная пагинация
  - Цель: общий хелпер пагинации.
  - Scope файлов: `crates/mcp/src/pagination.rs` (новый).
  - Содержание: порт semantics pagination.go — opaque cursor (base64 последнего
    ключа сортировки), клампинг limit, next_cursor только при наличии строк.
  - Тесты: roundtrip курсора, границы limit, отсутствие next_cursor на последней
    странице; паритет кейсам оракула где применимо.
  - Критерии приёмки: гейты mcp зелёные.

- [x] 5.3 Инструмент search
  - Цель: главный инструмент.
  - Scope файлов: `crates/mcp/src/tools/search.rs` (новый), регистрация в server.rs.
  - Содержание: design D4 — аргументы по замороженной схеме → Searcher (hybrid;
    режим lexical/semantic если схема exposes) → ответ в форме оракула
    (handlers/search.go). Approved-only факты — не ослаблять.
  - Тесты: паритет кейсам `../synopsis/internal/mcp/handlers/search_test.go`
    (in-memory SQLite + стабы Searcher где нужно): happy path, пустой результат,
    невалидные аргументы, домен.
  - Критерии приёмки: гейты mcp зелёные.

- [x] 5.4 catalog_overview + catalog_documents
  - Цель: обзор и список документов.
  - Scope файлов: `crates/mcp/src/tools/catalog.rs` (новый), регистрация.
  - Содержание: overview — счётчики documents/chunks/entities/facts (+прочее по
    схеме оракула); documents — DocumentDao list_paginated + курсор (5.2).
  - Тесты: паритет catalog_overview_test.go / catalog_documents_test.go кейсов:
    счётчики на засеянной БД, пагинация (первая/средняя/последняя страница),
    пустая БД.
  - Критерии приёмки: гейты mcp зелёные.

- [x] 5.5 catalog_entities + search_entities_by_type
  - Цель: списки сущностей.
  - Scope файлов: `crates/mcp/src/tools/entities_catalog.rs` (новый), регистрация.
  - Содержание: entities — EntityDao пагинированный список; by_type — фильтр типа
    + пагинация; формы ответов из handlers/catalog_entities.go /
    search_entities_by_type.go.
  - Тесты: паритет соответствующим *_test.go: фильтр типа, пагинация, пустой тип.
  - Критерии приёмки: гейты mcp зелёные.

- [ ] 5.6 search_facts + get_fact_by_id
  - Цель: факты.
  - Scope файлов: `crates/mcp/src/tools/facts.rs` (новый), регистрация.
  - Содержание: search_facts — фильтры по схеме + пагинация (approved-only);
    get_fact_by_id — факт + сущности + источники; not-found → tool error как у
    оракула.
  - Тесты: паритет search_facts_test.go / get_fact_by_id_test.go: фильтры,
    approved-only (pending не просачивается), not-found, sources shape.
  - Критерии приёмки: гейты mcp зелёные.

- [ ] 5.7 get_document_context + get_chunk_by_id
  - Цель: документ и чанк.
  - Scope файлов: `crates/mcp/src/tools/documents.rs` (новый), регистрация.
  - Содержание: document_context — метаданные документа + чанки + сущности чанков +
    id фактов; chunk_by_id — чанк + инфо документа + сущности.
  - Тесты: паритет get_document_context_test.go / get_chunk_by_id_test.go:
    полная структура, not-found, документ без чанков.
  - Критерии приёмки: гейты mcp зелёные.

- [ ] 5.8 get_entity_dossier
  - Цель: досье сущности.
  - Scope файлов: `crates/mcp/src/tools/dossier.rs` (новый), регистрация.
  - Содержание: резолв по id ИЛИ имени → факты (approved) + источники + related
    сущности + кросс-доменные связи; форма из handlers/get_entity_dossier.go.
  - Тесты: паритет get_entity_dossier_test.go: по id, по имени, not-found,
    пустые секции.
  - Критерии приёмки: гейты mcp зелёные.

- [ ] 5.9 get_entity_relations + get_entity_links
  - Цель: графовые инструменты.
  - Scope файлов: `crates/mcp/src/tools/graph_tools.rs` (новый), регистрация.
  - Содержание: relations — обход графа от id/имени (graph traverser, лимиты из
    конфига); links — кросс-доменные связи с провенансом (entity_link DAO);
    поведение при отсутствии графа (Option<Graph>) — как у оракула.
  - Тесты: паритет get_entity_relations_test.go / get_entity_links_test.go:
    обход, лимиты, not-found, no-graph деградация.
  - Критерии приёмки: гейты mcp зелёные.

- [ ] 5.10 Интеграционный тест + финальная сборка
  - Цель: сквозная проверка транспорта и публичный API.
  - Scope файлов: `crates/mcp/tests/server_integration.rs` (новый),
    `crates/mcp/src/lib.rs`.
  - Содержание: поднять сервер на эфемерном порту → подключиться rmcp-клиентом
    (паттерн parity-harness mcp_client.rs) → initialize → tools/list = ровно 12 →
    вызвать search + один каталог-инструмент → структурные ассерты; /health через
    HTTP GET. Ре-экспорт публичного API из корня, crate docs, compile-time
    root-API тест.
  - Критерии приёмки: `RUSTDOCFLAGS="-D warnings" cargo doc -p mcp --no-deps` чисто;
    гейты зелёные; workspace-тест прогоняет оркестратор.
