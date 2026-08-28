# Proposal: parity-harness-real-fixtures

## Change name
`parity-harness-real-fixtures`

## Why
`crates/parity-harness/src/fixtures.rs` пишет `b"stub"` для `vectors.bin` — машинный
паритет с Go-оракулом НЕ валидирован. Нужны реальные фикстуры (SYNX, извлечены из vec0
Go `knowledge.db`) и дифф-тесты recall@k + p50/p95 латентности.

## What changes
1. `fixtures.rs` — заменить stub на загрузчик SYNX + recall@k тест (recall@10 >= 0.95).
2. `crates/parity-harness/fixtures/vectors.bin` — реальная фикстура (dim=384, 270 векторов).
3. p50/p95 латентность-тест 12 MCP-инструментов vs Go-бейзлайны.

## Non-goals
- Не открывать Go `knowledge.db` в Rust (legacy DB rule).
- Не менять продакшн-код crates/* (кроме parity-harness).

## Deviations
- Фикстура dim=384 (bge-small-en-v1.5 из Go) ≠ продуктовый default 1024 (bge-m3-int8) —
  parity-тест сам конфигурирует dim фикстуры (не конфликтует с продуктом).
