# Tasks: fix-sse-docs

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml` (binding
context). This change **corrects stale SSE-transport documentation** to reflect
the double transport that was re-added by human decision 2026-08-31 (change
`add-legacy-sse-transport`, wire contract mcp-go v0.57.0). **No code, spec,
contract, or dependency changes. `../synopsis` and `openspec/changes/archive/**`
must stay untouched.**

## 1

- [ ] 1.1 Correct the four stale SSE statements (AGENTS.md, README.md, config.yaml)

**Goal.** Replace the false "legacy SSE deliberately not preserved / not
reproduced" claims with the true **double-transport** state, citing real
provenance.

**Scope.** Exactly four lines across three files:

- `AGENTS.md` line 27 (frozen-stack bullet) — currently:
  `- rmcp 3.x — official MCP SDK over Streamable HTTP (design D8); wire compatibility with the oracle's legacy SSE transport is **deliberately not preserved**`
  → reword to state the oracle's legacy HTTP+SSE transport is **also** served
  (double transport — override of D8, human decision 2026-08-31, change
  `add-legacy-sse-transport`; wire contract mcp-go v0.57.0). Keep the rest of the
  bullet (rmcp 3.x / Streamable HTTP / design D8).

- `AGENTS.md` line 70 (Gotchas, "MCP transport") — currently begins
  `Streamable HTTP via rmcp — do NOT implement the oracle's legacy SSE
  (`GET /sse` + `POST /message?sessionId=`); it is deprecated and was
  intentionally dropped by human decision 2026-08-18.`
  → reword to state the transport is now **double**: Streamable HTTP via rmcp
  **and** the oracle's legacy HTTP+SSE (`GET /sse` + `POST /message?sessionId=`),
  re-added by human decision 2026-08-31 (change `add-legacy-sse-transport`; wire
  contract mcp-go v0.57.0) after D8 originally dropped it. **Keep** the trailing
  sentence "Parity lives at the level of tool responses, compared against
  fixtures recorded once from the Go binary."

- `README.md` line 14 (Stack) — currently:
  `rmcp 3.x over Streamable HTTP for MCP (the oracle's legacy SSE transport is
  intentionally not reproduced — design D8)`
  → reword the parenthetical to state the oracle's legacy HTTP+SSE is **also**
  served (double transport — override of design D8 by human decision 2026-08-31,
  change `add-legacy-sse-transport`; wire contract mcp-go v0.57.0). Keep the rest
  of the line.

- `openspec/config.yaml` line 24 (frozen-stack, **Russian**) — currently:
  `  - rmcp 3.x — MCP протокол (Streamable HTTP, design D8; legacy SSE оракула намеренно\n    не поддерживается); cel-interpreter — ...`
  → correct the fact **in Russian** to state double transport: Streamable HTTP
  (design D8) + legacy HTTP+SSE оракула (override D8, решение человека 2026-08-31,
  change `add-legacy-sse-transport`, wire-контракт mcp-go v0.57.0). Keep the rest
  of the bullet (cel-interpreter, tokenizers, notify, indicatif). Do **not**
  translate the file — only fix this one line's fact (full translation is change
  `translate-to-english`).

**Dependencies.** None. (The double transport is already implemented and
specified; this only fixes the docs.)

**Acceptance.**
1. `rg -n "deliberately not preserved|intentionally not reproduced|намеренно\n *не поддерживается" AGENTS.md README.md openspec/config.yaml`
   → **zero** matches (the stale claims are gone).
2. All four corrected lines cite the double transport and the provenance
   (`add-legacy-sse-transport` / 2026-08-31 / mcp-go v0.57.0).
3. `openspec/config.yaml` is still valid YAML: `python3 -c "import yaml,sys; yaml.safe_load(open('openspec/config.yaml'))"`
   (or `openspec validate` if it parses the context) exits clean.
4. No code, spec (`mcp-contract`), or dependency files touched; `../synopsis` and
   `openspec/changes/archive/**` untouched; `cargo` gates unaffected (no Rust
   files changed) — confirm `git diff --name-only` shows only AGENTS.md,
   README.md, openspec/config.yaml.
