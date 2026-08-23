# llm — Design

Реализация LLM-клиента и реального LLM-линкера (замена стаба из change graph). Оракул: `internal/llm/client.go`, `internal/relations/llm_linker.go`, `internal/prompts/{loader,funcmap}.go`, шаблоны `configs/prompts/entity-linker/*.tmpl` — референс по поведению, не по коду.

## D1. Новый базовый крейт `crates/llm`

**Решение:** крейт на базовом ярусе D1 (зависимость только от config); граф пополняется: `config, db, vectors, llm → embedding, ingestion, graph → search → mcp → cli`. Публичный API: конструктор из `LlmConfig` + один метод вызова (system, user, опциональные schema/schema_name) → String.
**Почему не альтернатива:** модуль внутри config/db — чужеродно (HTTP-клиент не конфиг и не DAO); внутри graph — помешало бы переиспользованию NER'ом («shared by NER and the linker» — докa LlmConfig). Client не зависит от graph (инверсия: graph зависит от llm).
**Референс:** AGENTS.md layout (таблица дополняется строкой `crates/llm`).

## D2. ureq sync-клиент

**Решение:** ureq 3 (уже в workspace palette — downloader embedding'а), блокирующий API; вызывающий код (graph) оборачивает в spawn_blocking по конвенции D9. JSON — serde/serde_json (уже в палитре).
**Почему не альтернатива:** reqwest тянет tokio-стек поверх sync-фасада — двойной рантайм ради одного POST; hyper — слишком низкоуровневый.
**Референс:** crates/embedding/src/downloader.rs (паттерн использования ureq 3).

## D3. Шаблоны: minijinja + embedded-fallback

**Решение:** движок minijinja (решение человека 2026-08-23): шаблоны — данные (`prompts_path` позволяет переопределение пользователем без пересборки), содержат циклы по контекстным чанкам и хелперы join/truncate — hand-rolled парсер отвергнут. Загрузка: файлы из `paths.prompts_path/entity-linker/{system,user}.tmpl`; отсутствие файла → embedded-дефолт (include_str!), отличие фиксируется предупреждением в результатах (логгера в стеке нет — паттерн LinkResult.notes). Оракульные шаблоны переписываются в Jinja2-синтаксисе функционально (тот же текст промпта, та же структура данных).
**Референс:** internal/prompts/loader.go (embedded fallback), configs/prompts/entity-linker/*.tmpl.

## D4. Кэш решений: app_kv

**Решение:** ключ `llm_link_{hash}`, значение — JSON решения {same_entity, confidence, reasoning}; hash = sha256 (уже в палитре) от (id пары сущностей в каноническом порядке + хэши system/user шаблонов + имя модели). Хэши шаблонов в ключе: смена промпта инвалидирует кэш автоматически. TTL нет (как в оракуле).
**Почему не альтернатива:** отдельная таблица = изменение замороженной data-schema (в миграциях оракула таблицы кэша линкера НЕТ — проверено); in-memory кэш умирает между прогонами, а линковка — редкая операция над большим числом пар.
**Референс:** migration 005_app_kv.sql; AppKv DAO в db-крейте.

## D5. Попарные вызовы; batch_size не задействован

**Решение:** LinkPair-семантика оракула — один запрос на пару. Поле `batch_size` конфига остаётся незадействованным до реальной потребности (YAGNI, решение человека 2026-08-23): батчирование меняет форму промпта и схему ответа.

## D6. LinkDecision → EntityLink

**Решение:** парсинг JSON-ответа строго в {same_entity: bool, confidence: number, reasoning: string}; confidence клампится в [0,1]; link создаётся только при confidence ≥ `CrossDomainLinksConfig.llm_confidence_threshold`; атрибуты: method='llm', confidence, evidence=reasoning. Схема для json_schema-режима генерируется статически (та же форма). Не-JSON ответ или неверная форма — ошибка пары (non-fatal, фиксируется в LinkResult.errors), не паника.
**Референс:** internal/relations/llm_linker.go ParseLinkDecision/GenerateJSONSchema.

## D7. Безопасность и секреты

api_key хранится в локальном YAML открытым текстом — осознанно (локальный личный инструмент, паритет с оракулом); ключ не логируется, не попадает в ошибки/notes. Запросы уходят только на `api_base_url` из конфига (SSRF-защита загрузчика embedding'а здесь неприменима: base URL — легитимно пользовательский endpoint, может быть локальным прокси).

## D8. Отложено

Embeddings-via-API провайдер (решение 2026-08-21 — отдельный change); LLM-NER (ingestion); батчирование (D5); стриминг/function calling (контракт оракула не использует).

## Отклонения от оракула (осознанные)

- Go text/template → Jinja2/minijinja (шаблоны переписаны функционально).
- Кэш через app_kv вместо специализированного store (таблицы в схеме оракула нет).
- Предупреждения loader'а → LinkResult.notes (нет логгера в стеке).
- seed/temperature передаются как есть из LlmConfig (оракул — то же).
