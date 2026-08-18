# Design: config-module

## Context

`crates/config` — листовой крейт (tier 0), сейчас пустой стаб. Мотивация и скоуп — в proposal.md. Требования — в specs/config-format/spec.md (дельта). Go-референсы: `../synopsis/internal/config/config.go` (612 строк: Load/ApplyDefaults/Validate/ONNX), `../synopsis/internal/config/global_config.go` (global.xml: sources/cross-domain-links/ner), `../synopsis/internal/domain/domain_config.go` (domain-XML), `../synopsis/internal/domain/global_pool.go` (пул + MergeInto), их тесты `*_test.go`, файлы `../synopsis/configs/*.yaml`, `../synopsis/data/ontology/*.xml`.

## Goals / Non-Goals

**Goals:**
- Полный API конфигурации: YAML-пресеты, реестр onnx.yaml, онтологии XML — с дефолтами и валидацией, поведение совпадает с оракулом (кроме явных решений ниже).
- Rust-идиоматичный API: serde + enum'ы + thiserror; без Go-приёмов (node-сканирование, двойной парсинг, merge-мутации).
- Крейт самодостаточен: фикстуры закоммичены, тесты не зависят от `../synopsis` (CI без оракула).

**Non-Goals:**
- Registry/merge-логика пула, shadowing-lookup и валидация на объединении слоёв — change `graph` (здесь: парсинг + per-file валидация).
- Скачивание/проверка моделей ONNX — change `embedding`.
- CLI-флаги, resolution пресетов, загрузка prompt-шаблонов — change `cli`/`mcp`.
- Изменение форматов файлов — только API-представление.

## Decisions

### D1: API — serde-структуры, без builder
`Config::load(path) -> Result<Config, ConfigError>` (только парсинг), `apply_defaults(&mut self)`, `validate(&self) -> Result<(), ConfigError>`; хелперы `vector_dim()`, `db_path()`, `cache_db_path()`. Аналогично для `OnnxConfig` и онтологий.
*Почему:* KISS; конфиг читается один раз при старте, builder не даёт выгоды (решение человека). *Альтернативы:* builder (отклонён), единый `from_file` с цепочкой (отклонён — тесты и CLI хотят контролировать фазы). *Референс:* config.go Load/ApplyDefaults/Validate.

### D2: YAML — noyalib 0.0.23
Уже в палитре (проверен 2026-08-18; преемник deprecated serde_yaml/serde_yml). Неизвестные ключи игнорируются по умолчанию — совпадает с оракулом («неизвестные ключи не ломают старт»).
*Альтернативы:* serde_yaml/serde_yml (deprecated), yaml-rust2 (ниже уровень, без serde-интеграции).

### D3: XML — quick-xml (event-based, ручной мэппинг)
Выбор человека (поддержка записи на будущее). Атрибутно-насыщенные структуры онтологий маппятся вручную через `Reader` (события Start/End/Text) в типизированные структуры — предсказуемые ошибки с контекстом файла/элемента. Версию/MSRV/CVE реализатор проверяет онлайн перед пином (пин — в `crates/config/Cargo.toml`, крейт не общий).
*Альтернативы:* roxmltree (read-only DOM, нет writer — отклонён из-за будущей записи), xml-rs (старый, менее активный), serde-десериализация quick-xml (отклонена: неудобна для атрибутов, ошибки без контекста).

### D4: Ошибки — thiserror
`ConfigError` enum: `Io { path }`, `Yaml { path, source }`, `Xml { path, source }`, `Regex { file, rule, source }`, `Validation { message }`. Добавляется в палитру (версия — онлайн-проверка реализатором).
*Альтернатива:* hand-rolled Display (отклонена человеком — thiserror).

### D5: Regex компилируется при загрузке
`<regex pattern=...>` из global.xml и domain-XML компилируются в `regex::Regex` при загрузке; невалидный паттерн — ошибка старта с указанием файла и правила (решение человека: fail fast, как в Go).
*Альтернатива:* хранить строки, компилировать в ingestion (отклонена человеком — «лучше узнать сразу»). `regex` добавляется в палитру (онлайн-проверка версии).

### D6: Глобальный пул — слои вместо merge (BREAKING, решение человека)
- Один парсер global.xml (в Go файл парсится дважды: config.LoadGlobalConfig и domain.LoadGlobalPool — два набора структур, две валидации; в Rust — один проход, одна структура `GlobalConfig` с entities/relations/extraction + sources/cross-domain-links/ner).
- Эффективная схема домена = слой домена + глобальный слой как fallback (lookup: домен → глобальный). Без мутаций, без warnings; «override» становится явным shadowing'ом.
- Per-file валидация в config-крейте: уникальность ID в файле, ref-цели в файле, regex-компиляция. Валидация на объединении слоёв и сам lookup — в change `graph` (registry), сценарий спека «Валидация на объединении слоёв» покрывается там.
*Почему:* устраняет рассинхрон Go (глобальное relation может ссылаться на entity, переопределённую доменом — в Go после merge остаётся висячая ссылка); композиция без мутаций. *Альтернативы:* A — сохранить merge+warning (отклонена: неявная семантика, мутации), C — убрать пул (отклонена: потеря общей схемы). *Референс:* global_pool.go MergeInto/Validate.

### D7: Enum'ы вместо строк (решение человека)
Формат YAML не меняется (serde rename), API — на enum'ах:
- **Строгие** (Go валидирует): `EmbeddingsMode` (local/api — Validate), `NerMethod` (regex/prose/llm), `LinkMethod` (expression/equals/llm).
- **Толерантные** `Unknown(String)` (Go пропускает любые строки): `ChunkingStrategy`, `LogLevel`, `LogFormat`, `LogOutput`, `ResponseFormat`, `ArchiveFormat`, `SourceType`, `AttributeType`, `RelationType`.
*Почему:* типизация там, где оракул уже валидирует; сохранение толерантности там, где оракул её имеет (незнакомое значение не роняет старт — сценарий спека). *Альтернатива:* все строки как в Go (отклонена человеком).

### D8: auto_update presence — Option вместо node-сканирования
Go сканирует yaml.Node в поисках ключа `auto_update` (detectAutoUpdatePresence). В Rust: `auto_update: Option<AutoUpdateConfig>`; `None` → дефолты enabled=true/initial_sync=true, `Some` → уважается как есть. Тот же контракт, без хака.
*Референс:* config.go detectAutoUpdatePresence + ApplyDefaults.

### D9: Модульная структура
`src/lib.rs` (re-exports, crate docs), `src/error.rs` (ConfigError), `src/preset.rs` (YAML-пресет: Config + секции + дефолты + валидация), `src/onnx.rs` (реестр), `src/ontology.rs` (global.xml: GlobalConfig + loader), `src/domain.rs` (domain-XML: DomainConfig + loader). Разделение ontology/domain — по задачам 3.1/3.2 (каждая ≤ ~500 строк).

### D10: Фикстуры — `crates/config/tests/data/`
Копии из оракула: `config.default.yaml`, `onnx.yaml`, `global.xml`, `domains/domain_{hr,it,product}.xml` + `README.md` (provenance: скопированы verbatim из ../synopsis, дата, команда регенерации). Тесты читают через `env!("CARGO_MANIFEST_DIR")`.
*Почему не `tests/fixtures/`:* корневой `.gitignore` игнорирует `fixtures/*` на любом уровне (паттерн без ведущего слэша). *Почему не inline-константы:* ~750 строк, нечитаемые диффы. *Почему не ссылки на ../synopsis:* CI без оракула.

### D11: Зависимости
- В палитру (`Cargo.toml` workspace): `thiserror`, `regex` — с комментарием «версии проверены реализатором онлайн» (MSRV/CVE).
- Локально в `crates/config/Cargo.toml`: `quick-xml` (только config использует), `noyalib`, `serde` — через workspace.

## Risks / Trade-offs

- [quick-xml мэппинг атрибутов трудоёмок] → ручной event-based парсинг + фикстуры как ground truth; ошибки с контекстом файла.
- [noyalib API-нюансы (Option-presence, неизвестные ключи)] → ранние юнит-тесты в задаче 1.1 на реальном пресете.
- [Дрейф фикстур при изменении файлов оракула] → README с командой регенерации; machine-diff против Go-бинаря позже в parity-harness.
- [Строгие enum'ы могут упасть на значении, которое Go пропускает] → только там, где Go валидирует; толерантные Unknown для остальных; тесты на незнакомые значения.
- [BREAKING: shadowing вместо warning] → явное решение человека; дельта спека; warning-потребителей нет (граф ещё не реализован).

## Migration Plan

Листовой крейт без потребителей: миграции нет. Откат — revert коммита (удаление крейта из members + палитры). Порядок задач: 1.1 → 1.2 → 2.1 → 3.1 → 3.2 (приоритет yaml > onnx > xml, решение человека).

## Open Questions

Нет блокирующих. Мелочи (имена методов, точный набор re-exports) — на усмотрение реализатора в рамках задач.