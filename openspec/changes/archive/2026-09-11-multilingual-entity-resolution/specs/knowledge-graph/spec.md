# knowledge-graph Specification

## MODIFIED Requirements

### Requirement: Cross-domain linking pipeline

The pipeline SHALL apply the methods in configuration order: `equals` (exact name match), `expression` (CEL rules), `llm` (LLM comparison of the pair through an OpenAI-compatible client). Each method is idempotent: re-linking does not create duplicates (ON CONFLICT DO NOTHING via EntityLinkDao). The `linker.disabled` flag disables the LLM method. The LLM method, for each pair: loads the chunk context of both entities (up to 3 texts per entity), renders the system/user templates (loaded from `prompts_path`, with embedded fallback), calls the LLM client with structured output, parses the decision `{same_entity, confidence, reasoning}`, and creates a link only when confidence ≥ `llm_confidence_threshold` (method='llm', evidence=reasoning, confidence clamped to [0,1]); the decision is cached in the `llm_linker_cache` table (cache DB, `migrations/cache/1-init/up.sql`) by hash (entity pair + template hashes) — a repeat call for the same pair does not hit the LLM. A call/parse failure of one pair does not abort the pipeline (it is recorded in the results).

The pipeline supports two modes: **full rebuild** (all cross-domain candidate pairs) and **incremental** (a candidate entity set is linked against the remaining entities — only pairs with at least one member in the candidate set are considered). Both modes share the same methods, idempotency, and decision cache. The incremental mode is the one driven by the worker's `entity:link` queue events (see the pipeline spec, `Post-processing: linking`).

In addition, the pipeline SHALL generate **within-domain cross-script candidate pairs**: entities of the same domain and type whose dominant scripts differ (e.g. a Latin-script name and a Cyrillic-script name) and whose names are not already resolved by the resolution tiers (distinct article-stripped exact-match key and distinct stemmed key). These pairs are judged by the `llm` method only (equals/expression do not apply within a domain). For each such pair the decision is acted on as: confidence ≥ `merge_confidence_threshold` (ontology key, default 0.95) → the two entities are **merged** through the transactional merge operation (see the data-schema spec): the canonical is the entity with the greater number of source documents, on a tie the longer name, on a further tie the lower id; both names are recorded as aliases of the canonical. Otherwise, confidence ≥ `llm_confidence_threshold` → a `same_entity` link is created (method='llm', evidence=reasoning). Below `llm_confidence_threshold` → no action. Decisions for these pairs use the same decision cache as cross-domain pairs.

#### Scenario: equals linking
- **WHEN** two entities of different domains have identical normalized names
- **THEN** a cross-domain link is created; a repeat run does not duplicate it

#### Scenario: expression linking
- **WHEN** an ontology CEL rule is true for a pair of entities
- **THEN** a link is created with the rule's type/priority

#### Scenario: LLM linking above the threshold
- **WHEN** the LLM validly answers same_entity=true and confidence ≥ the threshold
- **THEN** a link with method='llm' and evidence=reasoning is created; the decision is written to the cache

#### Scenario: LLM below the threshold
- **WHEN** the decision's confidence is below `llm_confidence_threshold`
- **THEN** no link is created; the decision is cached

#### Scenario: Decision cache
- **WHEN** a pair already has a cached decision (same template hashes)
- **THEN** the LLM is not called; the cached decision is used (read from the `llm_linker_cache` table in the cache DB)

#### Scenario: Single-pair failure
- **WHEN** the LLM call or answer parsing for a pair ends in an error
- **THEN** the error is recorded in the results; the pipeline continues with the remaining pairs

#### Scenario: LLM stub
- **WHEN** the pipeline reaches the llm method
- **THEN** real LLM linking of the pair is performed (the stub from the graph change is replaced: context → templates → client call → decision); existing links are untouched

#### Scenario: Linker disabled
- **WHEN** `linker.disabled=true`
- **THEN** the llm method is excluded from the pipeline; equals/expression still work

#### Scenario: Incremental linking
- **WHEN** the pipeline runs in incremental mode with a candidate entity set
- **THEN** only pairs with at least one member in the candidate set are considered, links are created where a method matches, and no existing link is duplicated

#### Scenario: Within-domain cross-script merge
- **WHEN** two entities of the same domain and type are written in different scripts and the LLM answers same_entity=true with confidence ≥ `merge_confidence_threshold`
- **THEN** the entities are merged: the one with more source documents survives (tie: longer name, then lower id), both names are recorded as aliases of the survivor, and dependent rows (facts, chunk links, sources, links) are re-pointed

#### Scenario: Within-domain cross-script link below the merge threshold
- **WHEN** two entities of the same domain and type are written in different scripts and the LLM answers same_entity=true with confidence between `llm_confidence_threshold` and `merge_confidence_threshold`
- **THEN** a `same_entity` link is created and no merge happens

#### Scenario: Same-script pairs are not cross-script candidates
- **WHEN** two entities of the same domain and type are both written in the same dominant script
- **THEN** they are not generated as within-domain cross-script candidates (the resolution tiers and the alias map handle same-script variants)
