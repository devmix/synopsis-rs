## ADDED Requirements

### Requirement: Reasoning effort control

The LLM client SHALL send the configured `reasoning_effort` as a top-level
field in the chat-completions request body when it is set (non-empty), and
SHALL omit the field entirely when it is empty or unset. The value is passed
through verbatim (the client does not validate or reject effort levels). When
the field is omitted, the request body is byte-identical to the pre-change
shape (model, messages, temperature, max_tokens). The `reasoning_effort` is
sourced from the per-consumer LLM config (NER / linker) and is not a
per-call parameter.

#### Scenario: Effort set
- **WHEN** the LLM config sets `reasoning_effort: low`
- **THEN** the request body contains `"reasoning_effort": "low"` at the top level

#### Scenario: Effort empty or absent
- **WHEN** the LLM config leaves `reasoning_effort` empty or the key is absent
- **THEN** the request body contains no `reasoning_effort` field and is otherwise unchanged

#### Scenario: Effort value passed through
- **WHEN** the LLM config sets `reasoning_effort` to a value the client does not recognize
- **THEN** the client sends the value verbatim (no client-side validation or rejection)
