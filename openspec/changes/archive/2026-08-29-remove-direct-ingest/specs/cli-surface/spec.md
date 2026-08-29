# cli-surface Specification

## REMOVED Requirements

### Requirement: Подкоманда sync

Удалена (2026-08-29). Полный re-ingest источников теперь выполняется только через
очередь `document_jobs`: `synopsis db clear` очищает БД датасета, а последующий
старт `serve` (startup reconcile) заново ставит все файлы в очередь, которую
обрабатывает фоновый worker. Прямой ингест (`Runner::ingest_all`) удалён — очередь
является единственным путём обработки документов.

## MODIFIED Requirements

### Requirement: Глобальные флаги и структура командной строки

Формат вызова: `synopsis [--config PATH] [--preset NAME] [--db PATH] [--version] <subcommand> [flags...]`. Глобальные флаги предшествуют подкоманде; per-command флаги следуют за ней. Подкоманды: `serve`, `model`, `onnx-runtime`, `load-test`, `queue`, `db`.

> **Решение по контракту (2026-08-29):** подкоманда `sync` удалена (см. REMOVED
> Requirements); добавлена подкоманда `db` с действием `clear`. Полный re-ingest —
> через `db clear` + restart `serve` (startup reconcile), а не прямым ингестом.

#### Scenario: Порядок аргументов
- **WHEN** вызвать `synopsis --preset default serve --port 9090`
- **THEN** сервер стартует на порту 9090 (глобальный preset учтён, per-command флаг после подкоманды)

#### Scenario: Version
- **WHEN** вызвать `synopsis --version`
- **THEN** печатается версия и процесс завершается с кодом 0

### Requirement: Подкоманда serve

Единственный долгоживущий режим: startup reconcile (enqueue diff в `document_jobs`) + MCP over HTTP (Streamable HTTP, design D8) + file watching (enqueue diff). Флаги: `--no-initial-sync` (пропустить startup reconcile на старте), `--port N` (по умолчанию 8080, переопределяет server.port из конфига), `--auto-rebuild-vectors`. При несовпадении размерности векторного движка serve очищает БД датасета (`clear_dataset`) и startup reconcile заново ставит все файлы в очередь — worker пере-эмбеддит их (clear-then-queue, без прямого ингеста).

#### Scenario: Порт
- **WHEN** serve --port 9123
- **THEN** HTTP-сервер слушает на 9123, GET /health отвечает 200

## ADDED Requirements

### Requirement: Подкоманда db

Новая подкоманда для обслуживания БД датасета. Подкоманды: `db stats` — выводит
статистику по датасету и knowledge DB (число documents, chunks, entities,
entity_links, facts, queue-заданий), read-only, без изменений и без подтверждения;
`db clear` — выводит ту же статистику, запрашивает подтверждение (`Confirm
deletion? [y/N]` на stdin) и при ответе `y`/`Y` удаляет ВЕСЬ каталог состояния
датасета (`<workspace_dir>/datasets/<name>/state`, содержащий `knowledge.db` и
Lance-индекс векторов `vectors/`) целиком с диска (`std::fs::remove_dir_all`,
игнорируя отсутствие каталога). Это атомарно удаляет и SQLite-БД, и векторы за
один вызов. Команда one-shot, не загружает embedding-модель / ONNX (открывает
только dataset-bound БД для вывода статистики, затем закрывает его перед удалением).
После `db clear` нужен restart `serve`, чтобы startup reconcile заново поставил
файлы в очередь, а фоновый worker пере-эмбеддил документы и пересоздал БД + Lance-индекс.

#### Scenario: Stats
- **WHEN** вызвать `synopsis db stats`
- **THEN** выводятся counts по documents/chunks/entities/entity_links/facts/queue; БД не меняется

#### Scenario: Clear with confirmation
- **WHEN** вызвать `synopsis db clear` и ответить `y`
- **THEN** весь каталог состояния датасета (knowledge.db + vectors/) удаляется с диска; выводится итог очистки

#### Scenario: Clear aborted
- **WHEN** вызвать `synopsis db clear` и ответить `n` (или любой не-`y` ввод)
- **THEN** удаление не выполняется, команда завершается без изменений БД

#### Scenario: Stats shown before prompt
- **WHEN** вызвать `synopsis db clear`
- **THEN** до запроса подтверждения выводятся counts по documents/chunks/entities/entity_links/facts/queue
