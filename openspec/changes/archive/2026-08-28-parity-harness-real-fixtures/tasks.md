# Tasks: parity-harness-real-fixtures

Зависимости: vectors (SYNX reader + ANN engine), mcp (Server + dispatch), embedding
(провайдер), config — готовы. Оракул (read-only): `../synopsis/data/knowledge.db`
(источник vec0, 270 chunks, dim=384), `../synopsis/bin/synopsis` (бейзлайны). Дизайн:
design.md D1–D5 + native-seam-spikes (SYNX формат). Гейты: `cargo fmt --check`,
`cargo clippy -p parity-harness --all-targets -- -D warnings`, `cargo test -p parity-harness`;
workspace-гейты — оркестратор. Директива (binding): НЕ открывать Go knowledge.db в Rust
(legacy DB rule); НЕ менять продакшн-код; фикстуры — dim=384 (из Go), продукт 1024.

- [x] 1.1 Загрузчик SYNX-фикстур + тест recall@k (реальный, не stub)
  - Цель: заменить `b"stub"` в `fixtures.rs` на реальную загрузку и дифф-тест.
  - Scope файлов: `crates/parity-harness/src/fixtures.rs` (заменить stub-запись на
    `load_fixture_set_from_dir(path)` читающий `vectors.bin`),
    `crates/parity-harness/src/lib.rs` (точка входа harness), возможно
    `crates/parity-harness/src/metrics.rs` (recall@k).
  - Содержание: SYNX-формат (из `crates/vectors/src/synx.rs` golden-test): заголовок
    20 байт — `magic "SYNX"` (4) + `version: u32 LE = 1` (4) + `dim: u32 LE` (4) +
    `count: u64 LE` (8); строки `[chunk_id: u32 LE][f32 LE × dim] × count`, отсортированы
    по chunk_id. Реализовать `load_fixture_set_from_dir(dir)` → читает `dir/vectors.bin`,
    парсит в `Vec<(u32, Vec<f32>)>`. Тест `recall_at_k`: загрузить фикстуру
    (dim=384, 270 векторов), построить ANN-индекс через crate `vectors` (SYNX-backed или
    in-memory engine, dim=384 — прочитать API в `crates/vectors/src/lib.rs`), для набора
    query-векторов (взять, напр., 20 векторов фикстуры) вычислить exact-L2 top-k ground
    truth в Rust, запросить движок, измерить recall@10, assert `>= 0.95` (порог из
    `openspec/specs/data-schema/spec.md:39`).
  - Тесты: `cargo test -p parity-harness` — новый тест recall@k зелёный на реальной
    фикстуре (committed в 1.2).
  - Критерии приёмки: `fixtures.rs` не содержит `b"stub"`; `load_fixture_set_from_dir`
    парсит SYNX; recall@10 >= 0.95; `cargo clippy -p parity-harness --all-targets -- -D warnings` зелёный.

- [x] 1.2 Сгенерировать и закоммитить реальную фикстуру `vectors.bin`
  - Цель: реальные данные для 1.1.
  - Scope файлов: `crates/parity-harness/fixtures/vectors.bin` (новый, ~415KB),
    `crates/parity-harness/fixtures/README.md` (происхождение).
  - Содержание: реальный `vectors.bin` УЖЕ извлечён в `/tmp/opencode/vectors.bin`
    (415820 байт, SYNX, dim=384, 270 векторов) аналитиком из vec0 Go-оракула
    (`../synopsis/data/knowledge.db`) через Go-экстрактор в /tmp (оракул нетронут).
    Скопировать его в `crates/parity-harness/fixtures/vectors.bin`. Если файл потерян —
    перегенерировать: собрать маленький Go-экстрактор в `/tmp` (sqlite-vec-go-bindings,
    CGO_ENABLED=1), прочитать vec0 из `../synopsis/data/knowledge.db`, записать SYNX
    (формат из 1.1 / native-seam-spikes). `fixtures/README.md`: откуда взята фикстура
    (Go knowledge.db, read-only), dim=384, 270 векторов, НЕ коммитим knowledge.db.
  - Тесты: фикстура загружается тестом 1.1.
  - Критерии приёмки: `crates/parity-harness/fixtures/vectors.bin` существует (415820 байт),
    валидный SYNX (magic "SYNX", version 1, dim 384, count 270); `cargo test -p parity-harness` зелёный.

- [x] 1.3 Тест p50/p95 латентности (12 MCP-инструментов vs Go-бейзлайны)
  - Цель: машиный паритет латентности.
  - Scope файлов: `crates/parity-harness/src/mcp_client.rs` (TimingStats),
    `crates/parity-harness/tests/parity_test.rs` (новый интеграционный тест) или
    `crates/parity-harness/src/lib.rs`.
  - Содержание: поднять Rust MCP-сервер in-process (или через harness) с маленьким
    Rust-инджестнутым корпусом (НЕ из Go DB — инджест через crate ingestion на пару
    документов), прогнать 12 инструментов через `McpClient`, собрать `TimingStats`
    (p50/p95/avg/max). Assert: `p50(search) <= 2 × Go_baseline(~107ms)` и
    `p95(search) <= 5 × Go_baseline(~109ms)` (бейзлайны из Go benchmark, см. design.md D4).
    Если точный бейзлайн недоступен — использовать пороги 2×/5× от измеренного Go
    `search` p50/p95 и задокументировать.
  - Тесты: `cargo test -p parity-harness` — интеграционный тест латентности зелёный.
  - Критерии приёмки: тест запускает 12 инструментов, собирает TimingStats, assert
    p50/p95 в гейтах; `cargo clippy -p parity-harness --all-targets -- -D warnings` зелёный;
    `cargo test -p parity-harness` — 0 failures.
