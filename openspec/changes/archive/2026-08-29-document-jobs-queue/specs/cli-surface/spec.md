# cli-surface Specification

## ADDED Requirements

### Requirement: Подкоманда queue

Новая подкоманда для инспекции и обслуживания очереди индексации документов (`document_jobs`). Подкоманды: `queue status [--source PATH] [--status NAME]` — табличный вывод состояния заданий (колонки: path, source, status, attempts, last_error, next_attempt_at); `queue reset-retries [--source PATH] [--path PATH]` — сброс счётчика повторов для заданий в статусе `error` (status→`pending`, attempts→0, next_attempt_at→now), после чего фоновый worker переиндексирует их. Подкоманда аддитивна к существующим (`sync`, `serve`, `model`, `onnx-runtime`, `load-test`); оракул аналога не имеет, поэтому parity не требуется.

> **Решение по контракту (2026-08-29):** подкоманда изначально проектировалась как `index`, но переименована в `queue` человеком — имя `queue` точнее отражает сущность (единая таблица состояний `document_jobs`, разделяемая watcher/startup/worker), а не процесс индексации. Это отклонение от исходного наименования в задаче 1.7 зафиксировано явно.

#### Scenario: Status
- **WHEN** вызвать `synopsis queue status`
- **THEN** выводится таблица всех заданий очереди `document_jobs` с колонками path/status/attempts/last_error/next_attempt_at; фильтр `--source` сужает вывод до одного источника, `--status` — до одного статуса

#### Scenario: Reset retries
- **WHEN** вызвать `synopsis queue reset-retries --path workspace/datasets/edtech/ontology/../content/documents/product/adaptive_learning_prd.md`
- **THEN** задание переходит в `pending` с attempts=0; следующий цикл фонового worker переиндексирует документ (строка больше не в статусе `error`)
