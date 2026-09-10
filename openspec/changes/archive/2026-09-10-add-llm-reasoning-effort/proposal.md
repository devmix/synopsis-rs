# Proposal: add-llm-reasoning-effort

## Why

The LLM that powers NER and cross-domain entity linking is gpt-oss-20b — a
reasoning model whose default reasoning effort is `medium`. The NER request
sends no reasoning control and a large `max_tokens` budget, so each page of
extraction spends 70–120+ seconds in internal reasoning (measured: 73.6 s for
one ~12k-token page; `medium` loops toward the token cap and was observed
truncating mid-JSON at 8+ minutes). Full-corpus ingestion is therefore
impractically slow, and the same unbounded reasoning applies to the linker.
There is currently no way in the config to cap the model's reasoning effort
per LLM consumer.

## What Changes

- **New (config format):** an optional `reasoning_effort` key in the
  per-consumer LLM config (`ner.llm`, `linker.llm`, and any `llm:` block).
  A string; empty or absent → the field is not sent (current behavior,
  unchanged). This is the frozen-contract change (see "Frozen contracts
  touched").
- **New (LLM client):** when `reasoning_effort` is set, the request body
  includes a top-level `reasoning_effort` field. When empty, the request body
  is byte-identical to today's. (Top-level field chosen over
  `chat_template_kwargs.reasoning_effort`: both work identically on the tested
  llama.cpp server, but the top-level field is the OpenAI-compatible,
  provider-portable form.)
- **Config values:** `ner.llm.reasoning_effort: low` and
  `linker.llm.reasoning_effort: low` in the shipped presets
  (`config.default.yaml`, `config.demo.yaml`); NER `max_tokens` → 8192
  (safety bound — kills the long-reasoning hang). The shipped preset's LLM
  endpoint / model / logging are also updated to the local gpt-oss setup
  (committed as part of this change).
- **NER system prompt:** a soft exhaustiveness directive added to the
  "Entity descriptions" section (workspace override + embedded default), so
  `low` effort preserves full recall. Plain `low` was measured to drop
  explicitly-named entities (one referenced only via a wiki-link, one only in
  the HTML); the soft nudge recovers full recall at ~41 s vs ~90 s for
  `medium`. A strong / penalty-framed nudge was measured to cause truncation
  → invalid JSON and is **not** used. The pre-existing prompt divergence (the
  "Description must be concise: 200-500 characters" line present only in the
  embedded copy) is **reconciled** — the line is removed from the embedded
  copy so both prompt copies match.
- **Docs** updated in the same change: `site/docs/reference/config-schema.mdx`
  (the new key) and the LLM/NER guides as applicable.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `llm-client`: the request body gains an optional top-level
  `reasoning_effort` field; empty → the field is omitted and the request body
  is unchanged.
- `config-format`: the per-consumer LLM config gains an optional
  `reasoning_effort` key (empty/absent = not sent).
- `entity-extraction`: the NER system prompt carries an exhaustiveness
  directive (extract every explicitly-named entity, including wiki-link-only
  references) so extraction at a low reasoning effort stays complete.

## Frozen contracts touched

- **Config format** (frozen): one optional key, `reasoning_effort`, added to
  the LLM config. It is additive — a config that omits it behaves exactly as
  today (the field is not sent), and unknown keys already do not break
  startup, so old presets keep working. Parity is confirmed by the config
  parse tests, the new client request-serialization tests (set → present,
  empty → absent), and the shipped presets producing the same effective
  request body as before when the key is unset.
- **CLI surface / MCP tools / data schema**: not touched.

## Non-goals

- No change to LLM response parsing, error handling, or retry logic (the
  `EmptyContent` / `Truncated` paths are unchanged).
- No per-request reasoning override at call sites (NER / linker read it from
  config only; no new API surface on `LlmClient`).
- No change to `max_tokens` for the linker (only NER's is set to 8192).
- No new LLM provider; gpt-oss stays on the existing OpenAI-compatible
  endpoint.
- No change to the ONNX embedding path or the vector index.
