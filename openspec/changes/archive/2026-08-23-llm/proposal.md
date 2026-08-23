# llm — Proposal

## Why

LLM-кросс-доменная линковка сущностей — третий метод пайплайна линкера; в change `graph` он оставлен стабом в ожидании этого change'а (решение человека 2026-08-22). Оракул использует OpenAI-совместимый HTTP-клиент (`internal/llm`, 356 строк) с ретраями и структурированным выводом; конфигурация (`LlmConfig` — «shared by NER and the linker») уже заморожена в config-крейте. Без клиента линкер не завершает пайплайн, а будущий LLM-NER (change ingestion) не имеет фундамента.

## What Changes

- Новый крейт `crates/llm` (базовый ярус D1, зависимость только от config): OpenAI-совместимый клиент `/chat/completions` на ureq (sync, конвенция workspace) — Bearer-аутентификация, таймаут, ретраи с экспоненциальным backoff+jitter (429/5xx retryable; empty content — non-retryable), структурированный вывод `json_object`/`json_schema`.
- Реальный LLM-линкер в `crates/graph/src/linker.rs` (замена `run_llm_stub`): загрузка контекста чанков (до 3 на сущность, `ChunkEntityDao::get_chunk_texts_by_entity`), рендеринг шаблонов system/user, вызов клиента, парсинг `LinkDecision {same_entity, confidence, reasoning}`, гейт `llm_confidence_threshold`, кэш решений в `app_kv`.
- Шаблоны промптов: загрузка из `prompts_path` с embedded-fallback (паттерн оракула); движок **minijinja** (решение человека 2026-08-23: шаблоны — данные с циклами и хелперами join/truncate); хэши шаблонов участвуют в ключе кэша.
- Кэш решений: **app_kv** с ключами `llm_link_{hash}` (решение 2026-08-23: отдельной таблицы в схеме оракула нет — проверено; ноль изменений схемы).
- Вызов попарно (как в оракуле); `batch_size` остаётся незадействованным (решение 2026-08-23).

**BREAKING (frozen stack, одобрено человеком 2026-08-23):** добавлен `minijinja` (pure Rust). Граф зависимостей D1 пополняется базовым крейтом `llm`: `config, db, vectors, llm → embedding, ingestion, graph → …`.

## Capabilities

### New Capabilities
- `llm-client`: контракт LLM-клиента — вызов chat-completions, структурированный вывод, ретраи/таймауты, конфигурация из LlmConfig.

### Modified Capabilities
- `knowledge-graph`: требование «Кросс-доменный пайплайн линковки» — метод `llm` становится реальным (контекст → шаблоны → вызов → парсинг → threshold → кэш) вместо стаба.

## Impact

- **Код:** новый `crates/llm`; `crates/graph` (+зависимость llm, prompts-модуль, замена стаба); корневой `Cargo.toml` (пины ureq-переиспользование, minijinja; members+palette).
- **Замороженные контракты:** config-format не меняется (LlmConfig/LinkerConfig/CrossDomainLinksConfig существуют); data-schema не меняется (app_kv существует); mcp-contract не затронут. Паритет — функциональный: те же пары при тех же ответах LLM дают те же линки (дифференциально через mock-сервер).
- **Потребители:** graph (линкер сейчас), ingestion/NER (будущий change — переиспользование клиента).
- **Отложено:** embeddings-via-API провайдер (решение 2026-08-21 — отдельный change); батчирование вызовов; LLM-NER.

## Non-goals

- Embeddings API-провайдер (`embeddings.mode=api`) — отдельный change.
- LLM-NER (`ner.llm`) — change ingestion.
- Батчирование пар в один запрос (batch_size) — YAGNI до реальной потребности.
- Стриминг ответов, function calling, мультимодальность — контракт оракула их не использует.
- Менеджмент секретов: api_key хранится в локальном конфиге открытым текстом (как в оракуле; локальный личный инструмент) — задокументировано в design.
