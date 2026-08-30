# Proposal: add-usearch-ann-engine

## Цель
Добавить `usearch` (v2.26.1, Apache-2.0, C++/cxx FFI) как **второй** ANN-движок в `crates/vectors`
за существующим трейтом `VectorIndex` (design D5), рядом с текущим `LanceEngine`. Выбор движка —
гибридный: Cargo-фичи для compile-time включения + runtime-поле конфига `vectors.engine` для
инстанцирования. Это позволяет собрать и сравнить ОБА движка (recall@k + p50/p95 через
parity-harness) до финального решения, какой оставить.

## Затрагиваемые замороженные контракты
- **config-format** (заморожен): добавляется аддитивное поле `vectors.engine` (`"lance"` | `"usearch"`)
  в секцию `vectors:`. Это изменение замороженного контракта, оформлено явным решением (см. design.md,
  пересмотр ADR 0003) и одобрено человеком (2026-08-29).
- **vector-index** (контракт): добавляется требование «Выбор ANN-движка»; семантика трейта
  `VectorIndex` НЕ меняется (методы, ранжирование L2, каскадный протокол D3 остаются).
- **MCP tools / CLI surface / data schema** — не затрагиваются.

## Подтверждение паритета
- Машинно через `parity-harness`: recall@k и p50/p95 для обоих движков на одних фикстурах
  (`fixtures/vectors.bin`, формат SYNX). Критерии решения (см. design.md): usearch recall@k в пределах
  2% от базовой lance, p50 в пределах 20%, p95 в пределах 30%; дельта размера бинаря < 5 МБ при
  сборке только usearch.
- Семантический паритет трейта проверяется юнит-тестами `UsearchEngine`, зеркальными тестам
  `LanceEngine` (create/insert/search/delete/rebuild/dim-mismatch/reopen).

## Non-goals
- НЕ меняем семантику трейта `VectorIndex` (без добавления `filtered_search` в трейт в этой фазе —
  пост-фильтр в search-крате сохраняется ради паритета).
- НЕ удаляем `lance`/`lancedb` в этом change. Удаление невыбранного движка — отдельный follow-up
  change после сравнения и решения человека.
- НЕ добавляем batch-API в usearch-биндинг (его нет в Rust-API); batch-insert делаем через rayon-пул
  поверх штучного `add`.
- НЕ меняем формат фикстур SYNX и не читаем векторы из старого vec0 (векторы пересобираются по тексту).

## Риски (кратко; подробно в design.md)
- cxx FFI должен кросс-компилироваться на 5 CI-таргетах (windows-gnu, darwin) — снимается spike (1.1).
- u8-квантование usearch может дать recall@k чуть ниже u8-SQ lance — бенчмарк покажет; порог 2%.
- `unsafe_code` ослабляется только в `crates/vectors` (разрешено AGENTS.md для native-seam FFI).
