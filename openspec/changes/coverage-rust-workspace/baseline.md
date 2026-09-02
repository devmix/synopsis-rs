# Coverage baseline — coverage-rust-workspace

Per-crate **line-coverage** baseline recorded once for this change (measure-first,
Phase 2). No coverage gates (`--fail-under-*`) are applied; targets are documented in
`COVERAGE.md` (repo root).

## Reproduction

- Command (from the repo root): `cargo llvm-cov --workspace`
- Tool: `cargo-llvm-cov 0.9.0` (cargo-installed binary, dev-only — not in `Cargo.lock`)
- Toolchain: Rust 1.96.0 (`rust-toolchain.toml`), LLVM 19
- Setup: `rustup component add llvm-tools-preview` + `cargo install --locked cargo-llvm-cov`
- Recorded: 2026-09-02, commit `91695d3` (task 1.1 of this change)

The per-file table is the output of the command above (columns `Lines`, `Missed Lines`).
Aggregation below sums those columns per crate (`crates/<crate>/…`).

## Per-crate line coverage (product crates)

| Crate | Lines | Missed lines | Coverage |
|---|---:|---:|---:|
| `db` | 5,340 | 108 | 97.98% |
| `graph` | 2,732 | 70 | 97.44% |
| `llm` | 450 | 10 | 97.78% |
| `utils` | 226 | 5 | 97.79% |
| `ingestion` | 10,783 | 419 | 96.11% |
| `search` | 2,917 | 104 | 96.43% |
| `mcp` | 5,436 | 237 | 95.64% |
| `vectors` | 2,759 | 139 | 94.96% |
| `config` | 1,990 | 107 | 94.62% |
| `embedding` | 2,586 | 307 | 88.13% |
| `cli` | 6,022 | 1,391 | 76.90% |
| **Product total (11 crates)** | **41,241** | **2,897** | **92.98%** |

Coverage = (1 − missed/lines) × 100, computed from the summed `Lines` / `Missed Lines`
columns per crate.

## Excluded from targets

| Crate | Lines | Missed lines | Coverage | Note |
|---|---:|---:|---:|---|
| `parity-harness` | 1,111 | 85 | 92.35% | test infrastructure, not product code |

Std-library / toolchain paths appearing in the raw table are excluded from all
aggregation: one file
(`…/toolchains/1.96.0-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/std/src/sys/thread_local/native/mod.rs`,
6 lines, 3 missed) comes from the in-tree rust-src component and is not workspace code.

## Overall total

| Scope | Lines | Missed lines | Coverage |
|---|---:|---:|---:|
| All workspace crates (excl. std/toolchain) | 42,352 | 2,982 | **92.96%** |
| Product crates only | 41,241 | 2,897 | 92.98% |

## Residual-gap focus (two lowest crates)

- **`cli` — 76.90% (1,391 missed lines):** the single largest gap. Concentrated in the
  loadtest runner/filler paths (`loadtest/runner.rs`, `loadtest/filler.rs` are 0% —
  exercised only by real `loadtest` runs), `serve/server.rs` (79.33%), `model.rs`
  (81.51%), and `serve/bootstrap.rs` (71.07%). Integration-heavy dispatch; hard to
  unit-test without a full serve/sync environment.
- **`embedding` — 88.13% (307 missed lines):** ONNX runtime lifecycle and FFI error
  paths (`runtime.rs` 60.78%, `provider.rs` 84.30%, `library.rs` 87.12%, `lib.rs`
  80.98%); real `.so` loading and provider fallbacks are not unit-testable.

Every other crate is ≥ 94.62%. Both crates are already **above** their `COVERAGE.md`
target bands (50–60%), so the follow-up value is targeted gap-closing (or raising the
targets), not reaching a floor.

## Consistency note

These numbers match the planning smoke test recorded in `design.md` (Context) within
≤ 0.05 pp per crate (that run: overall ~93%, 42,352 lines / 2,983 missed; this run:
42,352 lines / 2,985 missed including the one std file, 2,982 excluded).
