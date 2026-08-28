# Design: parity-harness-real-fixtures

## D1 — SYNX format (контракт из native-seam-spikes)
Заголовок 20 байт: `magic "SYNX"` (4) + `version: u32 LE = 1` (4) + `dim: u32 LE` (4)
+ `count: u64 LE` (8). Тело: `[chunk_id: u32 LE][f32 LE × dim]` × count, отсортировано
по chunk_id. Верифицирован golden-bytes тестом в `crates/vectors/src/synx.rs`
(lines 268–279). Реальная фикстура: 415820 байт = 20 + 270×(4 + 384×4).

## D2 — Источник фикстуры (read-only оракул)
`../synopsis/data/knowledge.db` (2.2MB, 270 chunks, vec0 FLOAT[384]) — read-only.
vec0-векторы извлечены аналитиком через отдельный Go-экстрактор в `/tmp` (sqlite-vec-go-bindings,
CGO_ENABLED=1) в SYNX `vectors.bin`. Оракул НЕ тронут. Результат уже в
`/tmp/opencode/vectors.bin` (бэкап). Продуктовый Rust НЕ открывает Go knowledge.db
(legacy DB rule) — используем только извлечённый SYNX.

## D3 — recall@k дифф-тест
`load_fixture_set_from_dir(dir)` читает `vectors.bin` → `Vec<(u32, Vec<f32>)>`.
Строим ANN-индекс через crate `vectors` (dim=384). Для query-набора (часть векторов
фикстуры) считаем exact-L2 top-k ground truth в Rust, запрашиваем движок, меряем
recall@10. Порог `>= 0.95` из `openspec/specs/data-schema/spec.md:39`
("recall@10 >= 0.95 relative to brute-force ground truth on the same embedding model").

## D4 — p50/p95 латентность
Поднимаем Rust MCP-сервер in-process, маленький Rust-инджестнутый корпус (НЕ Go DB),
гоняем 12 инструментов через `McpClient`, собираем `TimingStats`. Go-бейзлайны
(search p50≈106.8ms, p95≈108.9ms — из Go benchmark на knowledge.db). Гейты:
p50(search) <= 2×, p95(search) <= 5× Go-бейзлайна.

## D5 — Не открывать Go knowledge.db / не менять продакшн
Фикстура — только SYNX `vectors.bin` (committed, ~415KB). `knowledge.db` НЕ коммитим и
НЕ открываем в Rust. Продакшн-код не меняется. Размерность фикстуры 384 (bge-small-en-v1.5
из Go) ≠ продуктовый default 1024 (bge-m3-int8) — не конфликтует: parity-тест сам
конфигурирует движок под dim фикстуры.

## Oracle references
- `../synopsis/data/knowledge.db` (read-only источник)
- `../synopsis/bin/synopsis` (бейзлайны латентности)
- `openspec/specs/data-schema/spec.md:39` (recall@10 >= 0.95)
- `crates/vectors/src/synx.rs` (SYNX reader + golden test)

## Non-goals
- Не менять продакшн-код crates/* (кроме parity-harness).
- Не коммитить Go `knowledge.db`.
