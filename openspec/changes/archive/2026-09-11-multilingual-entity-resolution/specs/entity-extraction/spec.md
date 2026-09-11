# entity-extraction Specification

## MODIFIED Requirements

### Requirement: Entity resolution

The resolver deduplicates entities by a four-tier match evaluated in order
(the first hit wins): (1) **article-stripped exact match** — the normalized
name with a leading definite/indefinite article removed (`The X` ≡ `X`,
`A X` / `An X` ≡ `X`); (2) **stem equality** — within the same (domain, type),
the per-word stemmed form of the article-stripped normalized name is equal
(Latin script → English Porter stem, Cyrillic script → Russian Snowball stem,
any other script → identity); (3) **dataset alias** — a name listed in the
dataset alias map (see the config-format spec) resolves to its canonical
name, which is then looked up by the tier-1 key; (4) **name similarity** —
rune-aware Jaro-Winkler ≥ the configured threshold, with bigram blocking by
key `domain:type:bigram`. Tiers 1–3 resolve deterministically (no threshold);
tier 4 requires the threshold. Different domains or types are never merged by
any tier. The in-batch union-find clustering applies the same tier order.
The canonical is the longest name. Every name that resolved to an existing
entity (any tier) is persisted as an alias of the canonical in the entity
alias table, so a repeated surface form resolves by lookup without
similarity. The index is hydrated lazily once from the DB and updated
incrementally. Operations: `lookup` (search only),
`lookup_or_create(_with_stats)` (search or create + link to the document
through entity_sources), `add_entities` (batch clustering + linking). Entity
metadata on creation is cleared of the document fields (url, image_paths,
page_links, categories).

#### Scenario: Article-prefix variants
- **WHEN** the names `The X` and `X` (or `A X` / `An X`) appear for the same type and domain
- **THEN** they resolve to a single entity with the longest name, regardless of the similarity threshold

#### Scenario: Inflected case or number variants
- **WHEN** two names of the same type and domain differ only by case/number inflection of a short word (so their Jaro-Winkler score is below the threshold but their stemmed forms are equal)
- **THEN** they resolve to a single entity with the longest name

#### Scenario: Stem equality is script-bounded
- **WHEN** two names of the same type and domain are written in different scripts (e.g. Latin vs Cyrillic)
- **THEN** the stem tier does not merge them (their stemmed forms differ); only the alias tier or the similarity tier may resolve them

#### Scenario: Alias map resolution
- **WHEN** a name is present in the dataset alias map and its canonical name already exists as an entity of the same type and domain
- **THEN** the name resolves to that entity without any similarity computation

#### Scenario: Alias memory persists across resolutions
- **WHEN** a surface form resolved to an entity by any tier in a previous run and is extracted again
- **THEN** it resolves to the same entity via the stored alias (the alias table), not by re-running similarity

#### Scenario: Merging similar names
- **WHEN** the names of two entities of the same type and domain have Jaro-Winkler ≥ the threshold and no earlier tier matched
- **THEN** they resolve to a single canonical entity with the longest name

#### Scenario: Domain isolation
- **WHEN** identical names appear in different domains
- **THEN** they are different entities

## ADDED Requirements

### Requirement: NER prompt canonical-form directive

The NER system prompt SHALL include a canonical-form directive instructing
the model to report entity names in dictionary form: the nominative (bare,
uninflected) form for inflected languages, and the bare proper name without
a leading article for English. The directive is present in both the workspace
prompt override and the embedded default prompt (the two copies stay
byte-identical). Because the NER cache key includes the template hash,
changing the directive invalidates cached extractions for the affected
templates.

#### Scenario: Directive present in both copies
- **WHEN** the workspace prompt override and the embedded default NER system prompt are compared
- **THEN** both contain the canonical-form directive and the two files are byte-identical

#### Scenario: Cache invalidation on prompt change
- **WHEN** the NER system prompt template changes (the directive is added) and a previously cached chunk is re-extracted
- **THEN** the cache key (template hash) differs and the chunk is re-extracted through the LLM rather than served from cache
