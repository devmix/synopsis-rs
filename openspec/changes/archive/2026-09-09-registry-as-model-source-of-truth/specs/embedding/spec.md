## ADDED Requirements

### Requirement: Provider model resolution

The embedding provider SHALL be built exclusively from the `onnx.yaml`
registry entry for the selected model name: the vector dimension from the
entry's `vector_dim`, the model file and the tokenizer (`tokenizer.json`) from
the entry's `files[]` list. No alternative resolution path (explicit file
paths in the main config) SHALL exist. A registry entry with a non-positive
`vector_dim` SHALL be rejected with a configuration error naming the model.

#### Scenario: Resolution from the registry entry
- **WHEN** the provider is built for a model present in the registry
- **THEN** the model file, the tokenizer, and the vector dimension all come from that registry entry, and the provider reports the entry's dimension

#### Scenario: Tokenizer missing from the entry
- **WHEN** the selected registry entry has no `tokenizer.json` in its `files[]` list (or the file is absent from the model directory)
- **THEN** provider construction fails with an explicit error naming the model and the missing file

#### Scenario: Non-positive dimension
- **WHEN** the selected registry entry declares `vector_dim <= 0`
- **THEN** provider construction fails with a configuration error naming the model
