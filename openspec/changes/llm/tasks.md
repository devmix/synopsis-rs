# llm — Tasks

Порядок = граф зависимостей: 1.1 → {1.2, 2.1} → 1.3 → 2.2 → 2.3. Каждая задача выполняется СВЕЖИМ агентом без памяти предыдущих — тело самодостаточно. Формат: чекбокс + блок деталей (цель / scope / зависимости / критерии приёмки / референс / история ревизий). **Принцип миграции (binding):** НЕ транскрибировать Go 1:1 — функциональная копия; архитектурно правильно для Rust (DRY, KISS, SOLID, YAGNI); внутренняя совместимость с оракулом не требуется; баги Go исправлять или фиксировать осознанные отклонения. CI без сети: все сетевые тесты — на mock-сервере TcpListener (прецедент: crates/embedding, crates/vectors).

Общие факты для всех задач (источники истины): design этого change'а D1–D8; config-крейт: `LlmConfig {api_base_url, api_key, model_name, temperature, max_tokens, seed, response_format(json_object|json_schema), timeout_ms, max_retries}`, `LinkerConfig {disabled, llm}`, `CrossDomainLinksConfig.llm_confidence_threshold`, `paths.prompts_path`; db-крейт: AppKv DAO, `ChunkEntityDao::get_chunk_texts_by_entity` (существует); graph-крейт: linker.rs с `run_llm_stub` (заменяется задачей 2.2), LinkResult {links_created, notes, errors}, CelEngine не участвует. Гейты каждой задачи: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -p <crate>`, `cargo test --workspace`; missing_docs=deny, unsafe_code=forbid; пин 1.96.0; Cargo.lock коммитится. Untracked `.idea/`, `.opencode/opencode.json`, `.opencode/plans/`, skills-lock.json не трогать.

---

- [x] 1.1 Скаффолдинг крейта llm
  - **Цель:** создать `crates/llm` (базовый ярус D1, зависимость только config) с ошибками и конфигурационной сборкой клиента.
  - **Scope файлов:** `crates/llm/Cargo.toml`, `crates/llm/src/lib.rs`, `crates/llm/src/error.rs`; корневой `Cargo.toml` (members + palette: minijinja pin; ureq/serde уже есть).
  - **Детали:** `LlmError` (thiserror) по конвенции workspace (см. error.rs соседних крейтов): варианты для HTTP-статусов (retryable/non-retryable различимы), сети/таймаута, пустого content, парсинга ответа, конфигурации. Конструктор клиента из `&LlmConfig` (валидация: base URL непустой, timeout_ms > 0 и т.п. — по вкусу, задокументировать). Публичный API пока каркас: метод вызова появится в 1.2.
  - **Критерии приёмки:** все гейты зелёные; cargo build -p llm компилируется; юнит-тесты валидации конфига и маппинга ошибок; missing_docs чисто.
  - **Зависимости:** нет.
  - **Референс:** design D1/D2; паттерн скаффолдинга — archive vectors 1.1 / graph 1.1.

- [x] 1.2 Клиент: запрос/ответ/Bearer
  - **Цель:** ядро вызова POST {base}/chat/completions: сборка тела, Bearer, парсинг ответа в текст.
  - **Scope файлов:** `crates/llm/src/client.rs` (+тесты), lib.rs (ре-экспорт).
  - **Детали:** тело: model, messages[{role:system},{role:user}] (content строкой — оракул использует parts, но для текстовых промптов строка эквивалентна; зафиксировать решение), temperature, seed, max_tokens, response_format (json_object → {"type":"json_object"}; json_schema → вложенный объект с name/schema; default name «llm_output»). Пустой api_key → без Authorization. Ответ: choices[0].message.content; пустой content → явная non-retryable ошибка. Таймаут из конфига на запрос.
  - **Критерии приёмки:** mock-сервер TcpListener: успешный ответ возвращает content; тело запроса соответствует ожидаемому JSON (golden-тест); пустой api_key → заголовка нет; пустой content → ошибка без ретрая; гейты зелёные.
  - **Зависимости:** 1.1.
  - **Референс:** design D2/D6; internal/llm/client.go (форма тела/ответа); mock-паттерн crates/embedding/src/model.rs.

- [x] 1.3 Клиент: ретраи и структурированный вывод
  - **Цель:** retry-политика и полный response_format.
  - **Scope файлов:** `crates/llm/src/client.rs` (+тесты).
  - **Детали:** 429/5xx и сетевые ошибки/таймаут — retryable до max_retries с экспоненциальным backoff + jitter (задержки инъецируемы для тестов — не спать по-настоящему); empty content — non-retryable немедленно; исчерпание — ошибка с последней причиной. json_schema-режим принимает (name, schema) от вызывающего.
  - **Критерии приёмки:** тесты: 429→ретрай→успех; 5xx исчерпание → ошибка с причиной; empty content → без ретрая; backoff растёт (инъектированный таймер); json_schema попадает в тело запроса; гейты зелёные.
  - **Зависимости:** 1.2.
  - **Референс:** design D6; internal/llm/client.go (retry-политика).

- [ ] 2.1 Prompts-модуль graph: загрузка + рендеринг
  - **Цель:** шаблоны entity-linker: загрузка из prompts_path с embedded-fallback, рендеринг minijinja, хэши для ключа кэша.
  - **Scope файлов:** `crates/graph/src/prompts.rs` (+тесты), `crates/graph/Cargo.toml` (+minijinja, +llm dep появится в 2.2 — можно сразу), lib.rs (ре-экспорт).
  - **Детали:** embedded-дефолты system/user (include_str! из fixtures — переписать оракульные configs/prompts/entity-linker/*.tmpl в Jinja2 функционально: тот же текст, циклы по контекстным чанкам, хелперы join/truncate зарегистрировать в minijinja). Загрузка: если файл существует по prompts_path/entity-linker/{system,user}.tmpl — он побеждает; отличие фиксируется записью в notes-канал (возврат из загрузчика). Хэши sha256 обоих отрендеренных ИСТОЧНИКОВ шаблонов (не данных) — для ключа кэша D4.
  - **Критерии приёмки:** тесты: embedded-fallback рендерится; override-файл побеждает; хелперы join/truncate работают; хэши стабильны и меняются при смене шаблона; гейты зелёные.
  - **Зависимости:** 1.1 (крейт llm существует), независимо от 1.2–1.3.
  - **Референс:** design D3; internal/prompts/loader.go; ../synopsis/configs/prompts/entity-linker/*.tmpl.

- [ ] 2.2 Реальный LLM-линкер (замена стаба)
  - **Цель:** run_llm: контекст → шаблоны → вызов → парсинг → threshold → кэш → link.
  - **Scope файлов:** `crates/graph/src/linker.rs` (замена run_llm_stub), lib.rs (ре-экспорты).
  - **Детали:** для каждой пары после equals/expression: контекст — до 3 текстов чанков на сущность (ChunkEntityDao::get_chunk_texts_by_entity); user-шаблон получает данные обеих сущностей (name/type/domain/description/context[]); вызов клиента (response_format из LlmConfig; json_schema-схема статическая {same_entity, confidence, reasoning}); парсинг строгий, confidence клампится [0,1]; threshold из CrossDomainLinksConfig.llm_confidence_threshold; link method='llm', evidence=reasoning через существующий create_bidirectional_link; кэш app_kv `llm_link_{sha256(pair_canonical + template_hashes + model)}` — проверка ДО вызова, запись ПОСЛЕ решения (в т.ч. ниже порога). Ошибка одной пары — non-fatal в LinkResult.errors. disabled исключает метод (как в стабе).
  - **Критерии приёмки:** юнит-тесты на mock-сервере: выше порога → link создан; ниже → нет, но кэш записан; cache hit → LLM не вызывается (счётчик запросов сервера); сбой вызова → errors, пайплайн жив; disabled → метод пропущен; гейты зелёные.
  - **Зависимости:** 1.3, 2.1.
  - **Референс:** design D4/D5/D6; internal/relations/llm_linker.go; текущий run_llm_stub.

- [ ] 2.3 E2E-интеграция линкера
  - **Цель:** сквозной тест пайплайна с реальным global.xml + mock-LLM.
  - **Scope файлов:** `crates/graph/tests/llm_linker_pipeline.rs` (+фикстуры при необходимости).
  - **Детали:** расширение существующего e2e (linker_pipeline.rs): БД с двумя доменами, онтология, mock-LLM отвечает валидным решением → link method='llm'; повторный прогон — идемпотентен И LLM не вызывается повторно для тех же пар (кэш); смена шаблона (override) инвалидирует кэш → новый вызов.
  - **Критерии приёмки:** e2e зелёный; счётчик запросов mock-сервера подтверждает кэширование и инвалидацию; все гейты workspace зелёные.
  - **Зависимости:** 2.2.
  - **Референс:** design D4; crates/graph/tests/linker_pipeline.rs (паттерн).
