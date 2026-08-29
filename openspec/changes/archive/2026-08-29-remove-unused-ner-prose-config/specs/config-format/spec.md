# config-format Specification

## MODIFIED Requirements

### Requirement: Полный YAML-пресет

Rust-бинарь читает те же `config.{preset}.yaml`, что и Go оригинал. Пресет включает секции: `database` (path, pragma), `embeddings` (mode local|api), `ingestion` (chunking.markdown/json, ner.llm, batch_size, resolver), `linker` (disabled, llm), `search` (rrf_k, top-k, boosts, authority_boost), `graph`, `auto_update` (enabled, debounce_seconds, watch_sources, initial_sync), `scheduler.jobs` (поимённые job'ы с enabled/interval_seconds), `logging` (level/format/output), `paths` (data_dir, documents_dir, migrations_dir, global_config_path, prompts_path, onnx_config), `server` (name/version/host/port). Неизвестные ключи не ломают старт. Неизвестные значения строковых полей, не валидируемых оракулом (`logging.level/format/output`, `chunking.strategy`, `response_format`, `archive_format`, `source.type`, `attribute.type`), не ломают старт.

#### Scenario: Существующий пресет
- **WHEN** Rust-бинарь стартует с `../synopsis/configs/config.default.yaml` без изменений
- **THEN** конфиг parsed успешно, значения по умолчанию совпадают с Go бинарём на том же файле (machine-diff effective config)

#### Scenario: Неизвестное значение строкового поля
- **WHEN** `logging.level` содержит незнакомое значение (например "verbose")
- **THEN** старт не завершается ошибкой; значение сохраняется как есть

#### Scenario: Отсутствие секции auto_update
- **WHEN** в YAML нет секции auto_update
- **THEN** применяются дефолты enabled=true, initial_sync=true (как в оракуле)

#### Scenario: Явная секция auto_update
- **WHEN** в YAML есть auto_update с enabled=false
- **THEN** enabled=false уважается (не перезаписывается дефолтом)

#### Scenario: Булевы дефолты с presence-семантикой
- **WHEN** `graph.load_on_startup` или `auto_update.watch_sources` явно установлены в false
- **THEN** значение false уважается (**BREAKING**: в оракуле явный false принудительно переворачивался в true — настройка была нерабочей)
- **WHEN** эти ключи отсутствуют в YAML
- **THEN** применяется дефолт true

#### Scenario: enable_graph по интенту документации
- **WHEN** `graph.enable_graph` отсутствует в YAML
- **THEN** применяется true (**BREAKING**: в оракуле отсутствующий ключ давал false вопреки doc-комментарию «default true»)
