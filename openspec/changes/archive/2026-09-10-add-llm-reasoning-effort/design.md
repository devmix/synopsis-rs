# Design: add-llm-reasoning-effort

## Context

See proposal.md for motivation. The relevant current state:

- `LlmConfig` (`crates/config/src/preset.rs:676`) is the per-consumer LLM
  settings struct shared by NER (`ner.llm`) and the linker (`linker.llm`).
  It derives `Deserialize` with a struct-level `#[serde(default)]`, so an
  absent key already deserializes to the field's `Default` — adding a field is
  additive and backward compatible.
- `LlmClient` (`crates/llm/src/client.rs`) holds a `config: LlmConfig`
  (built once in `LlmClient::new`) and builds the wire body in
  `build_request_body` (line 401) from a `RequestBody` struct (line 525) whose
  fields are `model`, `messages`, `temperature`, `seed`, `max_tokens`,
  `response_format`. All are always serialized.
- A captured NER request (a recorded pre-change wire body) is the reference
  for the byte-identity requirement: the pre-change body carries exactly
  `model`, `messages`, `temperature`, `seed`, `max_tokens`, `response_format`.
- The NER system prompt is rendered from the workspace override
  (`workspace/configs/prompts/ner/system.tmpl`) with the embedded default
  (`crates/ingestion/src/ner/templates/system.tmpl`) as fallback. The two
  files are currently divergent on one line (the "Description must be concise:
  200-500 characters" rule is in the embedded copy but not the workspace
  copy); this change reconciles that divergence (see D6).

## Goals / Non-Goals

**Goals:**

- A per-consumer `reasoning_effort` config key that, when set, is sent as a
  top-level `reasoning_effort` field in the chat-completions request.
- Byte-identical request body when the key is empty/absent (parity with the
  captured wire shape).
- Shipped presets drive gpt-oss to `low` effort (NER + linker) and set NER
  `max_tokens` to 8192.
- NER extraction stays complete at `low` effort via a soft exhaustiveness
  directive in the system prompt.

**Non-Goals:**

- No per-call reasoning override (no new `LlmClient` method; the value comes
  from config only).
- No client-side validation of effort levels (pass-through).
- No change to the linker `max_tokens`, response parsing, or retry logic.

## Decisions

### D1 — Top-level `reasoning_effort` field, not `chat_template_kwargs`

The request body carries `reasoning_effort` as a top-level field. On the
tested llama.cpp/gpt-oss server, `chat_template_kwargs.reasoning_effort`
behaves identically (verified empirically), but the top-level field is the
OpenAI-compatible, provider-portable form and is what this project's
"OpenAI-compatible endpoint" contract implies. Sending a nested
llama.cpp-specific key would couple the wire shape to one server
implementation.

- **Alternative A (`chat_template_kwargs.reasoning_effort`):** rejected —
  works on the tested server but is not the portable form; would need a
  server-specific branch to be portable.
- **Alternative B (top-level field):** chosen.

### D2 — `reasoning_effort: String` on `LlmConfig` (empty = not sent)

`LlmConfig` gains `pub reasoning_effort: String`. The struct-level
`#[serde(default)]` already maps an absent key to `String::default()` (`""`),
so no per-field serde attribute is needed and old presets parse unchanged.
An empty string is the "not set" sentinel (mirrors how `api_key` treats an
empty string as "not required").

- **Alternative A (`Option<String>`):** rejected — the rest of `LlmConfig`
  uses plain fields with `Default` sentinels; an `Option` would be the lone
  exception and adds `None`-handling in the client for no benefit.
- **Alternative B (`String`, empty sentinel):** chosen — consistent with the
  struct's existing style.

### D3 — Conditional serialization keeps the empty case byte-identical

`RequestBody` gains `reasoning_effort: String` annotated with
`#[serde(skip_serializing_if = "String::is_empty")]`. `build_request_body`
sets it from `self.config.reasoning_effort.clone()`. When empty the field is
omitted from the JSON, so the wire body is byte-identical to the pre-change
shape (the captured `ner-1-request.json`). The client does **not** validate or
reject effort levels — the value is passed through verbatim (the server owns
the allowed set; an unrecognized value is a server error, surfaced as a normal
HTTP error).

- **Alternative A (validate against a fixed set `{low, medium, high, ...}`):**
  rejected — the allowed set is server/model-specific and will grow; a
  closed enum in the client would false-reject valid values and is the kind
  of over-constraint the project's "tolerant" config style avoids.
- **Alternative B (pass-through, `skip_serializing_if`):** chosen.

### D4 — Config values: `low` on both consumers, NER `max_tokens` 8192

The shipped presets set `ner.llm.reasoning_effort: low` and
`linker.llm.reasoning_effort: low`. NER `max_tokens` is set to 8192 as a
safety bound (kills the long-reasoning hang if reasoning still runs long);
8192 is well above the observed `low`-effort output (~1355 tokens) while
still capping the worst case. The linker keeps its existing `max_tokens` (its
output is smaller and the hang risk is lower). The shipped preset's LLM
endpoint / model / logging are also updated to the local gpt-oss setup and
committed as part of this change (human decision: commit all working-tree
changes).

- **Alternative A (leave the budget unbounded):** rejected — the whole point
  is to bound the worst case; an unbounded budget defeats the fix.
- **Alternative B (8192):** chosen — generous headroom, hard cap.

### D5 — Soft exhaustiveness nudge, not a penalty

The NER system prompt gains one sentence in the entity-description section:
extract **every** explicitly-named entity in the chunk, including entities
referenced only via a wiki-link (`[[...]]`) or a bare name. It is phrased as a
soft instruction, not a penalty or hard constraint. This was measured: plain
`low` dropped two explicitly-named entities (one referenced only via a
`[[wiki-link]]`, one only in the HTML); the soft nudge recovered full recall
at ~41 s (vs ~90 s for `medium`), while a strong/penalty-framed nudge
truncated the response to invalid JSON. The nudge is added to **both** the workspace override and the
embedded default (the two must stay in sync — the embedded copy is the
fallback).

- **Alternative A (no nudge, accept the recall loss):** rejected — dropping
  explicitly-named entities is a correctness regression the user did not
  accept.
- **Alternative B (strong/penalty nudge):** rejected — measured to cause
  truncation → invalid JSON.
- **Alternative C (soft nudge):** chosen.

### D6 — The pre-existing prompt divergence is reconciled

The embedded NER prompt has a "Description must be concise: 200-500
characters" line that the workspace override lacks. This change **reconciles**
it (human decision): the line is removed from the embedded copy so both prompt
copies match, and the new exhaustiveness sentence is added to both (keeping the
content in sync). The runtime uses the workspace override, so the effective
prompt is unchanged by the removal; the embedded copy is the fallback and now
matches the override.

- **Alternative A (leave the divergence):** rejected — the user chose to
  reconcile it; leaving a known divergence contradicts the "prompts stay in
  sync" norm.
- **Alternative B (reconcile + add the nudge to both):** chosen.

### D7 — Tests

- **Client serialization** (`crates/llm/src/client.rs` + `crates/llm/tests/client.rs`):
  a new test asserts the request body **contains** `reasoning_effort` when the
  config sets it, and **omits** it (byte-identical to the pre-change body) when
  empty. This is the parity gate for the wire shape.
- **Test constructors:** the six `LlmConfig` literals that build a full struct
  (not via `Default`) must add the new field: `crates/llm/src/client.rs`
  (`valid_config`), `crates/llm/tests/client.rs` (`valid_config`),
  `crates/ingestion/src/ner/llm.rs` (`llm_config`),
  `crates/ingestion/src/ner/composite.rs` (`llm_config`),
  `crates/graph/src/linker.rs` (test config), and
  `crates/graph/tests/llm_linker_pipeline.rs` (config).
- **Config parse** (`crates/config`): a test that a preset with
  `reasoning_effort` set parses to the value, and one that a preset without it
  parses to `""`.
- **Prompt render** (`crates/ingestion`): a test that the rendered NER system
  prompt contains the exhaustiveness directive (both the workspace-override
  path and the embedded-fallback path).

Reference fixtures/contracts for this design: `openspec/specs/llm-client/spec.md`
("Reasoning effort control"), `openspec/specs/config-format/spec.md`
("LLM reasoning effort config"), `openspec/specs/entity-extraction/spec.md`
("NER prompt exhaustiveness directive"), and a captured NER request (recorded
pre-change wire body). No recorded MCP/CLI fixtures are affected (no tool or
CLI surface change).

## Risks / Trade-offs

- **Recall is model-dependent:** the nudge recovers recall on the tested
  gpt-oss-20b; a different model may need tuning. Mitigation: the directive is
  generic (extract every explicitly-named entity) and the config key lets a
  user raise the effort per consumer if a model regresses.
 - **`max_tokens` 8192 could truncate a very large page at `low`:** unlikely
   (observed ~1355 tokens) but possible for a dense page. Mitigation: a
   truncation is a non-retryable `LlmError::Truncated` (existing path) that
   surfaces clearly rather than silently; the bound is a deliberate trade for
  the 8-minute-hang fix.
- **Prompt reconciliation (D6):** the embedded NER prompt's "200-500
  characters" line is removed to match the workspace override. Mitigation: the
  runtime uses the workspace override, so the effective prompt is unchanged;
  the removal only affects the (now-matching) fallback copy.
- **Server ignores the field:** if a non-gpt-oss server does not understand
  top-level `reasoning_effort`, it is ignored (OpenAI-compatible servers
  ignore unknown fields) — no behavior change, just no speedup. No failure
  path introduced.
