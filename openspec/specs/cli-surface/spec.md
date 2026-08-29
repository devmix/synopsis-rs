# cli-surface Specification

## Purpose

Внешний контракт CLI Synopsis: подкоманды, флаги, порядок аргументов, поведение. Источник истины — `../synopsis/cmd/app/main.go` (+ cmd.go, model_cmd.go, onnx_runtime.go, serve.go, sync.go, loadtest.go). Паритет проверяется machine-diff'ом usage/`--help` выводов и поведенческими сценариями.
## Requirements
### Requirement: Глобальные флаги и структура командной строки

Формат вызова: `synopsis [--config PATH] [--preset NAME] [--db PATH] [--version] <subcommand> [flags...]`. Глобальные флаги предшествуют подкоманде; per-command флаги следуют за ней. Подкоманды: `sync`, `serve`, `model`, `onnx-runtime`, `load-test`.

#### Scenario: Порядок аргументов
- **WHEN** вызвать `synopsis --preset default serve --port 9090`
- **THEN** сервер стартует на порту 9090 (глобальный preset учтён, per-command флаг после подкоманды)

#### Scenario: Version
- **WHEN** вызвать `synopsis --version`
- **THEN** печатается версия и процесс завершается с кодом 0

### Requirement: Подкоманда sync

Одноразовый синхронизационный прогон. Флаги: `--rebuild` (очистить все данные перед реиндексацией), `--auto-rebuild-vectors` (пересобрать векторы при несовпадении размерности). Завершается кодом 0 при успехе, ненулевым при ошибке; прогресс печатается в stdout.

#### Scenario: Rebuild
- **WHEN** sync --rebuild на непустой БД
- **THEN** данные очищены и переиндексированы заново (счётчики документов/чанков после прогона соответствуют источнику)

### Requirement: Подкоманда serve

Единственный долгоживущий режим: initial sync + MCP over HTTP (Streamable HTTP, design D8) + file watching. Флаги: `--no-initial-sync` (пропустить полный scan источников на старте), `--port N` (по умолчанию 8080, переопределяет server.port из конфига), `--auto-rebuild-vectors`.

#### Scenario: Порт
- **WHEN** serve --port 9123
- **THEN** HTTP-сервер слушает на 9123, GET /health отвечает 200

### Requirement: Подкоманды model и onnx-runtime

`model` управляет реестром моделей (list/benchmark по поведению оракула); `onnx-runtime` управляет загрузкой/состоянием ONNX Runtime. Флаги и вывод совпадают с Go оригиналом.

#### Scenario: Паритет usage
- **WHEN** выполнить machine-diff выводов `model --help` / `onnx-runtime --help` (Go vs Rust)
- **THEN** набор флагов, значения по умолчанию и тексты описаний идентичны (допускается нормализация форматирования)

### Requirement: Подкоманда load-test

Бенчмарк MCP-инструментов на сгенерированных данных. Флаги: `--scale small|medium|large` (default small), `--iterations N` (default 100), `--json PATH`, `--no-fill` (бенчить существующую БД). Отчёт: CALLS, AVG/P50/P95/P99/MAX ms, QPS по каждому tool-кейсу; человекочитаемый отчёт в stdout, при --json — тот же отчёт JSON.

#### Scenario: Отчёт
- **WHEN** load-test --scale small завершён
- **THEN** stdout содержит таблицу задержек по кейсам, структура совпадает с отчётом Go оригинала (machine-diff по секциям)

### Requirement: Резолвинг конфигурации

Приоритет: `--config` > `config.{preset}.yaml`, где preset по умолчанию `default`; авто-поиск файла конфига — сначала относительно каталога исполняемого файла, затем CWD. Поведение при отсутствии конфига и текст ошибок совпадают с оракулом (с учётом того, что Rust-бинарь лежит в своём каталоге).

#### Scenario: Пресет
- **WHEN** рядом с бинарью лежит config.prod.yaml и вызов `synopsis --preset prod serve`
- **THEN** используется config.prod.yaml без явного --config

### Requirement: Подкоманда queue

Новая подкоманда для инспекции и обслуживания очереди индексации документов (`document_jobs`). Подкоманды: `queue status [--source PATH] [--status NAME]` — табличный вывод состояния заданий (колонки: path, source, status, attempts, last_error, next_attempt_at); `queue reset-retries [--source PATH] [--path PATH]` — сброс счётчика повторов для заданий в статусе `error` (status→`pending`, attempts→0, next_attempt_at→now), после чего фоновый worker переиндексирует их. Подкоманда аддитивна к существующим (`sync`, `serve`, `model`, `onnx-runtime`, `load-test`); оракул аналога не имеет, поэтому parity не требуется.

> **Решение по контракту (2026-08-29):** подкоманда изначально проектировалась как `index`, но переименована в `queue` человеком — имя `queue` точнее отражает сущность (единая таблица состояний `document_jobs`, разделяемая watcher/startup/worker), а не процесс индексации. Это отклонение от исходного наименования в задаче 1.7 зафиксировано явно.

#### Scenario: Status
- **WHEN** вызвать `synopsis queue status`
- **THEN** выводится таблица всех заданий очереди `document_jobs` с колонками path/status/attempts/last_error/next_attempt_at; фильтр `--source` сужает вывод до одного источника, `--status` — до одного статуса

#### Scenario: Reset retries
- **WHEN** вызвать `synopsis queue reset-retries --path workspace/datasets/edtech/ontology/../content/documents/product/adaptive_learning_prd.md`
- **THEN** задание переходит в `pending` с attempts=0; следующий цикл фонового worker переиндексирует документ (строка больше не в статусе `error`)

