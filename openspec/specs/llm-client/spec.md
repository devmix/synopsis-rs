# llm-client Specification

## Purpose

OpenAI-совместимый LLM-клиент: единая точка вызова chat-completions с ретраями, таймаутом и структурированным выводом для линкера сущностей и будущего NER.

## Requirements

### Requirement: Вызов chat-completions

Крейт `llm` предоставляет синхронный клиент OpenAI-совместимого API `POST {api_base_url}/chat/completions`: тело запроса содержит model, messages (system + user), temperature, seed, max_tokens; аутентификация — Bearer-токен из `api_key` (пустой ключ — заголовок не отправляется); ответ парсится в текст первого choice. Клиент конфигурируется из `LlmConfig` (base URL, модель, температура, seed, max_tokens, таймаут, ретраи).

#### Scenario: Успешный вызов
- **WHEN** сервер валидного формата возвращает choices[0].message.content
- **THEN** клиент возвращает текст; запрос содержит model/messages/temperature/seed/max_tokens и Bearer-заголовок

#### Scenario: Пустой api_key
- **WHEN** api_key в конфиге пуст
- **THEN** Authorization-заголовок не отправляется; вызов выполняется

### Requirement: Структурированный вывод

Режим `response_format` из конфига управляет полем request_format: `json_object` → `{"type":"json_object"}`; `json_schema` → `{"type":"json_schema","json_schema":{"name":N,"schema":S}}`, где схема передаётся вызывающим кодом вместе с именем (default «llm_output»).

#### Scenario: json_object
- **WHEN** response_format=json_object и схема не задана
- **THEN** запрос содержит только {"type":"json_object"}

#### Scenario: json_schema
- **WHEN** response_format=json_schema и переданы имя и схема
- **THEN** запрос содержит вложенный объект json_schema с именем и схемой

### Requirement: Ретраи и таймауты

Временные сбои повторяются до `max_retries` раз с экспоненциальным backoff и jitter: HTTP 429 и 5xx — retryable; пустой content в валидном ответе — явная non-retryable ошибка; сетевые ошибки/таймаут — retryable. Таймаут одного запроса — из конфига. Исчерпание попыток — явная ошибка с причиной последнего сбоя.

#### Scenario: Ретрай на 429/5xx
- **WHEN** сервер отвечает 429 или 5xx
- **THEN** выполняется повторная попытка (до лимита) с растущей задержкой; успех после ретрая возвращает результат

#### Scenario: Пустой content без ретрая
- **WHEN** валидный ответ содержит пустой content
- **THEN** возвращается явная ошибка немедленно, без повторных попыток

#### Scenario: Исчерпание попыток
- **WHEN** все попытки завершаются временным сбоем
- **THEN** возвращается ошибка с указанием последней причины
