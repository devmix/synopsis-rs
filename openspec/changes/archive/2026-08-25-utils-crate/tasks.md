# Tasks: utils-crate

Зависимости: нет (лист-крейт). Директива человека 2026-08-25 (binding): готовые
библиотеки дат/времени во всём проекте; обобщённый крейт утилит.
Дизайн: design.md D1–D5. Гейты каждой задачи: `cargo fmt --check`,
`cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo test -p <crate>`;
workspace-тест и cargo doc гоняет ТОЛЬКО оркестратор.
Директива (binding): НЕ копировать Go 1:1; DRY/KISS/SOLID/YAGNI.

- [x] 1.1 Крейс utils + модуль temporal на jiff
  - Цель: единая точка работы с датами.
  - Scope файлов: `crates/utils/{Cargo.toml,src/lib.rs,src/temporal.rs}` (новые),
    корневой `Cargo.toml` (+workspace member, +jiff в палитру).
  - Содержание: design D1–D3 — jiff (default-features=false, features=["std"];
    проверить сборку), API now_rfc3339 / format_rfc3339 / format_backup_stamp /
    normalize_to_rfc3339 / parse_epoch_seconds; семантика байт-в-байт от
    заменяемого кода (design D3).
  - Тесты: переезд таблицы паритета normalize (20 кейсов из search/enrich.rs),
    SQLite-лейаут + дробные секунды + 'z'→'Z' + мусор/пусто → None,
    round-trip format↔parse, backup-stamp формат (включая миллисекунды).
  - Критерии приёмки: гейты utils зелёные.

- [x] 1.2 Миграция search + ingestion на utils::temporal

- [x] 1.3 Миграция embedding на utils::temporal
  - Цель: устранить пятую рукописную точку (эскалация задачи 1.2, подтверждена ревью).
  - Scope файлов: `crates/embedding/src/library.rs`, `crates/embedding/src/model.rs`
    (если затронут), `crates/embedding/Cargo.toml` (+utils).
  - Содержание: удалить rfc3339_now/rfc3339_from_secs (Hinnant civil-from-days,
    константы 719_468/146_097) из library.rs → utils::temporal::{now_rfc3339,
    format_rfc3339}; потребители installed_at в манифесте кэша моделей — выходные
    строки байт-идентичны; ассерты потребительских тестов не менять.
  - Тесты: потребительские тесты embedding зелёные без изменения ассертов;
    grep-гейт: rfc3339_now/rfc3339_from_secs/civil-from-days константы отсутствуют.
  - Критерии приёмки: гейты embedding зелёные; рукописного кода дат в workspace не осталось.
  - Цель: удалить все рукописные реализации.
  - Scope файлов: `crates/search/src/{enrich.rs,rerank.rs,lib.rs}`,
    `crates/search/Cargo.toml` (+utils),
    `crates/ingestion/src/parsers/mod.rs`, `crates/ingestion/src/ingester/backup.rs`,
    `crates/ingestion/Cargo.toml` (+utils).
  - Содержание: design D4 — enrich.rs: удалить normalize_updated_at/format_rfc3339,
    вызывать utils (pub(crate) экспорт в rerank.rs скорректировать); rerank.rs:
    удалить parse_timestamp/days_from_civil → parse_epoch_seconds;
    parsers/mod.rs: format_rfc3339_utc делегирует в utils (сигнатуру сохранить —
    facts.rs/cleanup.rs не трогать); backup.rs: civil_from_days → format_backup_stamp.
    ВСЁ рукописное удалить, не оставлять fallback.
  - Тесты: потребительские тесты остаются без изменения ассертов (зелёные = доказательство
    байт-совместимости); grep-проверка отсутствия civil_from_days/days_from_civil/
    рукописных парсеров в workspace.
  - Критерии приёмки: гейты search И ingestion зелёные; рукописного кода дат не осталось.
