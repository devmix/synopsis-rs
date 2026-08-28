# Fixture: vectors.bin (SYNX vector dump)

Real vector dump used as the input for the parity-harness recall@k
differential test (change `parity-harness-real-fixtures`, task 1.1). The
binary is committed (415 KB); this README is the provenance record.

## Provenance

| Field | Value |
|---|---|
| Source | vec0 table (`chunks_vec`, `FLOAT[384]`) of the Go oracle knowledge base `../synopsis/data/knowledge.db` (read-only, ~2.2 MB, 270 chunks) |
| Extraction | One-off Go extractor (sqlite-vec-go-bindings, `CGO_ENABLED=1`) run under `/tmp` on 2026-08-28; the oracle tree was not modified |
| Format | SYNX v1 (contract fixed by change `native-seam-spikes`, D4; reader/writer + golden-bytes test in `crates/vectors/src/synx.rs`): header 20 bytes = `magic "SYNX"` + `version u32 LE = 1` + `dim u32 LE` + `count u64 LE`; body = `[chunk_id u32 LE][f32 LE × dim]` × count, sorted ascending by chunk_id |
| Size | 415,820 bytes = 20 + 270 × (4 + 384 × 4) |
| sha256 | `8320a6698553401cb3e490c59bec0fb3d27628f5d2737fd1562a344e21902e89` |
| Header (verified) | magic `SYNX`, version 1, dim 384, count 270 |
| Rows | 270 vectors, chunk_id 1..=270, ascending |

## Rules

- The Go `knowledge.db` itself is **not** committed to this repo and the
  Rust product code **never opens it** (legacy DB rule, design D5 of
  `native-seam-spikes`): Rust always builds its own DB from scratch. Only
  the extracted SYNX dump is used.
- Dimensionality note: the fixture is 384-dim (the Go oracle's
  bge-small-en-v1.5 vec0 embeddings), while the product default is 1024-dim
  (bge-m3-int8). This is not a conflict: the parity test configures the ANN
  engine with the fixture's own dim (design D5).

## Regeneration (if the file is ever lost)

Rebuild the small Go extractor under `/tmp` (sqlite-vec-go-bindings,
`CGO_ENABLED=1`), read vec0 from `../synopsis/data/knowledge.db` (read-only),
and write the rows as SYNX in the format above. Do not commit the Go
`knowledge.db` and do not open it from any Rust code.
