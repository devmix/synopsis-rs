# .archive/spikes — archived native-seam prototype crate

One-shot spike crate from change `native-seam-spikes` (design D1). Archived in task 5.1
via `git mv crates/spikes .archive/spikes` per the human decision of 2026-08-18: spikes
are **archived, not deleted** — the measurement code may be re-run when the `vectors`
change lands (e.g., repeat S3 on a real fixture instead of synthetic data, design D2).

## Provenance

| Item | Origin |
|---|---|
| Crate skeleton + stubs | task 0.2 |
| `probe_db.rs` | task 0.1 one-shot provenance probe for `fixtures/knowledge.db` (reused by s1_sqlite) |
| `s1_sqlite.rs` (S1: SQLite/FTS5 seam) | task 1.1 — **ADR 0001** (`docs/adr/0001-sqlite-fts5.md`) |
| `s2_onnx.rs` (S2: ONNX Runtime, all 3 registry models) | task 2.1 — **ADR 0002** (`docs/adr/0002-onnx-runtime.md`) |
| `s3a_usearch.rs` | stub only — spike 3.1 was cancelled (usearch abandoned in favor of lancedb) |
| `s3b_lance.rs` (S3: ANN engine measurements) | task 3.2 — **ADR 0003** (`docs/adr/0003-ann-engine.md`) + appendix `docs/adr/spike-s3-results.md` |

The ADRs are the single source of truth for every decision; this crate is measurement
code only and is **not part of the product dependency graph**. Reusable logic was
rewritten (or will be rewritten) in the product crates, never ported from here.

## Standalone build

This is deliberately NOT a workspace member: it builds with its own `Cargo.toml`
(explicit edition/lints/pins; pins moved out of the root `[workspace.dependencies]`
palette in task 5.1, each referencing the ADR that fixed them) and its own committed
`Cargo.lock`, so re-runs are reproducible without touching the workspace resolve graph.

```sh
# from the repository root:
cargo build --manifest-path .archive/spikes/Cargo.toml
cargo run   --release --manifest-path .archive/spikes/Cargo.toml --bin s3b_lance
```

## How to re-integrate into the workspace (if ever needed)

1. `git mv .archive/spikes crates/spikes` (or leave it in place and point at it).
2. Delete the empty `[workspace]` table from its Cargo.toml, then add the crate path back to
   `members` in the root `Cargo.toml`.
3. Either restore `workspace = true` inheritance here, or move the pins from this file's
   `[dependencies]` back into the root `[workspace.dependencies]` palette (product crates
   `db`/`embedding`/`vectors` are expected to create their own pins in their own changes,
   per ADRs 0001/0002/0003).

## Caveat: relative paths

The binaries resolve several runtime inputs **against the current working directory**, not
the crate location — run them from the repository root:

- `fixtures/knowledge.db` (S1, probe_db) — gitignored fixture; provenance in `fixtures/README.md`.
- `data/onnxruntime/libonnxruntime.so.*` + model files under `data/models/<name>/` (S2) —
  per the oracle layout from `../synopsis/configs/onnx.yaml`; see `data/README.md`.
- `../synopsis/configs/onnx.yaml` (S2 registry reference path).
- S3b writes its index work dir to `/tmp/opencode/s3b-lance` (override via env var in source).

Only the migrations directory is location-independent: it is embedded at compile time via
`include_dir!("$CARGO_MANIFEST_DIR/migrations")`. If this crate is ever moved or re-integrated
under a different layout, the CWD-relative paths above must be revisited.
