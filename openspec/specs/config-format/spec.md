# config-format Specification

## Purpose

Форматы конфигурационных файлов Synopsis: YAML-пресеты (`config.{preset}.yaml`), реестр моделей `onnx.yaml`, онтологии XML в `data/ontology/`. Фиксирует совместимость с существующими файлами из `../synopsis/configs/` и `../synopsis/data/ontology/`: пользователь не должен менять конфиги при переходе на Rust-бинарь.
## Requirements
### Requirement: Полный YAML-пресет

Rust-бинарь читает те же `config.{preset}.yaml`, что и Go оригинал. Пресет включает секции: `database` (path, pragma), `embeddings` (mode local|api), `ingestion` (chunking.markdown/json, ner.llm, batch_size, resolver, max_retries), `linker` (disabled, llm), `search` (rrf_k, top-k, boosts, authority_boost), `graph`, `auto_update` (enabled, debounce_seconds, watch_sources, initial_sync, retry_failed), `scheduler.jobs` (поимённые job'ы с enabled/interval_seconds), `logging` (level/format/output), `paths` (data_dir, documents_dir, migrations_dir, global_config_path, prompts_path, onnx_config), `server` (name/version/host/port). Неизвестные ключи не ломают старт. Неизвестные значения строковых полей, не валидируемых оракулом (`logging.level/format/output`, `chunking.strategy`, `response_format`, `archive_format`, `source.type`, `attribute.type`), не ломают старт.

Новые поля (аддитивные, с дефолтами — обратно совместимы с пресетами без них):
- `ingestion.max_retries` (целое, default 3) — максимальное число автоматических повторов индексации проблемного документа фоновым worker'ом; после исчерпания документ получает статус `error` в очереди `document_jobs`.
- `auto_update.retry_failed` (объект, default `{ enabled: true, poll_interval_seconds: 60 }`) — включает фоновый пересмотр проблемных документов и задаёт период опроса очереди `document_jobs` в секундах.

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

#### Scenario: Отсутствие retry-полей
- **WHEN** в YAML нет `ingestion.max_retries` и `auto_update.retry_failed`
- **THEN** применяются дефолты max_retries=3, retry_failed.enabled=true, retry_failed.poll_interval_seconds=60 (обратная совместимость)

#### Scenario: Явная настройка retry
- **WHEN** в YAML задано `ingestion.max_retries: 5` и `auto_update.retry_failed.poll_interval_seconds: 120`
- **THEN** значения уважаются (фоновый worker повторяет до 5 раз с опросом очереди каждые 120 с)

### Requirement: Поле vectors.engine

The `vectors:` section (additive config-format extension, decision 2026-08-21) contains the optional `engine` field. The field SHALL accept only: absent (default) or `"usearch"`. The value `"lance"` SHALL be rejected with an explicit validation error stating that the lance engine was removed and that usearch is the only engine (`post-migration-lance-removal`, user decision 2026-08-31). Any other value SHALL be rejected with an explicit parse/validation error. The field does not affect other config sections and does not change the preset format.

#### Scenario: Отсутствие поля
- **WHEN** the preset contains a `vectors` section without the `engine` field
- **THEN** the usearch engine is used (the only engine)

#### Scenario: Явное значение
- **WHEN** the preset sets `vectors.engine: "usearch"`
- **THEN** `vectors` instantiates `UsearchEngine`

#### Scenario: Удалённый движок
- **WHEN** the preset sets `vectors.engine: "lance"`
- **THEN** the configuration is rejected with an explicit error naming the removal and pointing to usearch

#### Scenario: Невалидное значение
- **WHEN** the preset sets `vectors.engine: "foo"`
- **THEN** the configuration is rejected with an explicit error

### Requirement: Секция vectors.usearch

Секция `vectors:` дополняется опциональным объектом `usearch`, содержащим параметры двухслойной persistence UsearchEngine (ADR 0004): `max_segment_vectors` (usize, default 1000000), `compaction_stale_threshold` (u8, 1..=100, default 30), `search_threads` (usize, default 4). Поле необязательно: при отсутствии применяется `UsearchConfig::default()`. Невалидные значения (`max_segment_vectors: 0`, `compaction_stale_threshold` вне 1..=100, `search_threads: 0`) отклоняются на уровне парсинга с явной ошибкой.

#### Scenario: Отсутствие секции usearch
- **WHEN** пресет содержит секцию `vectors` без поля `usearch`
- **THEN** движок получает `UsearchConfig::default()` (1000000 / 30 / 4)

#### Scenario: Явная настройка usearch
- **WHEN** пресет задаёт `vectors.usearch.max_segment_vectors: 500000`
- **THEN** flush RAM-слоя выполняется при достижении 500000 векторов

#### Scenario: Невалидное значение
- **WHEN** пресет задаёт `vectors.usearch.max_segment_vectors: 0`
- **THEN** конфигурация отклоняется с явной ошибкой «must be > 0»

#### Scenario: Частичная настройка
- **WHEN** пресет задаёт `vectors.usearch.compaction_stale_threshold: 50` без остальных полей
- **THEN** `compaction_stale_threshold=50`, остальные — дефолты (1000000, 4)

### Requirement: Реестр моделей onnx.yaml

Формат `onnx.yaml` сохраняется: секция `runtime` (version, platforms[] — key/os/arch/archive_url/archive_format/library_name/library_path) и `models` (default, entries[] — name/display_name/description/version/vector_dim/files[name,url,size_bytes]). Поведение загрузки моделей (скачивание по url, проверка размера, хранение в data/) совпадает с оракулом.

#### Scenario: Реестр из оракула
- **WHEN** Rust-бинарь читает `../synopsis/configs/onnx.yaml`
- **THEN** список моделей и параметры рантайма распознаны идентично; model list выводит те же записи (machine-diff)

### Requirement: Обработка ошибок загрузки onnx.yaml

Загрузка реестра моделей из внешнего `onnx.yaml` завершается ошибкой с путём файла, если файл отсутствует или не парсится (как в оракуле).

#### Scenario: Отсутствующий onnx.yaml
- **WHEN** файл onnx.yaml не существует
- **THEN** загрузка завершается ошибкой с путём файла (как в оракуле)

### Requirement: Онтологии XML

Источники инжестии объявляются в `data/ontology/global.xml` + `domains/*.xml` (не в YAML). Правила оракула сохраняются: отсутствующий или некорректный domain-XML — ошибка старта; отсутствующий global.xml — не ошибка (пустой пул). Эффективная схема домена строится из определений домена и глобального пула (`<entity>`, `<relation>`, `<extraction>` в global.xml) как двух слоёв: lookup сначала ищет в домене, затем в глобальном слое (shadowing). Определение домена с ID, совпадающим с глобальным, переопределяет глобальное для этого домена без warning (**BREAKING** vs оракул: в Go — merge с warning). Валидация ссылок (relation → entity, ref-атрибуты) выполняется на объединении слоёв.

**Формат global.xml (BREAKING vs оракул, решение человека 2026-08-19):** каждая группа повторяющихся элементов — в plural-обёртке: `<entities><entity id= name= description=>` (с `<attributes><attribute name= type= required= target=>` и `<synonyms><synonym>`), `<relations><relation source= predicate= target= description=>` (с `<attributes>`), `<sources><source path= type= disabled= space= dataset=>` (с `<domains><domain>`), `<cross-domain-links>` (`<methods><method>`, `<equals><min-words>`, `<llm-confidence-threshold>`, `<batch-size>`, `<expressions><expression>` с `<name>/<description>/<priority>/<where>/<relation-type>`), `<ner>` (`<methods><method>`), `<extraction>` (`<regex-rules><regex id= entity= pattern= confidence=>`). Go-бинарь не парсит новый формат (encoding/xml молча пропускает незнакомые обёртки); parity — по семантике (дифференциальный harness на оригинальном формате).

#### Scenario: Некорректная онтология
- **WHEN** domains/ содержит синтаксически невалидный XML и бинарь стартует
- **THEN** старт завершается ошибкой с сообщением о файле онтологии (как в оракуле)

#### Scenario: Переопределение глобального пула
- **WHEN** entity в domain-файле имеет ID из глобального пула
- **THEN** для этого домена используется версия из домена; глобальная версия остаётся доступной для остальных доменов; warning не требуется (BREAKING: в оракуле — merge с warning)

#### Scenario: Отсутствующий global.xml
- **WHEN** data/ontology/ не содержит global.xml
- **THEN** старт не завершается ошибкой; глобальный пул пуст

#### Scenario: Валидация на объединении слоёв
- **WHEN** глобальное relation ссылается на entity, переопределённую доменом
- **THEN** ссылка разрешается в версию домена (рассинхрон Go-версии исключён)

### Requirement: Domain-XML онтологии

Каждый файл `domains/*.xml` описывает домен: `<domain name= version= description=>` с `<entities><entity id= name= description=>` (атрибуты `<attributes><attribute name= type= required= target=>`, синонимы `<synonyms><synonym>`), `<relations><relation source= predicate= target=>` (атрибуты), `<extraction><regex-rules><regex id= entity= pattern= confidence=>`, `<confidence auto_publish_threshold= review_threshold= reject_threshold=>`. Формат domain-XML — та же схема обёрток, что и global.xml (BREAKING vs оракул). Правила: отсутствующий domain-файл — ошибка старта; невалидный XML — ошибка старта; невалидный regex-паттерн — ошибка старта (компиляция при загрузке, как в оракуле).

#### Scenario: Невалидный regex
- **WHEN** domain-XML содержит regex с некомпилируемым pattern
- **THEN** старт завершается ошибкой с указанием файла и правила

#### Scenario: Полный домен
- **WHEN** Rust-бинарь читает `../synopsis/data/ontology/domains/domain_hr.xml`
- **THEN** entity/relation/extraction/confidence распознаны идентично оракулу (machine-diff)

