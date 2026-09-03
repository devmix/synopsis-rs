# Proposal: fix-sse-docs

## Problem

The legacy MCP SSE transport was **re-added** by human decision **2026-08-31**
(reversal of design D8), implemented as a hand-rolled layer in
`crates/mcp/src/transport/sse.rs`, and shipped under change
`add-legacy-sse-transport` (archived 2026-09-01; wire contract **mcp-go v0.57.0**).
The server now serves a **double transport**: Streamable HTTP (rmcp 3.x) **and**
the oracle's legacy HTTP+SSE (`GET /sse` + `POST /message?sessionId=`).

The `mcp-contract` spec already documents this correctly ("Транспорт двойной:
Streamable HTTP + legacy HTTP+SSE оракула, override D8"). But three top-level
docs still carry the **stale** pre-2026-08-31 claim that legacy SSE was
"deliberately not preserved / intentionally not reproduced":

- `AGENTS.md` line 27 (frozen-stack bullet): "…legacy SSE transport is
  **deliberately not preserved**".
- `AGENTS.md` line 70 (Gotchas): "…do NOT implement the oracle's legacy SSE …
  intentionally dropped by human decision 2026-08-18".
- `README.md` line 14 (Stack): "…the oracle's legacy SSE transport is
  intentionally not reproduced — design D8".
- `openspec/config.yaml` line 24 (frozen-stack, Russian): "…legacy SSE оракула
  намеренно не поддерживается".

## Solution

Correct those four statements to describe the **double transport**, citing the
actual provenance (human decision 2026-08-31, change `add-legacy-sse-transport`,
wire contract mcp-go v0.57.0). The `config.yaml` line is corrected **in Russian**
(the file is currently Russian and is fully translated later by change
`translate-to-english`); correcting the fact now keeps the SSE decision atomic
and avoids a transient factual inaccuracy in the binding context.

## Frozen contracts touched

**None.** This is a pure documentation correction. The MCP contract (the
`mcp-contract` spec) already correctly describes the double transport and is
**not** changed. No code, tool, CLI surface, data schema, or config-format
change. No dependency change.

## Non-goals

- The `mcp-contract` spec is **not** modified (it is already correct).
- `crates/mcp/src/transport/sse.rs` and all other code are **not** touched.
- `openspec/changes/archive/**` is **not** touched.
- The full Russian→English translation of `openspec/config.yaml` is **not** done
  here — that is change `translate-to-english`. This change only corrects the SSE
  fact on that one line (in Russian).
- `../synopsis` (Go oracle) is **not** touched.
