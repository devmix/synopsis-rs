# Tasks: ingestion-ner

Зависимости change'а: ingestion-sources (архив), crates/llm, crates/db, crates/config — все готовы.
Оракул (read-only): `../synopsis/internal/ingestion/ner/`, `../synopsis/internal/ingestion/entities/`,
`../synopsis/configs/prompts/ner/{system,user}.tmpl`. Дизайн: design.md D1–D10.
Гейты каждой задачи: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test -p ingestion`; `cargo test --workspace` гоняет ТОЛЬКО оркестратор
(диск: линковка гигабайтных debug-бинарников дважды вешала агентов).
Директива (binding): НЕ копировать Go 1:1 — функциональная копия, архитектура для Rust
(DRY/KISS/SOLID/YAGNI); баги оракула исправлять или фиксировать отклонения.

- [x] 2.1 Ядро NER: типы + трейт + RegexNer
  - Цель: фундамент модуля ner и первый провайдер.
  - Scope файлов: `crates/ingestion/src/ner/mod.rs` (новый), `crates/ingestion/src/ner/regex.rs` (новый),
    `crates/ingestion/src/lib.rs` (объявление модуля), `crates/ingestion/Cargo.toml` (+regex workspace).
  - Содержание: типы `NerEntity{name,type,description,confidence,domain,metadata}`,
    `NerFact{subject_type,subject_name,predicate,object_type,object_name,domain,metadata}`,
    `NerResult{entities,facts}`; объектно-безопасный трейт `NerProvider: Send+Sync`
    (`name() -> &'static str`, `extract_entities(&self, content, metadata) -> Result<Option<NerResult>, IngestionError>`,
    дизайн D2); `RegexNer` по D3: prepared rules из доменных конфигов
    (`config::ontology::RegexRuleDef`, `CompiledPattern` уже есть), предпочтение capture-группы 1,
    trim, дедуп по (name,type,domain), `rule_id` в metadata, пустой контент/нет правил → Ok(None).
  - Тесты: паритет кейсов из `../synopsis/internal/ingestion/ner/regex_ner_test.go`
    (capture/no-capture, dedup, multi-domain, empty content).
  - Критерии приёмки: гейты зелёные; кейсы оракула покрыты; публичные типы задокументированы.

- [x] 2.2 Промпты NER: embedded minijinja + контекст документа (BINDING)
  - Цель: шаблоны system/user с явным блоком контекста документа.
  - Scope файлов: `crates/ingestion/src/ner/prompts.rs` (новый),
    `crates/ingestion/src/ner/templates/system.tmpl` + `user.tmpl` (новые, include_str!),
    `crates/ingestion/Cargo.toml` (+minijinja workspace).
  - Содержание: порт `../synopsis/configs/prompts/ner/{system,user}.tmpl` на minijinja
    (прецедент: `crates/graph/src/prompts.rs` — embedded defaults + user overrides + TemplateHashes);
    рендер system: entity/relation defs схемы домена + JSON-пример (флаг);
    рендер user: списки типов + КОНТЕКСТ ДОКУМЕНТА из метаданных чанка
    (breadcrumbs/section path — явный блок «Section path: A > B > C», OMIT если нет)
    + чистый текст чанка без префиксов. Binding-требование человека 2026-08-23:
    контекст ОБЯЗАН попадать в промпт явно (оракул делал это неявно через префиксы text —
    наш фикс бага офсетов). Контент рендерится в user-промпт напрямую
    (у LlmClient нет attachments — отклонение зафиксировано в design D4).
  - Тесты: рендер с/без breadcrumbs; переопределение пользовательским шаблоном;
    схема домена с атрибутами/синонимами.
  - Критерии приёмки: гейты зелёные; binding-сценарии спеки покрыты тестами.

- [x] 2.3 Обработка ответа LLM: parse + validate + JSON-schema
  - Цель: чистые функции обработки вывода LLM и генерация schema.
  - Scope файлов: `crates/ingestion/src/ner/llm_schema.rs` (новый),
    `crates/ingestion/src/ner/parse.rs` (новый).
  - Содержание: порт `GenerateJSONSchema` из `../synopsis/internal/ingestion/ner/llm_json_schema.go`
    (entities/relations schema, requires_schema вариант); parse по design D5:
    skip пустых имён, confidence default 0.5 только вне [0,1], truncate_description
    ≤500 на границе предложения (.!?;) иначе hard cap, validate_metadata
    (drop «implied by context»/«not explicitly stated», version regex
    `^[vV]?[0-9]+([._-][a-zA-Z0-9]+)*$` + reject-list « years», «-to-», «approximately»…),
    факты с пустым обязательным полем — skip.
  - Тесты: паритет кейсов `llm_json_schema_test.go` + `llm_ner_test.go` (parse-часть):
    truncate на границе, невалидный confidence, мусорные version, uncertainty-фильтр.
  - Критерии приёмки: гейты зелёные; функции чистые (без I/O).

- [x] 2.4 Кэш LLM-NER: sha256-ключ + ленивая таблица
  - Цель: персистентный кэш ответов.
  - Scope файлов: `crates/ingestion/src/ner/llm_cache.rs` (новый),
    `crates/ingestion/Cargo.toml` (+sha2 workspace).
  - Содержание: `build_cache_key(server,model,temperature,max_tokens,system,user,content)`
    → sha256 hex от «:»-join (temperature как `%g` — форматировать как оракул:
    кратчайшее представление без хвостовых нулей); store над rusqlite:
    `CREATE TABLE IF NOT EXISTS llm_ner_cache (cache_key TEXT PRIMARY KEY, result TEXT NOT NULL)`
    лениво при первом использовании (design D6 — таблица НЕ в миграциях, как у оракула);
    get → Option<NerResult> (повреждённая запись = промах), set сериализует NerResult;
    отключённый store = no-op.
  - Тесты: roundtrip; повреждённая запись → промах; детерминизм ключа; формат temperature.
  - Критерии приёмки: гейты зелёные; миграции не тронуты.

- [x] 2.5 LlmNer: сборка провайдера
  - Цель: полный LLM-провайдер поверх 2.2–2.4.
  - Scope файлов: `crates/ingestion/src/ner/llm.rs` (новый),
    `crates/ingestion/Cargo.toml` (+llm, db workspace).
  - Содержание: конструктор требует ≥1 доменный конфиг (иначе ошибка — как оракул);
    extract по design D5/D7: цикл доменов в порядке конфига, render промптов (2.2),
    cache key + get (2.4), при промахе `LlmClient::call(system, user, Some(schema), Some("ner_result"))`,
    parse/validate (2.3), тег домена на каждую сущность/факт, cache set ДО обогащения
    source-metadata (как оракул). Зависимости: `LlmConfig` из config crate.
  - Тесты: mock-TcpListener паттерн из crates/llm (интеграционный тест с фейковым
    OpenAI-совместимым сервером): hit/miss кэша, тегирование доменом, ошибка вызова → Err.
  - Критерии приёмки: гейты зелёные; сетевые тесты детерминированы (локальный mock).

- [x] 2.6 CompositeNer: стадии + пороговая фильтрация
  - Цель: оркестрация провайдеров и auto-publish фильтр.
  - Scope файлов: `crates/ingestion/src/ner/composite.rs` (новый).
  - Содержание: построение из `GlobalNerConfig.methods` (стадии regex|llm;
    неизвестная стадия → ошибка с перечнем допустимых; prose → отдельная осмысленная
    ошибка со ссылкой на решение об отсрочке); extract по design D7: последовательный запуск,
    короткое замыкание на ошибке, enrich metadata (source metadata extend + provider tag,
    domain провайдера сохраняется), фильтр auto_publish_threshold по доменам
    (config::domain thresholds), каскад фактов (субъект ИЛИ объект отфильтрован → факт долой),
    неизвестные домены проходят.
  - Тесты: паритет сценариев `../synopsis/internal/ingestion/ner/composite_test.go`
    (порядок стадий, enrichment, threshold-фильтр, каскад, unknown domain pass-through).
  - Критерии приёмки: гейты зелёные.

- [ ] 2.7 Примитивы разрешения: Jaro-Winkler + bigrams + кластеризация
  - Цель: чистая математика дедупликации.
  - Scope файлов: `crates/ingestion/src/entities/similarity.rs` (новый),
    `crates/ingestion/src/entities/cluster.rs` (новый), `crates/ingestion/src/entities/mod.rs` (новый).
  - Содержание: normalize_name (trim+lowercase+collapse whitespace), rune-aware bigrams
    (<2 rune → само имя), jaro_winkler (окно max(len)/2−1, prefix bonus ≤4×0.1) — design D8;
    cluster_batch: blocking `domain:type:bigram`, union-find с компрессией пути, checked-pairs,
    порядок кластеров по первому появлению; canonical_proto (длиннейшее имя, ties → первый);
    scope_entity_metadata (drop url/image_paths/page_links/categories, keep provenance,
    title → имя сущности если был строкой).
  - Тесты: ПОЛНЫЙ паритет `../synopsis/internal/ingestion/entities/similarity_test.go`
    + чистые кейсы `resolver_test.go` (включая кириллицу).
  - Критерии приёмки: гейты зелёные; кириллические кейсы оракула совпадают.

- [ ] 2.8 Resolver: индекс + операции над БД
  - Цель: персистентная дедупликация сущностей.
  - Scope файлов: `crates/ingestion/src/entities/resolver.rs` (новый),
    `crates/ingestion/Cargo.toml` (+db workspace).
  - Содержание: design D9 — Mutex-индекс (names/byID/blocks/domains) + hydrated flag +
    threshold из `ResolverConfig.similarity_threshold`; lazy hydrate через `EntityDao::list`;
    find_best_candidate (exact name → 1.0, иначе лучший JW по блокам того же type AND domain);
    resolve_one (merge-or-create, promote длиннейшего канона: DAO update + оба name-key +
    новые bigram-блоки; кандидат исчез (GC) → rehydrate + retry once);
    lookup / lookup_or_create_with_stats (созданные → EntitySourceDao::link_batch, счётчик новых)
    / add_entities (кластеризация батча → канон на кластер → dedup ids → link_batch);
    создание через `EntityDao::get_or_create` со scoped metadata JSON + description.
    Все операции принимают `&db::Executor` (pool или tx — аналог DBTX оракула).
  - Тесты: DB-сценарии `resolver_test.go` против in-memory SQLite (паттерн crates/db tests):
    hydrate, exact hit, similarity merge, promote имени, cross-domain изоляция, stats.
  - Критерии приёмки: гейты зелёные; сценарии оракула покрыты.

- [ ] 2.9 Финальная сборка: ре-экспорты + документация
  - Цель: публичный API модулей ner/entities из корня крейта.
  - Scope файлов: `crates/ingestion/src/lib.rs`, `crates/ingestion/src/ner/mod.rs`,
    `crates/ingestion/src/entities/mod.rs`.
  - Содержание: ре-экспорт всех публичных типов/провайдеров/Resolver в корне
    (конвенция задач 1.11/embedding/vectors/graph); crate docs отражают NER-слой;
    compile-time тест доступности API из корня.
  - Критерии приёмки: `RUSTDOCFLAGS="-D warnings" cargo doc -p ingestion --no-deps` без предупреждений;
    гейты зелёные; workspace-тест прогоняет оркестратор.
