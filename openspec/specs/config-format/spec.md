# config-format Specification

## Purpose

The configuration file formats of Synopsis: YAML presets (`config.{preset}.yaml`), the `onnx.yaml` model registry, and XML ontologies in `data/ontology/`. Fixes the formats: the user must not change configs when switching to the Rust binary.
## Requirements
### Requirement: Full YAML preset

The Rust binary reads `config.{preset}.yaml`. The preset includes the sections: `database` (pragma), `embeddings` (mode local|api), `ingestion` (chunking.markdown/json, ner.llm, batch_size, resolver, max_retries), `linker` (disabled, llm), `search` (rrf_k, top-k, boosts, authority_boost), `graph`, `auto_update` (enabled, debounce_seconds, watch_sources, initial_sync, retry_failed), `scheduler.jobs` (named jobs with enabled/interval_seconds), `logging` (level/format/output), `paths` (workspace_dir, migrations_dir, prompts_path, onnx_config), `server` (name/version/host/port). The knowledge-DB path is NOT a config field — it is derived from `paths.workspace_dir` + `dataset.name` as `<workspace_dir>/datasets/<name>/state/knowledge.db`. Unknown keys do not break startup. Unknown values of string fields that are not validated at parse time (`logging.level/format/output`, `chunking.strategy`, `response_format`, `archive_format`, `source.type`, `attribute.type`) do not break startup.

New fields (additive, with defaults — backward compatible with presets that lack them):
- `ingestion.max_retries` (integer, default 3) — the maximum number of automatic re-indexing attempts for a problematic document by the background worker; after exhaustion the document gets `error` status in the `queue_tasks` queue.
- `auto_update.retry_failed` (object, default `{ enabled: true, poll_interval_seconds: 60 }`) — enables the background re-processing of problematic documents and sets the polling interval for the `queue_tasks` queue in seconds.

#### Scenario: Existing preset
- **WHEN** the Rust binary starts with an unmodified `workspace/configs/config.default.yaml`
- **THEN** the config is parsed successfully, default values are applied per the preset (machine-diff of the effective config)

#### Scenario: Unknown value of a string field
- **WHEN** `logging.level` contains an unfamiliar value (e.g. "verbose")
- **THEN** startup does not fail; the value is kept as-is

#### Scenario: Missing auto_update section
- **WHEN** the YAML has no auto_update section
- **THEN** the defaults enabled=true, initial_sync=true are applied

#### Scenario: Explicit auto_update section
- **WHEN** the YAML has auto_update with enabled=false
- **THEN** enabled=false is honored (not overwritten by the default)

#### Scenario: Boolean defaults with presence semantics
- **WHEN** `graph.load_on_startup` or `auto_update.watch_sources` is explicitly set to false
- **THEN** the value false is honored (**BREAKING**: previously an explicit false was forcibly flipped to true — the setting was non-functional)
- **WHEN** these keys are absent from the YAML
- **THEN** the default true is applied

#### Scenario: enable_graph per documented intent
- **WHEN** `graph.enable_graph` is absent from the YAML
- **THEN** true is applied (**BREAKING**: previously an absent key yielded false despite the doc comment "default true")

#### Scenario: Missing retry fields
- **WHEN** the YAML has neither `ingestion.max_retries` nor `auto_update.retry_failed`
- **THEN** the defaults max_retries=3, retry_failed.enabled=true, retry_failed.poll_interval_seconds=60 are applied (backward compatibility)

#### Scenario: Explicit retry configuration
- **WHEN** the YAML sets `ingestion.max_retries: 5` and `auto_update.retry_failed.poll_interval_seconds: 120`
- **THEN** the values are honored (the background worker retries up to 5 times, polling the queue every 120 s)

#### Scenario: Knowledge-DB path derived, not a config field
- **WHEN** a preset sets `paths.workspace_dir` and `dataset.name`
- **THEN** the knowledge-DB path resolves to `<workspace_dir>/datasets/<name>/state/knowledge.db` (derived from `workspace_dir` + `dataset.name`), and the cache DB resolves to `<workspace_dir>/db/cache/cache.db`

### Requirement: Field vectors.engine

The `vectors:` section (additive config-format extension, decision 2026-08-21) contains the optional `engine` field. The field SHALL accept only: absent (default) or `"usearch"`. The value `"lance"` SHALL be rejected with an explicit validation error stating that the lance engine was removed and that usearch is the only engine (`post-migration-lance-removal`, user decision 2026-08-31). Any other value SHALL be rejected with an explicit parse/validation error. The field does not affect other config sections and does not change the preset format.

#### Scenario: Missing field
- **WHEN** the preset contains a `vectors` section without the `engine` field
- **THEN** the usearch engine is used (the only engine)

#### Scenario: Explicit value
- **WHEN** the preset sets `vectors.engine: "usearch"`
- **THEN** `vectors` instantiates `UsearchEngine`

#### Scenario: Removed engine
- **WHEN** the preset sets `vectors.engine: "lance"`
- **THEN** the configuration is rejected with an explicit error naming the removal and pointing to usearch

#### Scenario: Invalid value
- **WHEN** the preset sets `vectors.engine: "foo"`
- **THEN** the configuration is rejected with an explicit error

### Requirement: Section vectors.usearch

The `vectors:` section is extended with an optional `usearch` object holding the two-layer persistence parameters of UsearchEngine (ADR 0004): `max_segment_vectors` (usize, default 1000000), `compaction_stale_threshold` (u8, 1..=100, default 30), `search_threads` (usize, default 4). The field is optional: when absent, `UsearchConfig::default()` is applied. Invalid values (`max_segment_vectors: 0`, `compaction_stale_threshold` outside 1..=100, `search_threads: 0`) are rejected at the parsing level with an explicit error.

#### Scenario: Missing usearch section
- **WHEN** the preset contains a `vectors` section without the `usearch` field
- **THEN** the engine receives `UsearchConfig::default()` (1000000 / 30 / 4)

#### Scenario: Explicit usearch configuration
- **WHEN** the preset sets `vectors.usearch.max_segment_vectors: 500000`
- **THEN** the RAM layer is flushed upon reaching 500000 vectors

#### Scenario: Invalid value
- **WHEN** the preset sets `vectors.usearch.max_segment_vectors: 0`
- **THEN** the configuration is rejected with an explicit error "must be > 0"

#### Scenario: Partial configuration
- **WHEN** the preset sets `vectors.usearch.compaction_stale_threshold: 50` without the other fields
- **THEN** `compaction_stale_threshold=50`, the rest are defaults (1000000, 4)

### Requirement: onnx.yaml model registry

The `onnx.yaml` format is preserved: the `runtime` section (version, platforms[] — key/os/arch/archive_url/archive_format/library_name/library_path) and `models` (default, entries[] — name/display_name/description/version/vector_dim/files[name,url,size_bytes,checksum?]). Model-loading behavior is fixed by this contract: download by url, **size check enforced** (`size_bytes` is verified against the downloaded file, design D8), storage in data/. The `checksum` field (`"sha256:hex"`) is **optional and NOT verified** (no shipped entry sets one): downloads are verified by size only, not by "URL + size + SHA-256".

#### Scenario: Model registry
- **WHEN** the Rust binary reads `workspace/configs/onnx.yaml`
- **THEN** the model list and runtime parameters are fully recognized; the model list prints all entries (machine-diff)

#### Scenario: Size enforced, checksum optional
- **WHEN** a downloaded file's size does not match its `size_bytes`
- **THEN** the download is rejected (size is the enforced check); a `checksum` field, when present, is parsed but not used to verify the file

### Requirement: onnx.yaml load-error handling

Loading the model registry from an external `onnx.yaml` fails with the file path if the file is missing or does not parse.

#### Scenario: Missing onnx.yaml
- **WHEN** the onnx.yaml file does not exist
- **THEN** loading fails with the file path

### Requirement: XML ontologies

Ingestion sources are declared in `data/ontology/global.xml` + `domains/*.xml` (not in YAML). Rules: a missing or malformed domain XML — startup error; a missing global.xml — not an error (empty pool). The effective domain schema is built from the domain definitions and the global pool (`<entity>`, `<relation>`, `<extraction>` in global.xml) as two layers: lookup searches the domain first, then the global layer (shadowing). A domain definition with an ID matching a global one overrides the global one for this domain, without a warning (**BREAKING**: previously — merge with a warning). Reference validation (relation → entity, ref attributes) is performed on the merged layers.

**global.xml format (BREAKING, human decision 2026-08-19):** every group of repeating elements is wrapped in a plural wrapper: `<entities><entity id= name= description=>` (with `<attributes><attribute name= type= required= target=>` and `<synonyms><synonym>`), `<relations><relation source= predicate= target= description=>` (with `<attributes>`), `<sources><source path= type= disabled= space= dataset=>` (with `<domains><domain>`), `<cross-domain-links>` (`<methods><method>`, `<equals><min-words>`, `<llm-confidence-threshold>`, `<batch-size>`, `<expressions><expression>` with `<name>/<description>/<priority>/<where>/<relation-type>`), `<ner>` (`<methods><method>`), `<extraction>` (`<regex-rules><regex id= entity= pattern= confidence=>`). The new format is a deliberate breaking change; compatibility is by semantics.

#### Scenario: Malformed ontology
- **WHEN** domains/ contains syntactically invalid XML and the binary starts
- **THEN** startup fails with a message about the ontology file

#### Scenario: Global pool override
- **WHEN** an entity in a domain file has an ID from the global pool
- **THEN** the domain version is used for this domain; the global version remains available for the other domains; no warning is required (BREAKING: previously — merge with a warning)

#### Scenario: Missing global.xml
- **WHEN** data/ontology/ does not contain global.xml
- **THEN** startup does not fail; the global pool is empty

#### Scenario: Validation on merged layers
- **WHEN** a global relation references an entity overridden by a domain
- **THEN** the reference resolves to the domain version (layer desynchronization is excluded)

### Requirement: Domain-XML ontology

Each `domains/*.xml` file describes a domain: `<domain name= version= description=>` with `<entities><entity id= name= description=>` (attributes `<attributes><attribute name= type= required= target=>`, synonyms `<synonyms><synonym>`), `<relations><relation source= predicate= target=>` (attributes), `<extraction><regex-rules><regex id= entity= pattern= confidence=>`, `<confidence auto_publish_threshold= review_threshold= reject_threshold=>`. The domain-XML format — the same wrapper schema as global.xml (BREAKING). Rules: a missing domain file — startup error; invalid XML — startup error; an invalid regex pattern — startup error (compiled at load time).

#### Scenario: Invalid regex
- **WHEN** a domain XML contains a regex with a non-compilable pattern
- **THEN** startup fails, naming the file and the rule

#### Scenario: Complete domain
- **WHEN** the Rust binary reads `workspace/datasets/edtech/ontology/domains/domain_hr.xml`
- **THEN** entity/relation/extraction/confidence are fully recognized (machine-diff)

### Requirement: Embeddings model selection

The `embeddings.local` section SHALL contain only the optional `model_name` field. The model's vector dimension and file locations (model file, tokenizer) SHALL be resolved from the `onnx.yaml` registry entry for the selected model — the registry is the single source of truth for model metadata. `vector_dim`, `model_path`, and `tokenizer_path` SHALL NOT be configuration fields. An empty `model_name` SHALL select the `models.default` model from `onnx.yaml`. The `embeddings.api` section SHALL NOT contain a `vector_dim` field. A configuration file that still sets a removed key SHALL start successfully (unknown keys do not break startup), and the stale value SHALL be ignored without warning.

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
