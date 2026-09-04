# Design: cleanup-oracle-references

## Context note

The Go project at `../synopsis` **will be deleted**. The user therefore decided
(2026-09-03) that the Rust project's documentation should no longer reference it
or frame itself as a port of it: the docs should describe a **standalone native
Rust codebase**. This is broader than the original "remove `../synopsis` paths"
scope — it also removes the *migration narrative* (oracle / Go / ported).

Scope: all **living** documentation — crate doc comments (`crates/*/src/**`),
crate `Cargo.toml` comments, `AGENTS.md`, `README.md`, `openspec/config.yaml`,
`docs/adr/**`, and `openspec/specs/**`. **`openspec/changes/archive/**` is NOT
touched** — it is the historical audit trail and must not be rewritten.

All crate references are `//!`/`///` doc comments or `#` Cargo.toml comments —
no code-level usage (verified: no `Path::new`, no `include_str!`, no string
literal). So crate edits are comment-only; `cargo fmt/clippy/test` stay green by
construction.

## The rule (uniform across all tasks)

Remove **all migration-provenance** from living docs. Concretely:

**REMOVE:**
- Any reference to the Go project / oracle: `the oracle`, `Go oracle`,
  `Go original`, `Go code`, `Go service`, `Go project`, `Go binary`,
  `the original`, and any `../synopsis/...` path.
- Go source file names: `*.go`, `*.tmpl` (e.g. `rrf.go`, `tools.go`,
  `library.go`, `ner.go`) when they reference the Go source.
- Port/migration language: `ported`, `port of`, `faithful port`,
  `re-architected`, `not transcribed`, `functional copy`, `migration`,
  `migration principle(s)`, `transcribed from`, `verified against the oracle`,
  `deviations from the oracle`, `parity with the oracle`, `parity-checked`.
- Whole sections that only exist for the migration: AGENTS.md `## Oracle` and
  `## Migration principles`.

**KEEP (reframe where needed):**
- The design rationale / WHY, reframed as a native Rust decision (drop the
  "from the oracle" framing): e.g. "silent defaults are replaced by fail-fast
  validation"; "CLS pooling takes the first hidden state"; "downloads go
  through a retrying, SSRF-protected client"; "the index is disk-backed and
  quantized for the 16 GB laptop constraint".
- Behavioral and algorithm descriptions (`score += 1 / (k + rank)`, retry
  counts, timeouts, PRAGMA lists).
- The project's own design-decision references: `D1…D8`, `ADR 0001…0005`
  (these point at the Rust repo's `docs/adr/**` and the archive's design docs —
  both kept).
- Wire-format version identifiers such as `mcp-go v0.57.0` (a protocol version,
  not a reference to the deleted project) — but drop "the oracle's" around it.

**Reframe example:**
- before: `//! Deliberate deviations from the oracle: silent defaults are
  replaced by fail-fast validation`
- after:  `//! Design: silent defaults are replaced by fail-fast validation`
- before: `//! Faithful port of the oracle's `rrf.go` — every numeric behavior is
  preserved`
- after:  `//! Reciprocal Rank Fusion; every numeric behavior is preserved`

## Decisions

### D1 — Strip the narrative, keep the rationale

The goal is to delete the *provenance* (that this is a port of a Go project),
not the *design rationale* (why the code behaves as it does). Deviation notes
become plain design decisions; "faithful port of X.go" becomes a description of
the algorithm/behavior. The reviewer checks that no design rationale is lost and
that no oracle/Go/ported language remains.

### D2 — The archive is history, not living docs

`openspec/changes/archive/**` records the decisions as they were made (including
the migration framing). Rewriting it would falsify the audit trail, so it is
explicitly out of scope even though it is full of oracle references.

### D3 — Russian docs: strip narrative now, translate later

`openspec/config.yaml`, `docs/adr/**`, and `openspec/specs/**` are currently
Russian and will be translated to English by change `translate-to-english`.
This change removes the oracle/Go/ported narrative **in Russian** (content
decision); the translation (language change) is a separate concern and stays in
`translate-to-english`.

### D4 — Drop now-stale parity-harness clauses

Change `remove-parity-harness` deleted the differential-harness crate. Any line
asserting "differential parity with the oracle is an acceptance criterion" (or
"parity was machine-checked during the migration") is now false and is removed.

## Oracle references

None. `../synopsis` is not read or modified; its references are being removed
from all living docs, and the historical record in the archive is preserved.
