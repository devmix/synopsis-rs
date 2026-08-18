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

- [x] 2.1 Linux job: fmt + clippy + test — выполнено, commit 1a60326
  - **Цель:** базовые гейты в CI.
  - **Scope файлов:** workflow-файл CI (linux x64): шаги `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
  - **Зависимости:** задача 1.1.
  - **Критерии приёмки:** job зелёный на текущем скелете; red при намеренно сломанном formate/clippy-кейсе (проверить вручную один раз).
  - **Референс:** CI оракула для стилистики — ../synopsis/.github/ (если есть), иначе стандартные GitHub Actions.

- [x] 2.2 Кросс-матрица: 5 таргетов через cargo-zigbuild — выполнено, commit f9cb5ab
  - **Цель:** раннее обнаружение платформенных проблем сборки (замена CGO_CFLAGS/darwin-stubs из оракула).
  - **Scope файлов:** CI-job на матрице x86_64-unknown-linux-musl, aarch64-unknown-linux-gnu/musl, x86_64-pc-windows-msvc/gnu (через zig), aarch64-apple-darwin; install cargo-zigbuild + Zig 0.14+.
  - **Зависимости:** задача 2.1.
  - **Критерии приёмки:** все таргеты собираются из CI; бинарь linux-musl запускается в контейнере и печатает версию (`./synopsis --version` → код 0).
  - **Референс:** целевая матрица — ../synopsis/configs/onnx.yaml (platforms) и AGENTS.md оракула (make build-all / scripts/build.sh).
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, указание человека «сделать проще» + веб-верификация оркестратора):
      - Установка cargo-zigbuild — `cargo install --locked cargo-zigbuild` (официально задокументированный способ в README v0.23.0; версия фиксируется crates.io → 0.23.0, latest release от 18.06.2026, проверено на crates.io/lib.rs/Arch). Официального GitHub Action у проекта НЕТ (проверено по .github в репо) — предшествующий черновик с curl+sha256+tar из prebuilt-архива отклонён как избыточный; артефакт `cargo install` кэшируется Swatinem/rust-cache через CARGO_HOME.
      - Zig — фиксированная версия 0.16.0 (текущий stable на 2026-08-18, релиз 14.04.2026, ziglang.org/news/0.16.0-released) через `mlugg/setup-zig@v2` (action поддерживает GitHub Actions после миграции автора на Codeberg; minisign-верификация и кэш zig между рансами).
      - Матрица таргетов, шаги тулчейна/кэша и musl smoke-test в Alpine — без изменений относительно тела задачи. windows-gnu вместо msvc: Zig не линкует MSVC ABI с Linux-хоста (cargo-zigbuild CI сам использует gnu для Windows) — оставить x86_64-pc-windows-gnu, msvc не добавлять.
      - Риск (принят): репозиторий пинится на rustc 1.96.0, а текущий stable на момент ревизии = 1.97.1 (2026-07-16); README cargo-zigbuild тестирует «current stable + nightly». Пин НЕ менять в этой задаче (согласованность с job checks из 2.1, scope discipline) — если кросс-сборка на 1.96.0 упадёт по причине toolchain-age, это отдельное решение человека о bump'e rust-toolchain.toml.
      - Локальная проверка возможна без GitHub: на этой машине уже стоят zig 0.16.0 и rustc 1.96.0 → `cargo install --locked cargo-zigbuild`, затем `cargo zigbuild --release --target x86_64-unknown-linux-musl` и запуск статического бинаря `./synopsis --version` (де-ризикивает весь путь до реального CI).

## 3. parity-harness скелет

- [x] 3.1 Crate `parity-harness`: MCP-клиент (rmcp) + fixture loader API
  - **Цель:** каркас приёмочного механизма (design D6/D8) — без реальных parity-кейсов, они приходят вместе с модульными change'ами.
  - **Scope файлов:** корневой `Cargo.toml` ([workspace].members += `crates/parity-harness`; [workspace.dependencies] += `rmcp = "3"` с комментарием «официальный MCP SDK (modelcontextprotocol/rust-sdk), design D8»; обновить устаревающий комментарий блока «Intentionally NOT pinned» про отложенный выбор MCP-protocol crate — решение принято: rmcp 3.x), `crates/parity-harness/Cargo.toml` (`lints.workspace = true`; rmcp с features: клиентский путь `client`, `transport-streamable-http-client-reqwest`, `reqwest`; для round-trip теста — server-side streamable transport из того же crates.io release), `crates/parity-harness/src/{lib.rs, mcp_client.rs, fixtures.rs, diff.rs}`: обёртка над rmcp-клиентом (initialize/tools/list/tools/call + сбор таймингов p50/p95 на вызов; ошибки — типизированный результат без panic), загрузчик фикстур (API: путь к knowledge.db + vectors.bin — формат бинарного дампа фиксируется в native-seam-spikes, пока stub с TODO-документацией), JSON/text diff утилиты.
  - **Зависимости:** задача 1.1; решения design D2/D6/D8 (этот change).
  - **Критерии приёмки:** (а) юнит-тесты корректности percentiles на синтетических задержках (известные датасеты, граничные случаи n=0/n=1/несколько значений); (б) round-trip integration test: in-process rmcp Streamable HTTP server с 2–3 dummy tools → обёртка harness'а выполняет initialize/tools/list/tools/call (успех + error-кейс: неизвестный tool и/или tool-level error) — тайминги p50/p95 собираются, ошибки возвращаются типизированно; (в) `cargo test`, `cargo clippy --all-targets` (без замечаний), `cargo fmt --check` чисты.
  - **Референс:** README/примеры rmcp (`modelcontextprotocol/rust-sdk`: features и примеры transport-streamable-http client/server), секция Transports MCP spec; форма отчёта p50/p95 — ../synopsis/site/docs/guides/load-testing.mdx.
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, решение человека «используй rmcp, совместимость с Go не сохранять» + веб-верификация оркестратора): MCP в Rust — на официальном SDK `rmcp` (`modelcontextprotocol/rust-sdk`; crates.io 3.1.3 от 2026-08-17; spec `2026-07-28`, compat ≥ `2025-11-25`). Wire-совместимость с legacy SSE оракула (`mark3labs/mcp-go v0.57.0`: `GET /sse` + `POST /message?sessionId=`, spec 2024-11-05) намеренно НЕ сохраняется — legacy SSE deprecated в MCP spec с 2025-03-26, rmcp 3.x не содержит legacy-SSE транспорта. Паритет переносится на уровень ответов инструментов: фикстуры записываются из Go-оракула одноразово, сравнение — через rmcp-клиент (design D8). Клиент harness'а = обёртка над rmcp transport-streamable-http-client(-reqwest); ручной HTTP/SSE-парсер не пишется. Критерии приёмки заменены: round-trip против in-process rmcp server вместо «парсинг SSE на записанных фикстурах». Риск (принят): reqwest/rustls добавляет native-зависимости в кросс-матрицу 2.2 — при падении zigbuild отдельное решение человека.

## 4. Документация репо

- [x] 4.1 README.md + AGENTS.md
  - **Цель:** человек и любой свежий агент понимают стек, команды, правила паритета без внешних источников.
  - **Scope файлов:** `README.md` (что это, статус миграции, команды build/test/parity), `AGENTS.md` (стек из openspec/config.yaml context, команды, модель исполнения «ИИ пишет / человек ревьюит», правило ~500 строк diff на задачу, путь к оракулу ../synopsis).
  - **Зависимости:** задачи 1.1–3.1 (документируем то, что существует).
  - **Критерии приёмки:** человек-ревьюер подтверждает: по README новый агент может собрать проект и прогнать тесты без вопросов; в AGENTS.md зафиксированы модель исполнения и паритетные правила.
  - **Референс:** стиль — ../synopsis/AGENTS.md (командная таблица, gotchas), но содержание — только фактическое состояние этого репо.
  - **История ревизий:**
    - Ревизия 1 (2026-08-18, одобрено человеком «yes» с дефолтами оркестратора): (а) CI/clippy badges в README — нет; (б) секция OpenSpec workflow (proposal → design → specs → tasks → apply → archive) — только в AGENTS.md, не в README; (в) workspace crate'ы в AGENTS.md — компактная таблица по строчке на каждый crate.
