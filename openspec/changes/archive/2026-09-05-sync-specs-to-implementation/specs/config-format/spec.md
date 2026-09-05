# config-format Specification

## MODIFIED Requirements

### Requirement: Full YAML preset

The Rust binary reads `config.{preset}.yaml`. The preset includes the sections: `database` (pragma), `embeddings` (mode local|api), `ingestion` (chunking.markdown/json, ner.llm, batch_size, resolver, max_retries), `linker` (disabled, llm), `search` (rrf_k, top-k, boosts, authority_boost), `graph`, `auto_update` (enabled, debounce_seconds, watch_sources, initial_sync, retry_failed), `scheduler.jobs` (named jobs with enabled/interval_seconds), `logging` (level/format/output), `paths` (workspace_dir, migrations_dir, prompts_path, onnx_config), `server` (name/version/host/port). The knowledge-DB path is NOT a config field — it is derived from `paths.workspace_dir` + `dataset.name` as `<workspace_dir>/datasets/<name>/state/knowledge.db`. Unknown keys do not break startup. Unknown values of string fields that are not validated at parse time (`logging.level/format/output`, `chunking.strategy`, `response_format`, `archive_format`, `source.type`, `attribute.type`) do not break startup.

New fields (additive, with defaults — backward compatible with presets that lack them):
- `ingestion.max_retries` (integer, default 3) — the maximum number of automatic re-indexing attempts for a problematic document by the background worker; after exhaustion the document gets `error` status in the `document_jobs` queue.
- `auto_update.retry_failed` (object, default `{ enabled: true, poll_interval_seconds: 60 }`) — enables the background re-processing of problematic documents and sets the polling interval for the `document_jobs` queue in seconds.

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

### Requirement: onnx.yaml model registry

The `onnx.yaml` format is preserved: the `runtime` section (version, platforms[] — key/os/arch/archive_url/archive_format/library_name/library_path) and `models` (default, entries[] — name/display_name/description/version/vector_dim/files[name,url,size_bytes,checksum?]). Model-loading behavior is fixed by this contract: download by url, **size check enforced** (`size_bytes` is verified against the downloaded file, design D8), storage in data/. The `checksum` field (`"sha256:hex"`) is **optional and NOT verified** (no shipped entry sets one): downloads are verified by size only, not by "URL + size + SHA-256".

#### Scenario: Model registry
- **WHEN** the Rust binary reads `workspace/configs/onnx.yaml`
- **THEN** the model list and runtime parameters are fully recognized; the model list prints all entries (machine-diff)

#### Scenario: Size enforced, checksum optional
- **WHEN** a downloaded file's size does not match its `size_bytes`
- **THEN** the download is rejected (size is the enforced check); a `checksum` field, when present, is parsed but not used to verify the file
