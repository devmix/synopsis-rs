# Tasks — scaffold-rust-project

Порядок = граф зависимостей. Каждая задача самодостаточна для свежего агента (~100k контекста): цель, scope файлов, зависимости, критерии приёмки, референс. Формат: чекбокс + блок деталей.

## 1. Workspace-скелет

- [x] 1.1 Корневой workspace и crate-заглушки по layout design.md (D1) — выполнено, commit 6abd24a
  - **Цель:** `cargo build`/`cargo test` зелёные на пустом workspace; границы модулей зафиксированы.
  - **Scope файлов:** корневой `Cargo.toml` (workspace members + shared deps-палитра), `rust-toolchain.toml`, `.gitignore`, `crates/{config,db,vectors,embedding,ingestion,graph,search,mcp,cli}/src/lib.rs` (+ `main.rs` в cli с print-version заглушкой) — по одному пустому модулю-заглушке на crate.
  - **Зависимости:** нет (первая задача).
  - **Критерии приёмки:** `cargo build`, `cargo test`, `cargo clippy --all-targets` (без замечаний), `cargo fmt --check` — всё чисто; граф зависимостей между crate'ами соответствует design.md D1.
  - **Референс:** layout — design.md D1; маппинг на Go-пакеты — ../synopsis/internal/*, cmd/app.
  - **История ревизий:**
    - Ревизия 1 (2026-08-18): по решению человека добавить секцию `[workspace.lints]` в корневой Cargo.toml (`[workspace.lints.rust] missing_docs = "deny"` + базовые onboarding-lint'ы) и применить `lints.workspace = true` во всех 9 crate'ах; гейты (fmt/clippy/test) должны остаться зелёными. Вопрос noyalib vs serde_yaml отложен до config-change — в этой задаче не менять.
      - Корректировка (2026-08-18, проверка оркестратора): `lints.workspace = true` должен стоять в топ-уровневой таблице `[lints]`, а НЕ внутри `[package]` (иначе cargo warning «unused manifest key: package.lints» и линты не применяются). Пробой на том же тулчейне 1.96.0 подтвердил: топ-уровневый `[lints] workspace = true` наследуется штатно, `missing_docs = "deny"` срабатывает.

## 2. CI

- [ ] 2.1 Linux job: fmt + clippy + test
  - **Цель:** базовые гейты в CI.
  - **Scope файлов:** workflow-файл CI (linux x64): шаги `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
  - **Зависимости:** задача 1.1.
  - **Критерии приёмки:** job зелёный на текущем скелете; red при намеренно сломанном formate/clippy-кейсе (проверить вручную один раз).
  - **Референс:** CI оракула для стилистики — ../synopsis/.github/ (если есть), иначе стандартные GitHub Actions.

- [ ] 2.2 Кросс-матрица: 5 таргетов через cargo-zigbuild
  - **Цель:** раннее обнаружение платформенных проблем сборки (замена CGO_CFLAGS/darwin-stubs из оракула).
  - **Scope файлов:** CI-job на матрице x86_64-unknown-linux-musl, aarch64-unknown-linux-gnu/musl, x86_64-pc-windows-msvc/gnu (через zig), aarch64-apple-darwin; install cargo-zigbuild + Zig 0.14+.
  - **Зависимости:** задача 2.1.
  - **Критерии приёмки:** все таргеты собираются из CI; бинарь linux-musl запускается в контейнере и печатает версию (`./synopsis --version` → код 0).
  - **Референс:** целевая матрица — ../synopsis/configs/onnx.yaml (platforms) и AGENTS.md оракула (make build-all / scripts/build.sh).

## 3. parity-harness скелет

- [ ] 3.1 Crate `parity-harness`: MCP SSE-клиент + fixture loader API
  - **Цель:** каркас приёмочного механизма (design D6) — без реальных кейсов, они приходят вместе с модульными change'ами.
  - **Scope файлов:** `crates/parity-harness/src/{lib.rs, mcp_client.rs, fixtures.rs, diff.rs}`: клиент SSE (`GET /sse` + `POST /message`, вызов tools/call, тайминги p50/p95), загрузчик фикстур (API: путь к knowledge.db + vectors.bin — формат бинарного дампа фиксируется в native-seam-spikes, пока stub с TODO-документацией), JSON/text diff утилиты.
  - **Зависимости:** задача 1.1.
  - **Критерии приёмки:** юнит-тесты: парсинг SSE-ответов на записанных фикстурах (2–3 примера, включая error-кейс), корректность percentiles на синтетических задержках; `cargo test` зелёный.
  - **Референс:** протокол/эндпоинты — ../synopsis/internal/mcp (transport) и AGENTS.md оракула (`GET /sse`, `POST /message`, `GET /health`); структура отчёта p50/p95 — ../synopsis/site/docs/guides/load-testing.mdx.

## 4. Документация репо

- [ ] 4.1 README.md + AGENTS.md
  - **Цель:** человек и любой свежий агент понимают стек, команды, правила паритета без внешних источников.
  - **Scope файлов:** `README.md` (что это, статус миграции, команды build/test/parity), `AGENTS.md` (стек из openspec/config.yaml context, команды, модель исполнения «ИИ пишет / человек ревьюит», правило ~500 строк diff на задачу, путь к оракулу ../synopsis).
  - **Зависимости:** задачи 1.1–3.1 (документируем то, что существует).
  - **Критерии приёмки:** человек-ревьюер подтверждает: по README новый агент может собрать проект и прогнать тесты без вопросов; в AGENTS.md зафиксированы модель исполнения и паритетные правила.
  - **Референс:** стиль — ../synopsis/AGENTS.md (командная таблица, gotchas), но содержание — только фактическое состояние этого репо.
