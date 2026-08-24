# ingestion-sources — Design

Первый change серии ingestion (решение человека 2026-08-23: 3 change'а — sources → ner → pipeline). Оракул: `internal/ingestion/{types.go, sources/, parsers/, chunkers/}` — референс по поведению, не по коду.

## D1. Трейты и композит Source

**Решение:** `trait Parser { fn parse(&self, path) -> ParseResult; fn supported_extensions(&self) -> &[&str] }`, `trait Chunker { fn chunk(&self, content, metadata) -> Result<Vec<DocumentChunk>> }`, `trait Source: Parser + Chunker` (supertrait-композит — семантика оракула «самодостаточная единица»). ParseResult несёт документы И нефатальные ошибки вместе (контракт оракула сохранён).
**Почему так:** зеркалит архитектуру оракула, но в идиоматичных трейтах; реестр работает с `dyn Source`.

## D2. DocumentChunk без NerResult (осознанное отклонение)

**Решение:** чанк = {doc_id (заполняется на этапе записи, change 3), text, sequence_num, start_offset/end_offset (byte), metadata}. Поля NerResult в структуре НЕТ.
**Почему не альтернатива:** оракул встраивает `*ner.Result` прямо в чанк — связность этапов через структуру данных. В Rust NER-этап (change 2) присоединяет результаты своей структурой (параллельные списки/карта по chunk index); чанк остаётся чистым артефактом чанкинга. Это упрощает change 1 (нет зависимости от NER-типов) и тестирование.
**Референс:** internal/ingestion/chunkers/chunker.go DocumentChunk (поля сверены; NerResult исключён сознательно).

## D3. Порядок форматов (решение человека 2026-08-23)

markdown + json первыми (основные по конфигурации и testdata оракула); mediawiki, webpage, unstructured — следом в том же change'е по одному паттерну. Каждый формат = пара parser+chunker + регистрация в реестре.

## D4. Чанкинг markdown — structure-aware

**Решение:** границы по секциям заголовков; max_chunk_size (default 1000) и overlap_size (default 100) из `chunking.markdown`; **configured overlap 0 сохраняется** (баг-паттерн Go `< 0` vs `<= 0` уже зафиксирован в config-крейте — здесь то же правило применения). Метаданные чанка: заголовок секции. Смещения — byte-офсеты исходного текста (Rust UTF-8 строки делают их естественными; вырезка `&text[start..end]` воспроизводит чанк).
**Референс:** internal/ingestion/chunkers/markdown_chunker.go (+тесты — там зафиксированы ожидания структуры).

## D5. Реестр источников

**Решение:** тип источника из global.xml (`<source type=…>`) → реализация Source; расширения файлов — второй механизм диспетчеризации внутри парсера. Неизвестный тип/расширение — явная ошибка, не тихий пропуск.
**Референс:** internal/ingestion/sources/registry.go.

## D6. Паритет-фикстуры

**Решение:** testdata оракула (internal/ingestion/runner/testdata и файлы тестов парсеров) копируются в fixtures крейта; ожидания (число чанков, тексты) фиксируются из Go-реализаций/тестов оракула. Без сети.
**Референс:** AGENTS.md («Паритет проверяется машиной»).

## Отклонения от оракула (осознанные)

- DocumentChunk без NerResult (D2).
- Трейты вместо Go-интерфейсов с map[string]interface{} метаданными → типизированная Metadata-структура там, где возможно, + расширяемое поле (решает задачи 1.2–1.9; детали — в телах задач).
