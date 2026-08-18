# Design — scaffold-rust-project

## Context

См. proposal.md (Why). Ограничения, влияющие на архитектуру: цель — ноутбук 16 ГБ; один бинарь без внешних сервисов; реализацию пишет ИИ с бюджетом контекста ~100k на задачу и без памяти предыдущих задач; паритет с Go-оракулом (`../synopsis`) проверяется машиной.

## Goals / Non-Goals

**Goals:** workspace, который (а) отражает доменную структуру оракула 1:1 для удобного маппинга при ревью, (б) разбивает код на единицы, помещающиеся в контекст одного агента, (в) даёт каждому модулю автоматический паритетный гейт.

**Non-Goals:** какое-либо прикладное поведение поиска/инжестии/MCP; выбор конкретного ANN-движка; фикстура-экспортер (живёт в Go-репо и добавляется change'ом `native-seam-spikes`).

## Decisions

### D1. Workspace из доменных crate'ов (не монолит)

```
synopsis-rs/
  Cargo.toml            # workspace
  crates/
    config/             # YAML-пресеты, onnx.yaml, онтологии XML      ← internal/config
    db/                 # rusqlite, миграции, DAO, FTS5               ← internal/database
    vectors/            # ANN-индекс: стабильный trait + движок        ← (заменяет vec0)
    embedding/          # ONNX Runtime mgmt + provider                ← internal/onnx+embedding
    ingestion/          # parsers, chunkers, NER, entities            ← internal/ingestion
    graph/              # knowledge graph + linkers (CEL)             ← internal/graph+relations
    search/             # гибридный поиск, RRF                        ← internal/search
    mcp/                # MCP server + 12 tool handlers               ← internal/mcp
    cli/                # бинарь: subcommands, флаги, config resolve  ← cmd/app
    parity-harness/     # dev-tooling: MCP-клиент (rmcp), diff'ер, fixture loader
```

**Почему:** (1) границы crate'ов повторяют пакеты оракула — ревьюер видит маппинг «Go-пакет → crate» без усилий; (2) каждый crate ≤ ~3–5k строк = помещается в бюджет контекста одной задачи вместе с тестами и орракул-референсами; (3) независимые compile units ускоряют CI. Граф зависимостей: `config, db, vectors` → `embedding, ingestion, graph` → `search` → `mcp` → `cli`; `parity-harness` зависит от `mcp`-контракта и используется во всех последующих change'ах.
**Альтернативы:** один crate (отклонено: контекст агента и compile time); микросервисы/процессы (отклонено: нарушает «один локальный бинарь»).

### D2. tokio + axum для async/HTTP

Подтверждено как индустриальный стандарт 2026 (axum ведётся командой Tokio; production у Cloudflare/Pomerium). MCP-транспорт = Streamable HTTP через встроенный серверный транспорт официального SDK `rmcp` (решение D8); axum остаётся для вспомогательных эндпоинтов (`/health`) и не-MCP потребностей. Гибридный поиск использует параллельные ветки (tokio::join!) как в оракуле.
**Альтернативы:** actix-web (отклонено: узкая ниша raw-performance, не нужна для локального сервиса с десятками RPS); синхронный runtime (отклонено: SSE + параллельные ветки поиска).

### D3. rusqlite (bundled + fts5), sync-драйвер за spawn_blocking

FTS5 компилируется в бинарь всегда → весь класс «тихой деградации isModuleError» из Go исчезает по построению. Синхронный SQLite (WAL, single-writer) обслуживается через `tokio::task::spawn_blocking` / пул соединений; workload локальный, async-драйвер не нужен.
**Альтернативы:** sqlx (отклонено: async-first, тяжелее компиляция, FTS5/vec-модули менее проработаны); libsqlite3 системная (отлонено: возвращает проблему версионирования платформенных библиотек).

### D4. ONNX Runtime — внешний .so/.dylib через onnxruntime-rs

Механизм загрузки рантайма по `onnx.yaml` (download/verify в data/) переносится как есть; рантайм не линкуется на build-времени → кросс-компиляция остаётся чистой.
**Альтернативы:** pure-Rust инференс (нет практического эквивалента для bge-m3).

### D5. ANN: стабильный trait в crate `vectors`, движок выбирается спайками

`vectors` определяет контракт хранилища (build/add/search/remove/save, параметры M/efSearch/квантизация) независимо от реализации; usearch vs lance решает change `native-seam-spikes` по измерениям (p95 + recall@10 на фикстуре 1M). Остальные crate'ы знают только trait → смена движка не трогает систему.
**Причина:** оба кандидата удовлетворяют RAM-ограничению ноутбука; выбор по бенчмарку, а не по предпочтениям.

### D6. Паритетный harness — first-class dev-tooling в репо

`parity-harness`: (а) fixture loader — читает knowledge.db + vectors.bin (экспортируются из Go-оракула одноразово, формат фиксируется change'ом native-seam-spikes); (б) MCP-клиент на официальном SDK `rmcp` (Streamable HTTP transport) + слой инструментирования таймингов p50/p95 — паритет = сравнение ответов Rust-сервера с фикстурами, записанными из Go-оракула одноразово; (в) diff'еры: JSON-diff ответов tools/list и tool-вызовов, text-diff `--help`/usage выводов, effective-config diff. Каждый последующий модульный change добавляет свои parity-кейсы в harness — гейт задачи = «свои кейсы зелёные».
**Причина:** человек ревьюит контракты, а не строки; машина проверяет паритет на каждом шаге, а не один раз в конце.

### D7. CI

Linux job: `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test`. Кросс-проверка 5 таргетов (linux/darwin/windows × amd64/arm64) через cargo-zigbuild — замена Makefile+CGO_CFLAGS+darwin-stubs из оракула. Contract-gate job: запускает Rust-бинарь, гоняет parity-harness (rmcp-клиент) против него и сравнивает ответы с фикстурами, записанными из Go-оракула одноразово (включается по мере появления кейсов).

### D8. MCP-транспорт: Streamable HTTP через официальный SDK rmcp; wire-совместимость с оракулом намеренно не сохраняется (изменение замороженного контракта)

Решение человека от 2026-08-18 («используй rmcp, совместимость с Go не сохранять»): весь MCP в Rust — на официальном SDK `rmcp` (`modelcontextprotocol/rust-sdk`; crates.io 3.1.3, релиз 2026-08-17; реализует spec `2026-07-28`, совместимость ≥ `2025-11-25`). Транспорт — Streamable HTTP (единый endpoint: POST JSON-RPC → plain JSON или SSE-поток). Legacy SSE-транспорт Go-оракула (`mark3labs/mcp-go v0.57.0`: `GET /sse` + `POST /message?sessionId=`, spec 2024-11-05) **намеренно не воспроизводится** — актуальные SDK legacy-SSE транспорта уже не содержат (в rmcp 3.x `client-side-sse` — лишь парсер SSE внутри streamable-HTTP клиента).
**Почему:** legacy HTTP+SSE deprecated в MCP spec с ревизии 2025-03-26; актуальные клиенты говорят Streamable HTTP; rmcp — официальный SDK с активным релизным циклом (4 версии за месяц на момент решения); пин тулчейна 1.96.0 ≥ MSRV rmcp (1.88).
**Альтернативы:** сохранить legacy SSE 1:1 с оракулом ради wire-паритета (отклонено решением человека — deprecated протокол, ручная поддержка); собственный axum-SSE клиент/сервер без SDK (отклонено — ручной JSON-RPC/SSE, больше кода и багов).
**Влияние на паритет:** сравнение «два живых бинаря по одному wire» невозможно; фикстуры ответов инструментов записываются из Go-оракула одноразово и сравниваются с ответами Rust-сервера через rmcp-клиент (harness). Контракты 12 tools (имена, JSON-схемы параметров/ответов, approved-only семантика) остаются эталоном без изменений; `/health` остаётся.
**Риск:** reqwest/rustls (HTTP-стек rmcp) добавляет native-зависимости в кросс-матрицу 2.2; если cargo-zigbuild упадёт на каком-то таргете — отдельное решение человека о bump'e/замене TLS-providers.

## Risks / Trade-offs

- **MCP-транспорт несовместим с оракулом (D8)** — клиенты, говорящие только legacy SSE, не подключатся к Rust-серверу. Митигация: локальный личный сервис; актуальные MCP-клиенты используют Streamable HTTP; решение принято человеком 2026-08-18.
- **Спеки контрактов — транскрипция оракула на момент планирования.** Дрейф возможен. Митигация: machine-diff'ы сравнивают Rust против фикстур ЖИВОГО Go-бинаря (записанных одноразово; wire-совместимость снята D8), а не только против текстов спеков; спеки — человекочитаемая сводка.
- **sync SQLite в async runtime** требует дисциплины spawn_blocking (заблокированный пул = деградация). Митигация: pool + лимит concurrency в crate `db`, проверяется load-test кейсами.
- **Границы crate'ов унаследованы от Go.** Некоторые могут оказаться неоптимальными для Rust; перенос/слияние crate'ов разрешается в будущих change'ах при сохранении контрактов и графа зависимостей.
- **parity-harness зависит от фикстуры из Go-репо** — экспорт vectors.bin требует небольшого добавления в оракул (из scope этого change; выполняется в native-seam-spikes).
