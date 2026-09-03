# mcp-contract Specification

## Purpose

Внешний контракт MCP API Synopsis: набор инструментов, их параметры и ответы, транспорт. Контракты инструментов (имена, JSON-схемы, семантика) фиксируют поведение Go оригинала (`../synopsis/internal/mcp/tools.go`) как эталон паритета — клиенты (LLM-агенты) не должны замечать смены реализации по данным. Транспорт двойной: Streamable HTTP (официальный SDK rmcp 3.x, design D8) + legacy HTTP+SSE оракула (override D8, решение человека 2026-08-31; wire-контракт mcp-go v0.57.0).

## Requirements

### Requirement: Набор инструментов

Система предоставляет ровно 12 MCP tools со следующими именами: `search`, `catalog_overview`, `catalog_documents`, `catalog_entities`, `search_entities_by_type`, `search_facts`, `get_document_context`, `get_chunk_by_id`, `get_fact_by_id`, `get_entity_dossier`, `get_entity_relations`, `get_entity_links`.

#### Scenario: Перечень инструментов совпадает с оракулом
- **WHEN** клиент запрашивает tools/list у сервера Rust
- **THEN** ответ содержит ровно эти 12 имён, без лишних и без отсутствующих

### Requirement: Схемы параметров и ответов инструментов

Для каждого инструмента имена полей, типы и обязательность параметров из JSON-схем (tools/list) идентичны Go оригиналу; структура результата (имена полей, типизация, наличие/отсутствие опциональных полей при тех же входных данных) совпадает с ответами Go бинаря на одном и том же knowledge.db.

#### Scenario: JSON-схемы инструментов
- **WHEN** выполнить machine-diff ответа tools/list Rust-сервера против фикстуры, записанной одноразово из Go-сервера на том же knowledge.db (с нормализацией описаний)
- **THEN** схемы параметров каждого из 12 инструментов идентичны

#### Scenario: Паритет ответов на одинаковых данных
- **WHEN** вызов инструмента Rust-сервера с аргументами из фикстуры (записанной одноразово из Go-бинаря на том же knowledge.db)
- **THEN** результат совпадает по структуре и содержимому с записанным ответом оракула, за исключением полей, явно помеченных как приблизительные (ANN-scores, см. data/search specs), где допускается отклонение в пределах recall-гейта

### Requirement: Транспорт

Сервер предоставляет MCP протокол по HTTP на двух транспортах одновременно (оба всегда включены, без флагов и настроек): (1) Streamable HTTP (MCP spec ≥ 2025-11-25; официальный SDK rmcp 3.x, design D8) — единый endpoint для POST JSON-RPC с ответом plain JSON или SSE-потоком; (2) legacy HTTP+SSE оракула (mcp-go v0.57.0 SSEServer — wire-контракт оракула, override D8 решением человека 2026-08-31): `GET /sse` → `200 text/event-stream`, первый кадр `event: endpoint` с data = абсолютный URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host из `X-Forwarded-Proto`/`X-Forwarded-Host`, иначе `http` + заголовок `Host`; server-generated UUIDv4), далее кадры `event: message` с JSON-RPC 2.0; `POST /message?sessionId=<id>` → `202 Accepted` (пустое тело), ответы приходят по SSE-потоку; отсутствующий/неизвестный sessionId → `400`; keep-alive отключён (дефолт оракула); сессия живёт до закрытия SSE-потока или 300 с без активности (idle-reaper — отклонение от оракула для сервисной модели, решение человека 2026-09-01); wildcard-заголовок `Access-Control-Allow-Origin` НЕ выдаётся (отклонение от оракула, безопасность общего сервиса, решение человека 2026-09-01). JSON-RPC-методы legacy-транспорта — инструменты-only семантика mcp-go: `initialize` (result: protocolVersion/capabilities/serverInfo — serverInfo совпадает с name/version Streamable HTTP-пути), `notifications/initialized` (без ответа), `ping` (`{}`), `tools/list` (те же 12 инструментов), `tools/call` (тот же `Server::dispatch`; tool-ошибка → результат с `isError: true`, протокольная → JSON-RPC-ошибка); неизвестный метод → `-32601`. Дополнительно: `GET /health` — статус, версия и счётчики knowledge base; поведение health-endpoint совпадает с Go оригиналом по структуре ответа.

#### Scenario: Подключение modern MCP client
- **WHEN** MCP-клиент (Streamable HTTP) выполняет initialize и tools/list против Rust-сервера
- **THEN** handshake завершается согласованной версией протокола, tools/list возвращает 12 инструментов

#### Scenario: Health endpoint
- **WHEN** GET /health после успешного старта
- **THEN** 200 со структурой (статус/версия/sync-состояние/счётчики), совместимой с ответом Go бинаря

#### Scenario: Подключение legacy SSE-клиента
- **WHEN** MCP-клиент (legacy SSE) выполняет GET /sse
- **THEN** 200 text/event-stream; первый кадр — `event: endpoint` с data = абсолютный URL `<scheme>://<host>/message?sessionId=<uuid>` (scheme/host из `X-Forwarded-Proto`/`X-Forwarded-Host`, иначе `http` + `Host`)

#### Scenario: Idle-таймаут сессии
- **WHEN** legacy SSE-сессия 300 секунд не получает ни одного JSON-RPC-запроса
- **THEN** reaper закрывает сессию (SSE-поток завершается); последующие POST для этого sessionId дают 400

#### Scenario: Вызов инструмента по legacy SSE
- **WHEN** legacy SSE-клиент шлёт `tools/call` POST /message?sessionId=<id>
- **THEN** сервер отвечает 202 Accepted, а результат приходит кадром `event: message` (JSON-RPC response) по SSE-потоку той же сессии; тот же вызов по Streamable HTTP возвращает идентичный JSON (кроме полей, помеченных как приблизительные)

#### Scenario: Невалидная сессия
- **WHEN** POST /message?sessionId=<id> с отсутствующим или неизвестным sessionId
- **THEN** 400 Bad Request; на существующие сессии это не влияет

#### Scenario: Разрыв соединения
- **WHEN** legacy SSE-клиент закрывает соединение GET /sse
- **THEN** сессия удаляется из реестра; последующие POST для этого sessionId дают 400

### Requirement: Чтение только, статусов facts — approved-only

Все read-результаты содержат только факты с `status = 'approved'` (pending доступен лишь в тех инструментах, где это поведение задокументировано в оракуле). Записывающие инструменты на этом этапе НЕ предоставляются (write tools — отдельный будущий change).

#### Scenario: Фильтр approved-only
- **WHEN** в knowledge.db есть факты со status pending и approved, и клиент запрашивает результаты поиска/досье
- **THEN** в результатах присутствуют только approved-факты (как в Go оригинале)

### Requirement: search result carries chunk metadata
The `search` tool's result item SHALL carry a `metadata` field holding the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) as a raw JSON object — the Go oracle's `SearchResult.Metadata`, which the Rust port previously dropped. The bag SHALL be passed through uncured; an empty bag SHALL be omitted from the JSON. The `text` field's type is unchanged (a string) — only its content is now the chunk's pure body (`chunk_text`) rather than the breadcrumb-prefixed `search_text`. Of the Go oracle's *document-level* keys in `SearchResult.Metadata`, `updated_at` is now exposed as a top-level result field (see "search result carries document freshness"); the remaining keys (`document_source_type`, `document_metadata_json`) are still a deferred concern and SHALL NOT be part of the `metadata` bag.

#### Scenario: Sectioned chunk metadata in the response
- **WHEN** a search returns a chunk produced under a heading hierarchy
- **THEN** the result item's `metadata` object carries `section_title`, `heading_level`, and `breadcrumb`

#### Scenario: Empty bag omitted
- **WHEN** a search returns a chunk with no chunk-specific metadata
- **THEN** the result item has no `metadata` field (omitted, not `{}`)

#### Scenario: text is the pure body
- **WHEN** a search returns a chunk
- **THEN** the result item's `text` is the chunk's pure body (the byte-offset slice), and the section context is in `metadata`, not glued into `text`

### Requirement: search result carries document freshness
The `search` tool's result item SHALL carry an `updated_at` field holding the owning document's `updated_at` normalized to RFC3339 (the value the enricher already computes into the result's enrichment bag). The field SHALL be omitted from the JSON when the document has no parseable timestamp. This is a deliberate, additive divergence from the Go oracle's MCP wire item (`../synopsis/internal/mcp/handlers/search.go`), which does not expose `updated_at` per result: a RAG client can now see hit freshness without a second `get_document_context` call. It is distinct from the chunk's own `metadata` bag — `updated_at` is a top-level result field, not a bag key. The parity harness strips `updated_at` from the `search` response before comparison (it is a non-deterministic timestamp), so content parity is unaffected.

#### Scenario: Document with a parseable timestamp
- **WHEN** a search returns a chunk whose document has a parseable `updated_at`
- **THEN** the result item's `updated_at` is that timestamp in RFC3339 form

#### Scenario: Document with no parseable timestamp
- **WHEN** a search returns a chunk whose document has no (or unparseable) `updated_at`
- **THEN** the result item has no `updated_at` field (omitted, not `null`)
