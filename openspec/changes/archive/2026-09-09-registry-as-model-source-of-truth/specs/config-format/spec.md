## ADDED Requirements

### Requirement: Embeddings model selection

The `embeddings.local` section SHALL contain only the optional `model_name`
field. The model's vector dimension and file locations (model file, tokenizer)
SHALL be resolved from the `onnx.yaml` registry entry for the selected model —
the registry is the single source of truth for model metadata. `vector_dim`,
`model_path`, and `tokenizer_path` SHALL NOT be configuration fields. An empty
`model_name` SHALL select the `models.default` model from `onnx.yaml`. The
`embeddings.api` section SHALL NOT contain a `vector_dim` field. A
configuration file that still sets a removed key SHALL start successfully
(unknown keys do not break startup), and the stale value SHALL be ignored
without warning.

#### Scenario: Dimension from the registry
- **WHEN** the preset sets `embeddings.local.model_name: bge-small-en-v1.5` and the registry entry declares `vector_dim: 384`
- **THEN** the embedding provider and the vector index use dimension 384, with no dimension declared in the main config

#### Scenario: Empty model name
- **WHEN** `embeddings.local.model_name` is empty or absent
- **THEN** the `models.default` model from onnx.yaml is used

#### Scenario: Removed keys ignored
- **WHEN** an old preset still sets `embeddings.local.vector_dim` (or `model_path` / `tokenizer_path`)
- **THEN** startup succeeds and the values are silently ignored (no warning, no error)

#### Scenario: Non-positive registry dimension
- **WHEN** the selected registry entry has `vector_dim <= 0`
- **THEN** startup fails with an explicit configuration error naming the model
