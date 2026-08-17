## Purpose

Форматы конфигурационных файлов Synopsis: YAML-пресеты (`config.{preset}.yaml`), реестр моделей `onnx.yaml`, онтологии XML в `data/ontology/`. Фиксирует совместимость с существующими файлами из `../synopsis/configs/` и `../synopsis/data/ontology/`: пользователь не должен менять конфиги при переходе на Rust-бинарь.

## ADDED Requirements

### Requirement: YAML-пресеты конфигурации

Rust-бинарь читает те же `config.{preset}.yaml`, что и Go оригинал: имена ключей, значения по умолчанию и структура секций (`database` — path/pragma; `embeddings` — mode local|api с подсекциями model_name/vector_dim/base_url/api_key/max_retries/timeout_ms; `ingestion.chunking.*`; серверные настройки; пути к onnx-конфигу и промптам) идентичны. Неизвестные ключи не ломают старт (как в оракуле).

#### Scenario: Существующий пресет
- **WHEN** Rust-бинарь стартует с `../synopsis/configs/config.default.yaml` без изменений
- **THEN** конфиг parsed успешно, значения по умолчанию совпадают с Go бинарём на том же файле (machine-diff effective config)

### Requirement: Реестр моделей onnx.yaml

Формат `onnx.yaml` сохраняется: секция `runtime` (version, platforms[] — key/os/arch/archive_url/archive_format/library_name/library_path) и `models` (default, entries[] — name/display_name/description/version/vector_dim/files[name,url,size_bytes]). Поведение загрузки моделей (скачивание по url, проверка размера, хранение в data/) совпадает с оракулом.

#### Scenario: Реестр из оракула
- **WHEN** Rust-бинарь читает `../synopsis/configs/onnx.yaml`
- **THEN** список моделей и параметры рантайма распознаны идентично; model list выводит те же записи (machine-diff)

### Requirement: Онтологии XML

Источники инжестии объявляются в `data/ontology/global.xml` + `domains/*.xml` (не в YAML). Правила оракула сохраняются: отсутствующий или некорректный domain-XML — ошибка старта; сущность домена с ID, совпадающим с глобальным пулом, переопределяет его (с warning в лог).

#### Scenario: Некорректная онтология
- **WHEN** domains/ содержит синтаксически невалидный XML и бинарь стартует
- **THEN** старт завершается ошибкой с сообщением о файле онтологии (как в оракуле)

#### Scenario: Переопределение глобального пула
- **WHEN** entity в domain-файле имеет ID из global.xml
- **THEN** используется версия из домена, в лог записан warning (поведение совпадает с оракулом)
