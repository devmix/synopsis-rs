# Proposal: multilingual-entity-resolution

## Why

The entity resolver deduplicates only by Jaro-Winkler name similarity (threshold
0.85, bigram blocking). Surface-form variants of one real-world entity are not
merged: article-prefix variants (e.g. `The X` / `X`) score 0.76–0.83, inflected
case/number variants of a short name score below the threshold, and
cross-lingual names of the same entity (e.g. an English and a Russian name)
score ~0.37. Measured on the game-wiki dataset: 6 duplicate pairs of the same
type and domain, each backed by real source documents. The resolver also has no
alias memory — every re-ingest re-splits the same variants, and the NER prompt
does not ask the model for a canonical (dictionary-form) name, so inflected
forms keep appearing.

## What Changes

- **Resolution tiers.** `find_best_candidate` and the in-batch `cluster_batch`
  gain three tiers before the existing Jaro-Winkler tier: (1) article-stripped
  exact match (`The X` ≡ `X`), (2) Snowball-stem equality per
  (domain, type) — handles case/number inflection (English Porter, Russian
  Snowball; CJK and unknown scripts fall back to identity), (3) dataset alias
  map (explicit alias → canonical). Canonical promotion stays "longest name";
  every merged name is recorded in the new alias table (memory across runs).
- **Data schema.** New `entity_aliases` table (`entity_id`, `alias`,
  unique) plus a transactional `merge_entities(into, from)` operation that
  rewires `facts`, `chunk_entities`, `entity_sources`, `entity_links` and
  records both names as aliases before deleting the duplicate. Shipped as a
  new numbered migration (`1-init` is never edited).
- **Config.** Optional `<aliases>` block in the dataset ontology
  (`global.xml` / `domain_*.xml`): alias → canonical map, parsed and
  validated by the config crate (revised 2026-09-10 — originally a separate
  `ontology/aliases.yaml`); new ontology key `merge_confidence_threshold`
  (default 0.95) in the `<cross-domain-links>` block.
- **NER prompt.** Canonical-form directive: entity names in the dictionary
  (nominative) form. Both copies (embedded default + workspace override) stay
  byte-identical. This changes the NER cache key (template hash) → the cache
  invalidates itself.
- **CLI.** New `db merge-entities <id> --into <id>` action (same
  type+domain check, confirmation, summary) on top of the transactional merge.
- **Linker.** Within-domain cross-script candidate generation (same type,
  different dominant script, distinct match/stem keys) judged by the existing
  LLM method: confidence ≥ `merge_confidence_threshold` → merge (canonical
  chosen by source-document count, then longest name, then lowest id);
  confidence ≥ `llm_confidence_threshold` → `same_entity` link; below → skip.
  Decisions use the existing decision cache.
- **MCP read path.** Additive `aliases` field on the entity payload in
  `get_entity_dossier`, the entity catalog, and search results. Recorded MCP
  fixtures are re-recorded for the additive field.
- **Utils.** New stemming module in `crates/utils` (script detection +
  per-script Snowball stemming) — shared by ingestion and graph. New workspace
  dependency `rust-stemmers 1.2.0` (verified: Snowball algorithms incl.
  Russian, MIT/BSD-3-Clause, pure Rust, no I/O/unsafe).
- **Verification.** Re-ingest of the affected dataset (cache self-invalidates
  after the prompt change) and confirmation that the previously duplicated
  pairs resolve to single entities.

**Frozen contracts touched:** data schema (`entity_aliases`), config format
(ontology `<aliases>` block, `merge_confidence_threshold`), CLI surface
(`db merge-entities`), MCP contract (additive `aliases`). Parity: differential
similarity/resolver fixtures, re-recorded MCP fixtures, full
fmt/clippy/test gates.

## Capabilities

### New Capabilities

(none — all behavior changes map to existing capabilities)

### Modified Capabilities

- `entity-extraction`: resolution tiers (article/stem/alias before JW); new
  NER prompt canonical-form directive requirement
- `knowledge-graph`: cross-domain linking pipeline extended with within-domain
  cross-script candidates and the merge action
- `data-schema`: `entity_aliases` table + transactional merge semantics
- `config-format`: ontology `<aliases>` block (global + domain files);
  `merge_confidence_threshold` ontology key
- `cli-surface`: `db` subcommand gains `merge-entities`
- `mcp-contract`: additive `aliases` field in tool responses
- `utils`: shared utility crate gains the stemming module

## Non-goals

- No lemmatization beyond Snowball stemming (no morphological dictionaries, no
  language models for morphology).
- No cross-domain behavior change: the existing cross-domain pipeline is
  untouched; the new candidate generation is within-domain only.
- No type correction: same name under different entity types stays separate
  (a classification issue, not deduplication).
- No migration or upgrade of existing knowledge DBs (fresh-build only, design
  D6).
- No ANN/vector, chunking, or search-ranking changes.
- No site/UI changes beyond documentation updates.

## Impact

- **Crates:** `utils` (new module + dependency), `db` (migration, DAO, merge),
  `ingestion` (similarity, resolver, cluster, NER prompts), `config`
  (ontology `<aliases>` parsing, ontology key), `graph` (linker), `mcp`
  (dossier/catalog/search payloads), `cli` (`db` subcommand).
- **Dependencies:** `rust-stemmers 1.2.0` added to the workspace (frozen-stack
  extension, user-approved; verified online per project rules).
- **Runtime:** NER cache invalidation (template-hash key) → one re-ingest of
  the affected dataset; stem computation adds per-entity string work in the
  ingestion path (microsecond-scale, no model load).
- **Documentation:** data-schema / config-format / cli-surface / mcp-contract
  specs, `README.md`, and site docs updated in the same change.
