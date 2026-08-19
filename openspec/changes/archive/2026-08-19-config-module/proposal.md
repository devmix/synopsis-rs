## Why

`crates/config` — единственный листовой крейт (tier 0) в графе зависимостей, но сейчас это пустой стаб. Без него не стартуют ни CLI (YAML-пресеты), ни embedding (реестр onnx.yaml), ни graph (онтологии XML). Это первый полноценный модуль миграции: он задаёт API-стиль (serde + enum'ы + thiserror) для всех последующих крейтов.

## What Changes

- Полная реализация `crates/config`: YAML-пресеты (`config.{preset}.yaml`), реестр моделей `onnx.yaml`, онтологии XML (`global.xml` + `domains/*.xml`).
- API: serde `Deserialize` + `apply_defaults()` + `validate()`; enum'ы вместо строк — строгие там, где Go валидирует (`embeddings.mode`, NER/link-методы), толерантные (`Unknown(String)`) там, где Go пропускает любые строки (`chunking.strategy`, `logging.*`, `response_format`, `archive_format`, `source.type`, `attribute.type`). Форматы файлов не меняются (serde rename).
- Новые зависимости в палитру: `thiserror` (ошибки), `regex` (компиляция extraction-правил при загрузке, как в Go); XML — `quick-xml` (поддержка записи на будущее). Все три — с онлайн-проверкой версий/MSRV/CVE реализатором.
- **BREAKING (поведенческое, не форматное)**: глобальный пул онтологии — слои вместо merge. В Go `global.xml` парсится дважды (config + domain), пул мержится в каждый домен с warning'ом при переопределении. В Rust: один парсер, эффективная схема домена = домен + глобальный слой (lookup: домен → глобальный), без мутаций и warnings. Сценарий спека «переопределение с warning» меняется на «shadowing». Решение человека 2026-08-18 (вариант B).
- Спек `config-format` дополняется: секции `scheduler`, `linker`, `auto_update`, `paths.onnx_config`, структура `domains/*.xml`, сценарий shadowing.

## Capabilities

### New Capabilities

- (нет — контрактный спек `config-format` уже существует)

### Modified Capabilities

- `config-format`: расширение требований (недостающие секции YAML-пресета, структура domain-XML) и изменение сценария «Переопределение глобального пула»: warning-merge → явный shadowing (BREAKING, решение человека).

## Impact

- Код: `crates/config/src/*` (lib.rs, yaml-пресеты, onnx, ontology), `crates/config/Cargo.toml`, палитра `Cargo.toml` (thiserror, regex), фикстуры `crates/config/tests/data/` (копии из оракула, provenance в README).
- Спеки: `openspec/specs/config-format/spec.md` (дельта в этом change, синк при архивации).
- Go-референсы: `../synopsis/internal/config/config.go`, `global_config.go`, `../synopsis/internal/domain/domain_config.go`, `global_pool.go` (+ их тесты), `../synopsis/configs/*.yaml`, `../synopsis/data/ontology/*.xml`.
- Замороженные контракты: затрагивается только `config-format` (см. BREAKING выше); MCP tools / CLI surface / data schema не затрагиваются. Паритет форматов — фикстуры-копии + юнит-тесты; machine-diff против Go-бинаря — позже, в parity-harness.

## Non-goals

- НЕ реализуем registry/merge-логику пула и shadowing-lookup (это change `graph`; здесь — только парсинг и валидация файлов).
- НЕ реализуем скачивание/проверку моделей ONNX (change `embedding`; здесь — только структура реестра).
- НЕ трогаем CLI-флаги и resolution пресетов (change `cli`).
- НЕ меняем форматы файлов — только внутреннее API-представление (enum'ы).
- НЕ пишем parity-harness тесты против Go-бинаря (отдельный change).