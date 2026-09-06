# entity-extraction Specification

## Purpose

Knowledge extraction from chunks: NER providers (regex per domain-config rules, LLM with structured output and a cache), composite orchestration of stages with threshold filtering, and entity resolution (deduplication by name similarity) with persistence through entity_sources.

## Requirements

### Requirement: NER providers

The ingestion crate provides the trait `NerProvider` (`name()` + `extract_entities()`), implemented by the `RegexNer` and `LlmNer` providers; "nothing found" is `Ok(None)`, and a provider error is fatal for the call. RegexNer is built from the regex rules of the domain configs, prefers the first capture group, deduplicates by (name, type, domain), and sets `rule_id`. LlmNer requires at least one domain config and processes domains in configuration order.

#### Scenario: Regex extraction with a capture group
- **WHEN** a rule has a capture group and the text matches
- **THEN** the entity name becomes the group's content; without groups — the full match; duplicates by (name, type, domain) are removed

#### Scenario: LLM extraction across multiple domains
- **WHEN** several domains are configured
- **THEN** each domain is processed in a separate call; the results are tagged with the domain name

### Requirement: LLM response validation

The LLM response is parsed as JSON `{entities, relations}`: empty names are skipped; a confidence outside [0,1] is replaced with the default 0.5; description is truncated to ≤500 characters at a sentence boundary; string metadata values with uncertainty markers ("implied by context", "not explicitly stated") are dropped; the `version` field is checked for the version format; facts with any empty required field are dropped.

#### Scenario: Invalid confidence
- **WHEN** the LLM returned a confidence of 1.7 or a negative one
- **THEN** the default 0.5 is used

#### Scenario: description truncation
- **WHEN** a description is longer than 500 characters
- **THEN** it is truncated at the last sentence boundary within the limit, or hard-truncated at the limit if there is none

### Requirement: Document context in the NER prompt

The LLM prompt MUST explicitly include the context of the chunk's position in the document (the section path / breadcrumbs from the chunk metadata) when it is present; the chunk is passed as a pure slice without prefixes.

#### Scenario: Chunk with breadcrumbs
- **WHEN** the chunk metadata contains a section path
- **THEN** the user prompt contains an explicit document-context block before the chunk text

#### Scenario: Chunk without context
- **WHEN** the metadata contains no section path
- **THEN** the context block is not rendered

### Requirement: LLM-NER cache

LLM responses are cached in the `llm_ner_cache` table (created lazily on first use, outside the migration schema); the key is the SHA-256 of the call parameters (server, model, temperature, max_tokens, system/user prompts, content). A corrupted entry is treated as a miss; a disabled cache is a no-op.

#### Scenario: Repeat chunk
- **WHEN** the same content with the same parameters is extracted again
- **THEN** no HTTP call is made and the result is taken from the cache

### Requirement: Composite NER

The composite runs the stages from the configuration (`regex`, `llm`) in declaration order; enriches the metadata of each entity/fact with the source metadata and the provider name; filters by the domain's `auto_publish_threshold` — entities below the threshold are dropped, and facts with a dropped subject or object are dropped cascadingly; entities of unknown domains pass through unfiltered.

#### Scenario: Cascading fact filtering
- **WHEN** an entity below the auto_publish threshold participates in a fact
- **THEN** the fact is removed together with the entity

#### Scenario: Unknown stage
- **WHEN** the configuration contains a stage outside {regex, llm}
- **THEN** building the composite fails with the list of allowed values

### Requirement: Entity resolution

The resolver deduplicates entities by name similarity: name normalization, rune-aware Jaro-Winkler, bigram blocking with keys `domain:type:bigram` (different domains/types are never merged), union-find clustering of the batch, the canonical is the longest name. The index is hydrated lazily once from the DB and updated incrementally. Operations: `lookup` (search only), `lookup_or_create(_with_stats)` (search or create + link to the document through entity_sources), `add_entities` (batch clustering + linking). Entity metadata on creation is cleared of the document fields (url, image_paths, page_links, categories).

#### Scenario: Merging similar names
- **WHEN** the names of two entities of the same type and domain have Jaro-Winkler ≥ the threshold
- **THEN** they resolve to a single canonical entity with the longest name

#### Scenario: Domain isolation
- **WHEN** identical names appear in different domains
- **THEN** they are different entities

### Requirement: Extraction parity

NER extraction and resolution are verified against recorded fixtures: regex rules, response validation, threshold filtering, and name similarity (including Cyrillic) yield the same results as fixed in the recorded cases.

#### Scenario: Recorded-case run
- **WHEN** the cases (regex, parse, composite, similarity, resolver) are run through the Rust implementation
- **THEN** the results match the recorded fixtures

### Requirement: Effective domain schema (two-layer)

The LLM NER prompt and JSON schema render from the effective domain schema:
the domain's own definitions plus the global ontology pool — entities
shadowed by id, relations by predicate, extraction rules by rule id — with
the domain winning silently. The regex NER stage receives the merged
extraction rules.

#### Scenario: Pool types appear in the prompt
- **WHEN** a domain has no `employee` entity and the global pool defines one
- **THEN** the rendered system prompt and JSON schema include `employee` as an extractable type for that domain

#### Scenario: Shadowing
- **WHEN** the domain and the pool define an entity with the same id
- **THEN** only the domain's definition is used, without a warning
