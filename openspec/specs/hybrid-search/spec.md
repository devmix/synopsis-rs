# hybrid-search Specification

## Purpose

Гибридный поиск знаний: лексическая нога (FTS5/BM25), семантическая нога (векторное сходство), Reciprocal Rank Fusion с BM25-калибровкой, обогащение, реранкинг бизнес-правилами и графовое расширение.

## Requirements

### Requirement: Гибридный поиск

Крейт search предоставляет трейт `Searcher` с тремя методами: `hybrid_search` (оба суб-поиска → RRF-фьюжн), `lexical_search` (FTS5/BM25), `semantic_search` (векторное сходство). Пустой запрос возвращает пустой результат; при отказе обоих суб-поисков — ошибка с обеими причинами; при отказе одного — работа продолжается на уцелевших результатах. Финальный конвейер (enrich → rerank → truncate → expand) выполняется на всех путях, включая одиночные ноги.

#### Scenario: Отказ одного суб-поиска
- **WHEN** лексический поиск упал, семантический успешен
- **THEN** возвращаются результаты семантического поиска без ошибки

#### Scenario: Отказ обоих суб-поисков
- **WHEN** оба суб-поиска вернули ошибки
- **THEN** гибридный поиск завершается ошибкой, упоминая обе причины

### Requirement: Reciprocal Rank Fusion

Фьюжн сливает два ранжированных списка: score += 1/(k + rank) по каждому списку (k default 20); BM25-оценки нормализуются min-max только по записям лексического списка (семантически-единственные получают нейтральную 0.5); RRF-оценки нормализуются в [0,1]; итог 0.7·rrf + 0.3·bm25; сортировка по убыванию счёта с детерминированным tiebreak по возрастанию chunk_id; тип результата: lexical | semantic | hybrid.

#### Scenario: Чанк в обоих списках
- **WHEN** чанк найден и лексическим, и семантическим поиском
- **THEN** его RRF-счёт суммируется из обоих списков и тип помечается hybrid

#### Scenario: Детерминированный порядок
- **WHEN** два чанка имеют равный финальный счёт
- **THEN** выше стоит чанк с меньшим chunk_id

### Requirement: Фильтрация по домену

Доменная фильтрация выполняется внутри суб-поисков (лексическая — на уровне SQL, семантическая — на стороне приложения с переизбытком выборки ×3), до фьюжна и усечения: до topK результатов выживают в запрошенном домене. Сравнение доменов нормализовано (регистр/пробелы).

#### Scenario: Доменный фильтр до усечения
- **WHEN** hybrid_search вызывается с доменом и topK
- **THEN** возвращается до topK результатов из этого домена, а не меньше из-за пост-фильтрации

### Requirement: Обогащение результатов

Enricher батчево добавляет к результатам: путь документа, объединённый тип (поиск+документ), updated_at в RFC3339 (принимает RFC3339 и формат SQLite CURRENT_TIMESTAMP), флаги реранкера (is_deprecated/is_official/valid_to) из метаданных документа, список доменов; сущности чанка прикрепляются одним батчевым запросом.

#### Scenario: Батчевое обогащение
- **WHEN** enriched пул содержит результаты нескольких документов
- **THEN** документы и сущности запрашиваются пакетно, без N+1

### Requirement: Реранкинг

Reranker применяет бизнес-правила (deprecated ×0.2, official ×1.5, истёкший valid_to ×0.1 — множители компонуются), freshness-буст для документов обновлённых в пределах recent_days (×recent_boost), authority-буст по типу документа из конфигурационной карты; затем пересортировка по убыванию счёта и перенумерация рангов. Значения по умолчанию: 0.2/1.5/1.2/90; конфиг переопределяет только положительные значения.

#### Scenario: Композиция бустов
- **WHEN** документ одновременно deprecated и official
- **THEN** счёт умножается на оба фактора (0.2 × 1.5)

#### Scenario: Перенумерация рангов
- **WHEN** бусты меняют порядок результатов
- **THEN** ранги переприсваиваются согласно новому порядку после усечения topK

### Requirement: Графовое расширение

Если включено конфигурацией и граф предоставлен: сущности результатов расширяются BFS в обе стороны (max_depth/max_nodes), одобренные факты загружаются батчево; рёбра и факты сериализуются в metadata.related_entities. Ошибки расширения не фатальны — результаты возвращаются без графового контекста.

#### Scenario: Незначимый сбой расширения
- **WHEN** обход графа падает
- **THEN** поиск возвращает обогащённые результаты без related_entities, без ошибки

### Requirement: Паритет поиска

RRF-фьюжн, реранкер и обогащение проверяются против записанных фикстур: калибровочные константы (k=20, 0.7/0.3), нормализации, бусты и порядок дают те же значения, что зафиксированы в записанных кейсах; сквозные сценарии прогоняются через реальную FTS5 с просчитанными вручную ожиданиями.

#### Scenario: Прогон записанных кейсов
- **WHEN** кейсы (rrf, enricher, reranker, graph expansion) прогоняются через Rust-реализацию
- **THEN** счёты, порядки и метаданные совпадают с записанными фикстурами

### Requirement: Search legs consume search_text
Both search legs SHALL be fed the chunk's `search_text` for **matching**: the lexical leg matches the FTS5 index (built over `search_text`) and the semantic leg compares the query embedding against chunk embeddings computed from `search_text`. The fused, ranked result's `text` field SHALL be the chunk's pure `chunk_text` (the byte-offset slice), and the chunk's metadata bag (`section_title`, `heading_level`, `breadcrumb`, `image_paths`, …) SHALL be carried on the result as structured `metadata` — the section context that was previously glued into the text. The fusion (RRF), reranking, and enrichment are otherwise unchanged.

#### Scenario: Lexical leg sees heading terms
- **WHEN** a query term appears in a chunk's heading breadcrumb but not in its body
- **THEN** the lexical leg can match that chunk (the FTS index is over `search_text`)

#### Scenario: Semantic leg embeds sectioned text
- **WHEN** the semantic leg ranks chunks for a query
- **THEN** it compares against embeddings computed from `search_text` (breadcrumb + body)

#### Scenario: Result text carries section context
- **WHEN** a chunk is returned in a search result
- **THEN** the result's `text` field is the chunk's pure `chunk_text` (the byte-offset slice), and the section context (breadcrumb, section title) is carried in the result's `metadata` field, not glued into the text

#### Scenario: Result carries the chunk metadata
- **WHEN** a chunk is returned in a search result
- **THEN** the result's `metadata` carries the chunk's own metadata bag (`section_title`, `heading_level`, `breadcrumb`, …); a chunk with no metadata carries an empty bag
