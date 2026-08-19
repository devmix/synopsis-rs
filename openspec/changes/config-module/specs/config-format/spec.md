# config-format Delta

## MODIFIED Requirements

### Requirement: Онтологии XML

Источники инжестии объявляются в `data/ontology/global.xml` + `domains/*.xml` (не в YAML). Правила оракула сохраняются: отсутствующий или некорректный domain-XML — ошибка старта; отсутствующий global.xml — не ошибка (пустой пул). Эффективная схема домена строится из определений домена и глобального пула (`<entity>`, `<relation>`, `<extraction>` в global.xml) как двух слоёв: lookup сначала ищет в домене, затем в глобальном слое (shadowing). Определение домена с ID, совпадающим с глобальным, переопределяет глобальное для этого домена без warning (**BREAKING** vs оракул: в Go — merge с warning). Валидация ссылок (relation → entity, ref-атрибуты) выполняется на объединении слоёв.

**Формат global.xml (BREAKING vs оракул, решение человека 2026-08-19):** контейнеры-обёртки списков `<entities>/<relations>/<sources>/<domains>/<expressions>` убраны — повторяющиеся элементы являются прямыми детьми: `<entity>`/`<relation>`/`<source>` под `<global>`, `<domain>` под `<source>`, `<expression>` под `<cross-domain-links>`. Контейнеры под-структур сохранены: `<cross-domain-links>` (`<method>`, `<equals><min-words>`, `<llm-confidence-threshold>`, `<batch-size>`, `<expression>` с `<name>/<description>/<priority>/<where>/<relation-type>`), `<ner>` (`<method>`), `<extraction>` (`<regex id= entity= pattern= confidence=>`), `<equals>`. Атрибуты: `<source path= type= disabled= space= dataset=>`, `<entity id= name= description=>` с `<attribute name= type= required= target=>` и `<synonym>`, `<relation source= predicate= target= description=>` с `<attribute>`. Go-бинарь не парсит новый формат; parity — по семантике (дифференциальный harness на оригинальном формате).

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

## ADDED Requirements

### Requirement: Полный YAML-пресет

Rust-бинарь читает те же `config.{preset}.yaml`, что и Go оригинал. Пресет включает секции: `database` (path, pragma), `embeddings` (mode local|api), `ingestion` (chunking.markdown/json, ner.prose/llm, batch_size, resolver), `linker` (disabled, llm), `search` (rrf_k, top-k, boosts, authority_boost), `graph`, `auto_update` (enabled, debounce_seconds, watch_sources, initial_sync), `scheduler.jobs` (поимённые job'ы с enabled/interval_seconds), `logging` (level/format/output), `paths` (data_dir, documents_dir, migrations_dir, global_config_path, prompts_path, onnx_config), `server` (name/version/host/port). Неизвестные ключи не ломают старт. Неизвестные значения строковых полей, не валидируемых оракулом (`logging.level/format/output`, `chunking.strategy`, `response_format`, `archive_format`, `source.type`, `attribute.type`), не ломают старт.

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

### Requirement: Domain-XML онтологии

Каждый файл `domains/*.xml` описывает домен: `<domain name= version= description=>` с `<entity id= name= description=>` (атрибуты `<attribute name= type= required= target=>`, синонимы `<synonym>`), `<relation source= predicate= target=>` (атрибуты), `<extraction><regex id= entity= pattern= confidence=>`, `<confidence auto_publish_threshold= review_threshold= reject_threshold=>`. Формат domain-XML НЕ меняется (entity/relation — прямые дети `<domain>`, как в оракуле). Правила: отсутствующий domain-файл — ошибка старта; невалидный XML — ошибка старта; невалидный regex-паттерн — ошибка старта (компиляция при загрузке, как в оракуле).

#### Scenario: Невалидный regex
- **WHEN** domain-XML содержит regex с некомпилируемым pattern
- **THEN** старт завершается ошибкой с указанием файла и правила

#### Scenario: Полный домен
- **WHEN** Rust-бинарь читает `../synopsis/data/ontology/domains/domain_hr.xml`
- **THEN** entity/relation/extraction/confidence распознаны идентично оракулу (machine-diff)

### Requirement: Обработка ошибок загрузки onnx.yaml

Загрузка реестра моделей из внешнего `onnx.yaml` завершается ошибкой с путём файла, если файл отсутствует или не парсится (как в оракуле).

#### Scenario: Отсутствующий onnx.yaml
- **WHEN** файл onnx.yaml не существует
- **THEN** загрузка завершается ошибкой с путём файла (как в оракуле)