# Design: utils-crate

Oracle references: none — this is a Rust-side infrastructure change motivated by
the human directive of 2026-08-25 (ready-made date/time libraries only).

## D1 — Generalized crate `crates/utils`, module-per-concern

```
crates/utils/src/
  lib.rs       — crate docs; re-exports
  temporal.rs  — date/time helpers over jiff (first module)
```

The crate is the project's home for cross-cutting helpers that multiple crates
need but which belong to no domain crate. Dependency position: a leaf like
config — any crate may depend on it; it depends on nothing internal. Future
modules (e.g. fs helpers, string helpers) land here following the same pattern.

## D2 — Library: jiff 0.2.x, feature-trimmed

jiff pinned in the workspace palette. Default features pull tz-database
machinery this project never uses (everything is UTC); implementation enables
`default-features = false, features = ["std"]` and must verify the musl
cross-build stays lean. Parsing/formatting needs:

- RFC3339 parse/format: `jiff::Timestamp` FromStr/Display;
- SQLite layout `%Y-%m-%d %H:%M:%S` (UTC): `civil::DateTime::strptime`;
- backup stamp `%Y-%m-%dT%H-%M-%S-<ms>`: civil formatting via strftime-style
  tokens + millisecond component.

## D3 — Public API (thin, exactly what call sites need)

```rust
pub fn now_rfc3339() -> Option<String>
pub fn format_rfc3339(t: SystemTime) -> Option<String>
pub fn format_backup_stamp(t: SystemTime) -> String        // infallible: clamped
pub fn normalize_to_rfc3339(value: &str) -> Option<String> // RFC3339 | SQLite layout -> RFC3339
pub fn parse_epoch_seconds(value: &str) -> Option<i64>     // same inputs -> epoch s
```

Semantics preserved from the hand-rolled code being deleted:
- `normalize_to_rfc3339`: RFC3339 pass-through (canonicalized), SQLite layout
  interpreted as UTC, fractional seconds ≤9 digits, lowercase 'z' normalized,
  empty/garbage → None (13-case parity table moves from enrich.rs);
- `parse_epoch_seconds`: same acceptance set → epoch seconds (reranker freshness/
  expiry comparisons);
- `format_backup_stamp`: `%Y-%m-%dT%H-%M-%S-<ms>` UTC, the VACUUM snapshot naming
  contract from ingestion-pipeline task 3.6.

## D4 — Migration map

| Site today | Becomes |
|---|---|
| search/enrich.rs `normalize_updated_at` + `format_rfc3339` | `utils::temporal::{normalize_to_rfc3339, format_rfc3339}` |
| search/rerank.rs `parse_timestamp` + `days_from_civil` | `utils::temporal::parse_epoch_seconds` |
| ingestion/parsers/mod.rs `format_rfc3339_utc` | delegates to `utils::temporal::format_rfc3339` (signature kept; facts.rs/cleanup.rs untouched) |
| ingestion/ingester/backup.rs `civil_from_days` stamp building | `utils::temporal::format_backup_stamp` |

All hand-rolled implementations and their helper math are DELETED, not kept as
fallbacks. The moved parity tables (13-case normalization, boundary ±60s,
round-trips) re-home in crates/utils tests; consumer crates keep their
integration-level tests unchanged.

## D5 — Verification gates

Per-crate gates for utils/search/ingestion; the orchestrator runs the workspace
suite and cargo doc. Byte-identity is proven by: (a) moved parity tables green in
utils, (b) unchanged consumer tests green (they assert on output strings).
