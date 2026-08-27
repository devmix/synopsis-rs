# Tasks: cli

Зависимости: config, db, vectors, embedding, ingestion, graph, search, mcp — все
готовы и проходят gates. Замороженный контракт: `openspec/specs/cli-surface/spec.md`
— РЕАЛИЗУЕТСЯ, не меняется (help-текст НЕ байт-паритетен с Go — решение
пользователя 2026-08-27, см. proposal.md Deviations). Оракул (read-only):
`../synopsis/cmd/app/{main,cmd,serve,sync,model_cmd,onnx_runtime,loadtest}.go`.
Дизайн: design.md D1–D12. Гейты каждой задачи: `cargo fmt --check`,
`cargo clippy -p cli --all-targets -- -D warnings`, `cargo test -p cli`;
workspace-гейты гоняет ТОЛЬКО оркестратор. Директива (binding): НЕ копировать Go
1:1; DRY/KISS/SOLID/YAGNI; баги оракула исправлять или фиксировать отклонения.
Новые зависимости (user-approved 2026-08-27): clap 4.6.6, tracing 0.1.44,
tracing-subscriber 0.3.23 (workspace); notify 8 / tokio-cron-scheduler 0.15 /
indicatif 0.18 уже в workspace.

- [x] 1.1 CLI-каркас: clap + глобальные флаги + разрешение конфигурации + dispatch
  - Цель: парсинг аргументов и маршрутизация подкоманд.
  - Scope файлов: `crates/cli/Cargo.toml` (добавить clap, tracing,
    tracing-subscriber, ingestion, utils, vectors к зависимостям), `crates/cli/src/main.rs`
    (точка входа: init tracing, parse, dispatch), `crates/cli/src/lib.rs` (pub модули),
    `crates/cli/src/cli.rs` (clap Builder: глобальные флаги + 5 подкоманд-заглушек
    пока без тела), `crates/cli/src/config_resolver.rs` (новый: resolve_config_path).
  - Содержание: design D1 + D2. clap 4.6.6 Builder API: глобальные `--config`,
    `--preset` (default "default"), `--db`, `--version` (печатает `synopsis <version>`
    и exit 0); подкоманды sync/serve/model/onnx-runtime/load-test (пока dispatch на
    заглушки, которые печатают "not yet implemented" в stderr + exit 1 — реальные
    тела в 1.7–1.10). `config_resolver.rs`: порт `resolveConfigPath`/`resolveConfigCandidates`
    (exeDir/configs → exeDir → parent → parent/configs → cwd/configs → cwd →
    fallback `configs/config.{preset}.yaml`); `--config` побеждает. tracing init из
    `config.Logging.Level` (default info) — для этого загрузи config в main ДО dispatch
    (упрощённо: читай только logging.level, остальное — в bootstrap позже).
  - Тесты: unit `config_resolver` — все ветви кандидатов (tmp-файлы); `--version`
    exit 0; неизвестная подкоманда → exit 1; нет подкоманды → usage + exit 1.
  - Критерии приёмки: гейты cli зелёные; `cargo build -p cli` собирается;
    `synopsis --version` печатает версию и exit 0.

- [x] 1.2 serve bootstrap: DB + миграции + dimension-mismatch + embedding + cache + health
  - Цель: функция bootstrap и старт-хелсчек.
  - Scope файлов: `crates/cli/src/serve/mod.rs` (новый модуль serve),
    `crates/cli/src/serve/bootstrap.rs` (новый: bootstrap, open_db, ensure_model,
    open_cache, dimension-mismatch), `crates/cli/src/serve/health.rs` (новый:
    startup health check), `crates/cli/src/error.rs` (новый: CliError + exit-коды).
  - Содержание: design D3 + D4 (часть). `bootstrap(cfg_path, db_path) -> Bootstrap`
    где Bootstrap содержит Config, DomainRegistry, Db, Option<CacheDb>,
    EmbeddingProvider, OnnxConfig. `Db::open(db_path)` + `run_migrations`
    (PRAGMA user_version). Dimension-mismatch: перехвати ошибку миграции, НЕ fatal —
    верни признак mismatch в Bootstrap (serve/sync решат в 1.6/1.7). `new_onnx_provider`
    (local: auto-download через ModelManager::ensure_model; explicit ModelPath —
    skip). Cache DB: `Db::open(cache_path)`; Err → warn + `cache=None`. Health:
    db.ping, doc count (DocumentDao::count), embedding-provider probe (создай
    provider, лови ошибку) — только лог, не fatal.
  - Тесты: unit — bootstrap с in-memory Db (`:memory:` или temp file): миграции
    применяются; cache-DB failure → Bootstrap.cache == None (подмени путь на
    невалидный); embedding provider init с заранее скачанной моделью или skip при
    explicit ModelPath.
  - Критерии приёмки: гейты cli зелёные; bootstrap покрыт тестами.

- [ ] 1.3 Runner-сборка + initial sync
  - Цель: собрать RunnerParams и запустить начальную синхронизацию.
  - Scope файлов: `crates/cli/src/serve/bootstrap.rs` (добавить build_runner +
    initial_sync), `crates/cli/src/serve/ingest.rs` (новый: обёртки ingest_all /
    ingest_source_by_path / prune_deleted / cleanup_orphaned_data).
  - Содержание: design D3 + D4. Собери `ingestion::runner::RunnerParams` (≈11 полей:
    db, ingest_cfg, global, domains, registry, embed, vectors, prompts, linker_cfg,
    prompts_path, llm_cache) из Bootstrap + config. Domain discovery — порт
    `domain.DiscoveryWithLogger` (config::domain). NER prompts — `ingestion::ner`
    loader по `config.paths.prompts_path`. Vectors — `vectors::Engine::new`. Graph —
    `graph::GraphIndex::from_db` (если enable_graph). `Runner::new(params)` →
    `ingest_all(rebuild)`; верни `SummaryStats`.
  - Тесты: unit — build_runner с минимальным config (1 in-memory источник) не падает;
    initial_sync на temp Db + temp source dir создаёт документы/чанки (проверь count).
  - Критерии приёмки: гейты cli зелёные; Runner собирается из реальных коллабораторов.

- [ ] 1.4 File watcher (notify + debounce)
  - Цель: инкрементальная переиндексация при изменении файлов.
  - Scope файлов: `crates/cli/src/serve/watcher.rs` (новый).
  - Содержание: design D5. `notify::PollWatcher` с debounce =
    `config.autoupdate.debounce_seconds`. Callback: dedupe changed paths по source
    (`Runner::source_for_path`) → `ingest_source_by_path` для каждого затронутого
    source → `prune_deleted` → если `enable_graph`: reload graph, `SetGraph` на
    searcher + `mcp::Server`. Watch всех enabled/не-disabled sources из global config.
    `Watcher::stop()` для graceful shutdown.
  - Тесты: unit — debounce-логика (таймер); callback вызывает ingest для изменённого
    пути (подмени Runner на stub через trait/closure).
  - Критерии приёмки: гейты cli зелёные; watcher не блокирует старт.

- [ ] 1.5 Scheduler (orphan_cleanup)
  - Цель: периодическая очистка осиротевших данных.
  - Scope файлов: `crates/cli/src/serve/scheduler.rs` (новый).
  - Содержание: design D6. `tokio_cron_scheduler::JobScheduler`; регистрируй job
    `orphan_cleanup` iff `config.scheduler.jobs["orphan_cleanup"].enabled`, interval
    из `interval_seconds`; тело → `Runner::cleanup_orphaned_data`. `start()` после
    initial sync; `shutdown()` на остановке.
  - Тесты: unit — регистрация job только если enabled; shutdown не паникует.
  - Критерии приёмки: гейты cli зелёные.

- [ ] 1.6 MCP mount + graceful shutdown
  - Цель: поднять MCP over Streamable HTTP на порту и корректно гасить.
  - Scope файлов: `crates/cli/src/serve/server.rs` (новый: HybridSearcher + mcp::Server
    + axum listener + shutdown), `crates/cli/src/serve/mod.rs` (связать watcher/
    scheduler/server в run_serve).
  - Содержание: design D4 + D7. `HybridSearcher::new(...)` (6 параметров — см.
    `crates/search/src/hybrid.rs:83`). `mcp::Server::new(name, version, db, searcher,
    graph).router()` → `axum::serve(listener, router).with_graceful_shutdown(async {
    shutdown_rx.await })`. SIGINT+SIGTERM (`tokio::signal::unix`) → shutdown_tx; после
    остановки axum — `scheduler.shutdown()` + `watcher.stop()`. 10s timeout через
    `tokio::time::timeout`. `run_serve` собирает 1.2–1.5 в единый поток.
  - Тесты: integration — `synopsis serve --port N --no-initial-sync` (без реальных
    sources) поднимается, `GET /health` → 200, SIGTERM гасит за <10s (spawn процесса
    или in-process с tokio runtime в тесте).
  - Критерии приёмки: гейты cli зелёные; serve стартует и корректно останавливается.

- [ ] 1.7 sync subcommand
  - Цель: одноразовая полная реиндексация.
  - Scope файлов: `crates/cli/src/sync.rs` (новый).
  - Содержание: design D8. `run_sync(cfg_path, db_path, rebuild, auto_rebuild_vectors)`:
    bootstrap (1.2) → dimension-mismatch handling (auto-rebuild через
    `ingest_all(true)` если флаг/config, иначе fatal) → `Runner::new` →
    `ingest_all(rebuild)` → stderr summary block (sources/documents created-updated-
    skipped/errors/duration). Exit 0 / non-zero.
  - Тесты: integration — `synopsis sync --rebuild` на temp config+source создаёт
    документы (проверь count в БД); summary печатается.
  - Критерии приёмки: гейты cli зелёные; summary блок совпадает по полям с оракулом
    (sources/created/updated/skipped/errors/duration).

- [ ] 1.8 model subcommand
  - Цель: управление моделями эмбеддингов.
  - Scope файлов: `crates/cli/src/model.rs` (новый).
  - Содержание: design D9. `model list|download|delete|info|benchmark` через
    `embedding::ModelManager` (`list_models`, `download_model`, `delete_model`,
    `get_model_path`, `registry`) + `embedding::benchmark_model` (порт model_cmd.go:
    hardware/CPU/RAM/ONNX version + production padded seq=512 + natural lengths
    tok/s). `indicatif` прогресс где оракул показывает прогресс.
  - Тесты: unit — `model list` на temp data dir с заранее положенной моделью печатает
    таблицу; `model info <known>` печатает поля; неизвестная подкоманда → exit 1.
  - Критерии приёмки: гейты cli зелёные; вывод таблиц совпадает по полям с оракулом.

- [ ] 1.9 onnx-runtime subcommand
  - Цель: управление ONNX Runtime библиотекой.
  - Scope файлов: `crates/cli/src/onnx_runtime.rs` (новый).
  - Содержание: design D10. `onnx-runtime install|status|uninstall` через
    `embedding::LibraryManager` (`ensure_library`, `get_library_path`, `uninstall`,
    `get_version`, `get_cache_dir`) + platforms table из onnx config (порт
    onnx_runtime.go: install/status/uninstall + Supported Platforms tabwriter).
  - Тесты: unit — `onnx-runtime status` печатает Version/Status/Cache (+ platforms);
    неизвестная подкоманда → exit 1.
  - Критерии приёмки: гейты cli зелёные; вывод совпадает по полям с оракулом.

- [ ] 1.10 load-test subcommand
  - Цель: бенчмарк 12 MCP-инструментов на сгенерированных данных.
  - Scope файлов: `crates/cli/src/loadtest/mod.rs` (новый), `generator.rs`,
    `filler.rs`, `runner.rs`, `report.rs` (новые).
  - Содержание: design D11. `load-test --scale small|medium|large --seed 42
    --iterations 100 --json PATH --no-fill`. Dimension mismatch под `--no-fill` fatal,
    иначе drop+recreate vector table. `require_embedding_model` (никогда не
    auto-download). `Generator(seed).generate(scale)` → `Fill` (реальные эмбеддинги,
    progress) → graph load + timing → `HybridSearcher` → `benchmark::Runner` вызывает
    `mcp::Server::dispatch(name, args)` НАПРЯМУЮ (уже `pub`, server.rs:113) по всем 12
    инструментам → `Report` (CALLS/AVG/P50/P95/P99/MAX ms/QPS) в stdout, `--json` в
    файл. Модуль НЕ зависит от parity-harness.
  - Тесты: unit — generator детерминирован (один seed → одни данные); report-форма
    (секции/колонки) совпадает с оракулом (см. `../synopsis/internal/benchmark/
    report.go`); `dispatch` по всем 12 инструментам возвращает Value (используй
    in-memory Db + seeded данные).
  - Критерии приёмки: гейты cli зелёные; `synopsis load-test --scale small` печатает
    таблицу задержек по кейсам (структура как у Go оригинала).
