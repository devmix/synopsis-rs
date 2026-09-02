# parsing-and-chunking Specification

## Purpose

Разбор документов источников: обнаружение файлов по расширениям, извлечение контента (markdown, json, mediawiki, webpage, unstructured), structure-aware чанкинг с офсетами и метаданными — вход конвейера ингестии.

## Requirements

### Requirement: Трейты разбора

Крейт `ingestion` определяет трейты: `Parser` — обход исходного пути и извлечение документов (результат содержит документы И нефатальные ошибки разбора вместе); `Chunker` — разбиение контента на чанки; `Source` — композит Parser+Chunker для одного типа источника («самодостаточная единица ингестии»). Чанк несёт текст, порядковый номер в документе, byte-офсеты начала/конца в исходном тексте и метаданные; NER-результат в чанке не хранится (осознанное отклонение от оракула — NER присоединяется отдельным этапом).

#### Scenario: Обход каталога источников
- **WHEN** парсер вызывается на пути источника
- **THEN** возвращаются все документы подходящих расширений; ошибки отдельных файлов собраны в результат, не прерывая обход

#### Scenario: Чанк несёт офсеты
- **WHEN** контент разбит на чанки
- **THEN** каждый чанк имеет sequence_num по порядку и byte-офсеты, вырезка из которых по исходному тексту даёт текст чанка

### Requirement: Markdown-источник

Markdown-парсер обнаруживает файлы `.md`/`.markdown` и извлекает их содержимое. Markdown-чанкер делит документ structure-aware — по секциям заголовков — с ограничением максимального размера чанка и перекрытием из конфигурации (`chunking.markdown.max_chunk_size`, default 1000; `overlap_size`, default 100; configured 0 для overlap сохраняется). Метаданные чанка включают заголовок секции.

#### Scenario: Секционный чанкинг
- **WHEN** markdown-документ с несколькими заголовками делится на чанки
- **THEN** границы чанков следуют структуре заголовков; размер уважает max_chunk_size; соседние чанки перекрываются на overlap_size

#### Scenario: Нулевой overlap
- **WHEN** overlap_size сконфигурирован как 0
- **THEN** чанки не перекрываются (0 — валидное значение, не заменяется дефолтом)

### Requirement: JSON-источник

JSON-парсер обрабатывает `.json`-файлы согласно семантике оракула (проверяется по его реализации и тестам). JSON-чанкер делит содержимое по структуре документа.

#### Scenario: Разбор JSON-источника
- **WHEN** json-источник парсится и чанкуется
- **THEN** документы извлечены, чанки покрывают содержимое без потери данных

### Requirement: Дополнительные форматы

Источники mediawiki, webpage и unstructured реализуют те же трейты по семантике оракула (каждый — свой формат обнаружения и извлечения).

#### Scenario: Единый паттерн форматов
- **WHEN** добавляется источник нового формата
- **THEN** он реализует тот же трейт Source и регистрируется в реестре без изменений потребляющего кода

### Requirement: Реестр источников

Реестр сопоставляет тип источника (из конфигурации global.xml `<source type=…>`) и расширения файлов с реализацией Source; неизвестный тип — явная ошибка.

#### Scenario: Выбор источника по типу
- **WHEN** запрашивается источник по типу из конфигурации
- **THEN** возвращается зарегистрированная реализация; неизвестный тип даёт явную ошибку

### Requirement: Паритет с оракулом

Разбор и чанкинг проверяются дифференциально против фикстур оракула: одинаковый вход даёт одинаковое число чанков, идентичный текст чанков и согласованные офсеты.

#### Scenario: Дифференциальный тест
- **WHEN** фикстура оракула прогоняется через Rust-парсер и чанкер
- **THEN** число чанков и их текст совпадают с ожиданиями, зафиксированными из Go-бинаря

### Requirement: search_text emission
The Markdown chunker emits, for every chunk, a `search_text` value in addition to the invariant-preserving `text`. `search_text` is the chunk's heading breadcrumb (the multi-line heading path, e.g. `> H1` / ` > H2`) followed by a blank line and the chunk body; when the chunk has no breadcrumb (e.g. the preamble before the first heading) `search_text` equals `text`. The `text` field and the byte-offset invariant (`content[start_offset..end_offset] == text`) are unchanged, and the breadcrumb is still also available in the chunk metadata.

#### Scenario: Chunk under a heading
- **WHEN** a Markdown chunk is produced under a non-empty heading hierarchy
- **THEN** its `search_text` is the breadcrumb followed by the body, while its `text` remains the pure body slice

#### Scenario: Chunk with no heading
- **WHEN** a Markdown chunk has no heading breadcrumb
- **THEN** its `search_text` is equal to its `text`

#### Scenario: Invariant unaffected
- **WHEN** any chunk is produced
- **THEN** `content[start_offset..end_offset] == text` still holds (only `search_text` carries the synthetic breadcrumb)
