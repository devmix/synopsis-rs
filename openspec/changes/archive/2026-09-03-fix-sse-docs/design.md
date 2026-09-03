# Design: fix-sse-docs

## Context note

Pure documentation correction. The double-transport behavior is already
implemented (`crates/mcp/src/transport/sse.rs`) and already correctly specified
in `openspec/specs/mcp-contract/spec.md` ("Транспорт двойной: Streamable HTTP +
legacy HTTP+SSE оракула, override D8"). Only the three top-level docs are stale.
`../synopsis` is not touched.

## Provenance (verified)

- The legacy SSE transport was re-added by **human decision 2026-08-31** (a
  reversal of design D8, which had dropped it on 2026-08-18), shipped under
  change **`add-legacy-sse-transport`** (archived `2026-09-01-add-legacy-sse-transport`).
- Wire contract: **mcp-go v0.57.0** (the oracle's `SSEServer`).
- `crates/mcp/src/transport/sse.rs` exists and is wired in (double transport,
  both always on, no flags).
- `openspec/specs/mcp-contract/spec.md` lines 5 and 31 already state the double
  transport correctly — no spec change needed.

## Decisions

### D1 — Correct all four stale statements to "double transport", citing real provenance

Each stale line is rewritten to say the server serves **both** Streamable HTTP
and the oracle's legacy HTTP+SSE, and to cite the actual decision
(2026-08-31, change `add-legacy-sse-transport`) and wire contract (mcp-go
v0.57.0). This replaces the false "deliberately not preserved / intentionally not
reproduced" claims with the true state.

*Why cite the change + date:* the whole point of this change is an auditable
correction of a reversed decision; leaving the line vague ("SSE is also served")
would lose the provenance that the transport is a deliberate human override of
D8, not the original D8 behavior.

### D2 — `config.yaml` corrected in Russian (fact only); full translation deferred

`openspec/config.yaml` is currently Russian and will be fully translated by
change `translate-to-english`. Correcting the SSE line **now, in Russian**, keeps
the SSE decision atomic (all four stale statements fixed in one change) and
prevents a transient state where the binding context still claims SSE is "not
supported". The `translate-to-english` change will then carry the corrected
statement into English.

*Why not defer the config.yaml fix to the translation change:* deferring would
(a) split the SSE audit trail across two changes and (b) leave a factually wrong
statement in `config.yaml` between the two changes. The cost of one throwaway
Russian edit is cheaper than both.

### D3 — The `mcp-contract` spec is untouched

The spec is the authoritative contract and is already correct. Re-stating the
transport in the spec is unnecessary and would risk drift. The three top-level
docs are the only stale surfaces.

## Oracle references

None. `../synopsis` is not read or modified; the wire-contract reference
(mcp-go v0.57.0) is cited as provenance text, not a file dependency.
