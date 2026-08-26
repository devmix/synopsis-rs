# Proposal: utils-crate

## Change name
`utils-crate`

## Why

Human decision 2026-08-25 (binding): the project must use ready-made date/time
libraries everywhere instead of hand-rolled implementations; shared helpers go
into a generalized utility crate. Four hand-rolled sites accumulated across
search and ingestion (~400 lines): an RFC3339/SQLite-layout timestamp parser, a
hand-written RFC3339 formatter, and two civil-from-days calendar math copies —
one of which already caused a masked bug that hung two agents (task 4.4
recovery).

Library verified per migration principles (crates.io + docs.rs, Aug 2026):
**jiff 0.2.35** — actively maintained (BurntSushi, release 2026-07), modern
Temporal-inspired API, built-in RFC3339 plus `fmt::strptime` covering the SQLite
`%Y-%m-%d %H:%M:%S` layout, no CVE history, MSRV compatible with the pinned
toolchain. chrono 0.4.45 evaluated as the alternative.

## What changes

1. **New crate `crates/utils`** — generalized utility home for cross-cutting
   helpers; first module `temporal` wraps jiff thinly:
   `now_rfc3339`, `format_rfc3339(SystemTime)`, `format_backup_stamp(SystemTime)`
   (`%Y-%m-%dT%H-%M-%S-<ms>` naming for VACUUM snapshots),
   `normalize_to_rfc3339(&str)` (accepts RFC3339 and the SQLite CURRENT_TIMESTAMP
   layout), `parse_epoch_seconds(&str) -> Option<i64>`.
2. **Migration of all four sites**: search/enrich.rs and search/rerank.rs delete
   their parsers/formatters and call utils; ingestion/parsers/mod.rs
   `format_rfc3339_utc` delegates to utils (facts.rs/cleanup.rs consumers keep
   working); ingestion/ingester/backup.rs replaces civil_from_days stamp
   building with `format_backup_stamp`.
3. **Frozen stack entry**: jiff added by this human decision; recorded in the
   change design and AGENTS-relevant docs via the design file.

## Non-goals

- No behavior changes: outputs stay byte-identical (parity tests move with the
  code); the SQLite-layout acceptance deviation from task 4.4 is preserved.
- No new features in utils beyond what the four call sites need (YAGNI).
- SQL string literals (CURRENT_TIMESTAMP defaults in db DAOs) are not touched.
- No timezone-aware features — everything stays UTC (jiff default features may
  be trimmed if they pull unneeded tzdb weight; decide at implementation).

## Risks

- jiff pulls tz-database machinery by default — feature selection at
  implementation must keep the musl cross-builds lean.
- Byte-identical output must be proven by the moved parity tables (13-case
  normalization table, boundary cases).
