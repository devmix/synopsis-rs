## ADDED Requirements

### Requirement: LLM reasoning effort config

The per-consumer LLM config (the `ner.llm` block, the `linker.llm` block, and
any `llm:` block) SHALL accept an optional `reasoning_effort` string key. It
is additive and backward compatible: a preset that omits it parses
successfully and the effective value is empty (the field is not sent to the
LLM). The key is not validated at parse time (an unrecognized effort value
does not break startup), consistent with the other non-validated string
fields. The shipped presets set `reasoning_effort: low` on both the NER and
the linker LLM.

#### Scenario: Key absent
- **WHEN** a preset's `ner.llm` block has no `reasoning_effort` key
- **THEN** the config parses successfully and the effective `reasoning_effort` is empty (not sent)

#### Scenario: Key set
- **WHEN** a preset sets `ner.llm.reasoning_effort: low`
- **THEN** the config parses successfully and the NER LLM consumer sends `reasoning_effort: low`

#### Scenario: Unrecognized effort value
- **WHEN** a preset sets `reasoning_effort` to a value not recognized by the client (e.g. `extreme`)
- **THEN** startup does not fail; the value is passed through to the request as-is
