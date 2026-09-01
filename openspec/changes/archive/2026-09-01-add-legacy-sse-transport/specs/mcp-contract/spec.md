# mcp-contract Specification (delta)

## MODIFIED Requirements

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
