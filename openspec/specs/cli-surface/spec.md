# cli-surface Specification

## Purpose

Внешний контракт CLI Synopsis: подкоманды, флаги, порядок аргументов, поведение. Паритет проверяется machine-diff'ом usage/`--help` выводов и поведенческими сценариями.
## Requirements
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

### Requirement: Подкоманды model и onnx-runtime

`model` управляет реестром моделей (list/benchmark); `onnx-runtime` управляет загрузкой/состоянием ONNX Runtime. Флаги и вывод зафиксированы этим контрактом.

#### Scenario: Паритет usage
- **WHEN** выполнить machine-diff выводов `model --help` / `onnx-runtime --help` против записанных фикстур
- **THEN** набор флагов, значения по умолчанию и тексты описаний идентичны (допускается нормализация форматирования)

### Requirement: Подкоманда load-test

Бенчмарк MCP-инструментов на сгенерированных данных. Флаги: `--scale small|medium|large` (default small), `--iterations N` (default 100), `--json PATH`, `--no-fill` (бенчить существующую БД). Отчёт: CALLS, AVG/P50/P95/P99/MAX ms, QPS по каждому tool-кейсу; человекочитаемый отчёт в stdout, при --json — тот же отчёт JSON.

#### Scenario: Отчёт
- **WHEN** load-test --scale small завершён
- **THEN** stdout содержит таблицу задержек по кейсам, структура совпадает с записанной фикстурой отчёта (machine-diff по секциям)

### Requirement: Резолвинг конфигурации

Приоритет: `--config` > `config.{preset}.yaml`, где preset по умолчанию `default`; авто-поиск файла конфига — сначала относительно каталога исполняемого файла, затем CWD. Поведение при отсутствии конфига и текст ошибок зафиксированы этим контрактом (с учётом того, что Rust-бинарь лежит в своём каталоге).

#### Scenario: Пресет
- **WHEN** рядом с бинарью лежит config.prod.yaml и вызов `synopsis --preset prod serve`
- **THEN** используется config.prod.yaml без явного --config

### Requirement: Подкоманда queue

Новая подкоманда для инспекции и обслуживания очереди индексации документов (`document_jobs`). Подкоманды: `queue status [--source PATH] [--status NAME]` — табличный вывод состояния заданий (колонки: path, source, status, attempts, last_error, next_attempt_at); `queue reset-retries [--source PATH] [--path PATH]` — сброс счётчика повторов для заданий в статусе `error` (status→`pending`, attempts→0, next_attempt_at→now), после чего фоновый worker переиндексирует их. Подкоманда аддитивна к существующим (`serve`, `db`, `model`, `onnx-runtime`, `load-test`).

> **Решение по контракту (2026-08-29):** подкоманда изначально проектировалась как `index`, но переименована в `queue` человеком — имя `queue` точнее отражает сущность (единая таблица состояний `document_jobs`, разделяемая watcher/startup/worker), а не процесс индексации. Это отклонение от исходного наименования в задаче 1.7 зафиксировано явно.

#### Scenario: Status
- **WHEN** вызвать `synopsis queue status`
- **THEN** выводится таблица всех заданий очереди `document_jobs` с колонками path/status/attempts/last_error/next_attempt_at; фильтр `--source` сужает вывод до одного источника, `--status` — до одного статуса

#### Scenario: Reset retries
- **WHEN** вызвать `synopsis queue reset-retries --path workspace/datasets/edtech/ontology/../content/documents/product/adaptive_learning_prd.md`
- **THEN** задание переходит в `pending` с attempts=0; следующий цикл фонового worker переиндексирует документ (строка больше не в статусе `error`)

### Requirement: Подкоманда db

A subcommand for dataset database maintenance. It SHALL provide two actions: `db stats` — prints dataset and knowledge-DB statistics (document, chunk, entity, entity_link, fact and queue-job counts), read-only, no modification and no confirmation; `db clear` — prints the same statistics, asks for confirmation (`Confirm deletion? [y/N]` on stdin) and on `y`/`Y` deletes the entire dataset state directory (`<workspace_dir>/datasets/<name>/state`, containing `knowledge.db` and the vector index directory `vectors/`) from disk (`std::fs::remove_dir_all`, ignoring a missing directory). This atomically removes both the SQLite DB and the vectors in one call. The command is one-shot and does not load the embedding model / ONNX (it opens only the dataset-bound DB to print statistics, then closes it before deletion). After `db clear` a `serve` restart is required so the startup reconcile re-enqueues the files and the background worker re-embeds the documents and recreates the DB + vector index.

#### Scenario: Stats
- **WHEN** `synopsis db stats` is invoked
- **THEN** counts for documents/chunks/entities/entity_links/facts/queue are printed; the DB is not modified

#### Scenario: Clear with confirmation
- **WHEN** `synopsis db clear` is invoked and the answer is `y`
- **THEN** the whole dataset state directory (knowledge.db + vectors/) is removed from disk; a cleanup summary is printed

#### Scenario: Clear aborted
- **WHEN** `synopsis db clear` is invoked and the answer is `n` (or any non-`y` input)
- **THEN** no deletion happens; the command exits without modifying the DB

#### Scenario: Stats shown before prompt
- **WHEN** `synopsis db clear` is invoked
- **THEN** before the confirmation prompt, counts for documents/chunks/entities/entity_links/facts/queue are printed

