# Design: multilingual-entity-resolution

## Context

See proposal.md for motivation. Current state that shapes the approach:

- **Resolution** lives in `crates/ingestion/src/entities/`: `similarity.rs`
  (`normalize_name` → `ner::normalize` = trim + lowercase + whitespace
  collapse; rune-aware `bigrams`; `jaro_winkler`), `resolver.rs`
  (`BlockingIndex`: name map + bigram blocks keyed `domain:type:bigram`,
  `find_best_candidate` = exact name hit → best JW among block candidates ≥
  threshold), `cluster.rs` (`cluster_batch`, same JW + threshold, union-find).
- **Linking** lives in `crates/graph/src/linker.rs`: methods equals/expression/
  llm, candidate generation is **cross-domain only** (`cross_domain_pairs`:
  equal normalized name, different domain); LLM method with templates,
  decision cache (`llm_linker_cache` in the cache DB), and
  `llm_confidence_threshold` (default 0.7) from the ontology
  (`crates/config/src/ontology.rs`, `<cross-domain-links>` block).
- **NER prompts**: embedded defaults in `crates/ingestion/src/ner/templates/`
  and workspace overrides in `workspace/configs/prompts/ner/` — the two copies
  are byte-identical by contract; the NER cache key includes the template
  SHA-256, so a prompt change self-invalidates the cache.
- **Migrations**: `migrations/knowledge/1-init/up.sql` is shipped and never
  edited; future migrations are numbered directories
  `<id>-<slug>/up.sql`, forward-only, `PRAGMA user_version` is the sole
  schema-state authority.
- **CLI**: `crates/cli/src/db.rs` implements `db stats|clear` (one-shot,
  opens only the dataset-bound DB, confirmation on stdin).
- **MCP**: entity payloads in `crates/mcp/src/tools/dossier.rs`
  (`DossierEntity`), `entities_catalog.rs`, `search.rs`; field order is a
  frozen contract.
- **Dependency graph (D1)**: `config, db, vectors, utils, llm → embedding,
  ingestion, graph → search → mcp → cli`. `ingestion` and `graph` are
  siblings — anything shared by both goes into `crates/utils`.
- **NDA constraint (user decision, 2026-09-10):** subject-matter entity names
  (the game content) MUST NOT appear in project code or in the OpenSpec
  artifacts. Tests use generic words; dataset-specific alias pairs are
  discovered by querying the DB at implementation time, never hardcoded in
  specs or tests.

## Goals / Non-Goals

**Goals:** same real-world entity → one row in `entities`, across article
variants, case/number inflection, and cross-lingual names; alias memory so
repeated surface forms resolve by lookup; a manual repair command; bounded LLM
judgment for cross-lingual pairs; additive visibility of aliases in MCP
responses.

**Non-Goals:** lemmatization beyond Snowball; cross-domain pipeline changes;
type correction; existing-DB migration (fresh build only); ANN/search-ranking
changes; site/UI beyond docs.

## Decisions

### D1 — Four-tier match, in order: exact (article-stripped) → stem → alias → JW

`find_best_candidate` and `cluster_batch` evaluate:

1. **Article-stripped exact**: key = `normalize(name)` with a leading
   `the ` / `a ` / `an ` (word-boundary, lowercase) removed. Hit → score 1.0.
   Fixes `The X` / `X` (measured JW 0.767–0.825, all below the 0.85
   threshold).
2. **Stem equality**: key = `(domain, type, stem_key)` where
   `stem_key = stem_name(strip_articles(normalize(name)))` — per-word Snowball
   stem. Hit → score 1.0. Fixes case/number variants JW misses (short words,
   e.g. a 5-letter noun where an o→∅ alternation drops JW to ~0.81).
3. **Dataset alias**: name ∈ the ontology `<aliases>` map → canonical name →
   tier-1 lookup of the canonical. Hit → score 1.0. Fixes cross-lingual pairs
   and abbreviations that no string metric can bridge.
4. **JW** (existing): bigram blocking, threshold 0.85.

Rationale: tiers 1–3 are deterministic and cheap; JW stays the fallback for
genuinely novel variants. Alternatives considered: (a) lowering the JW
threshold — rejected: the 0.80–0.849 band holds ~800 distinct-entity pairs on
the current dataset, a lower threshold mass-merges; (b) JW on stemmed names as
a score — rejected: a new scoring rule with no measured boundary; equality is
decidable.

**Stemmer placement:** `rust-stemmers = "1.2.0"` (verified: Snowball
algorithms incl. Russian + English Porter, MIT/BSD-3-Clause, pure Rust, no
I/O/unsafe; input must be lowercase — normalization already does that). New
module `crates/utils/src/text.rs` (leaf crate, shared by `ingestion` and
`graph` per D1): `detect_script` (first alphabetic rune: Latin/Cyrillic/other),
`stem_word` (Latin→English, Cyrillic→Russian, other→identity), `stem_name`
(per word, rejoined). A `Stemmer` instance is created once and reused
(`Stemmer::create` is cheap but the stemmers are stateless function pointers;
a small lazy static or per-call creation both pass the laptop budget — per
call, to avoid a global).

**Stem tier risk (accepted, documented):** stem equality auto-merges
homonym stems (the "policy/police" class). Mitigations in force: same
(domain, type) gate, article stripping, and the user's explicit opt-in.
Display is safe because the stored name is unchanged and canonical promotion
picks the longest name.

### D2 — `entity_aliases` table + transactional `merge_entities` in the db crate

New migration `migrations/knowledge/2-entity-aliases/up.sql` (1-init is
shipped/never edited):

```sql
CREATE TABLE entity_aliases (
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    alias     TEXT NOT NULL,
    UNIQUE (entity_id, alias)
);
CREATE UNIQUE INDEX idx_entity_aliases_alias ON entity_aliases(alias);
```

`db::merge_entities(into, from)` — one transaction: re-point `facts`
(subject and object; `INSERT … ON CONFLICT DO NOTHING` into a temp or
delete-then-reinsert to respect UNIQUE (subject, object, predicate)),
`chunk_entities` (PK collision → delete-then-insert), `entity_sources`
(INSERT OR IGNORE), `entity_links` (skip self-links after re-point, ON
CONFLICT DO NOTHING), record `from.name` + `into.name` as aliases (INSERT OR
IGNORE), `DELETE FROM entities WHERE id = from`. Preconditions checked before
the first write: both exist, same type+domain, `into != from` — violation →
Err, no partial state (a single transaction also makes a mid-way failure
atomic).

Why in `db` (not `ingestion` or `cli`): it is pure SQL re-wiring; both the
CLI command (T4) and the linker merge action (T5) call it. Alternatives:
(1) resolver-side merge (ingestion) — rejected: ingestion does not own
`facts`/`entity_links` re-wiring and the CLI cannot depend on ingestion's
resolver state; (2) each caller re-implements the SQL — rejected: DRY.

### D3 — Alias memory is written on every non-creating resolution

Whenever a surface form resolves to an existing entity by any tier (1–4),
the surface form (if different from the canonical's stored name) is recorded
in `entity_aliases` (INSERT OR IGNORE). The resolver's hydration also loads
existing aliases into a name→entity_id map, so repeated surface forms skip
all similarity work. This makes resolution monotone: once merged, a surface
form never re-splits across re-ingests (even if the canonical later gets a
longer name promotion).

### D4 — Alias map in the ontology XML (`global.xml` / `domain_*.xml`)

**Revised 2026-09-10 (user decision):** aliases live in the ontology files
themselves, not in a separate `ontology/aliases.yaml` (the YAML loader from
the original D4 is reverted by task 4.3).

Both `global.xml` and each `domain_*.xml` MAY carry an optional top-level
`<aliases>` block:

```xml
<aliases>
    <alias name="alias-surface" canonical="canonical-name"/>
</aliases>
```

Parsed by the config crate into `GlobalConfig` / `DomainConfig` (quick-xml,
same style as the other elements); validated at load: `name` and `canonical`
non-empty, duplicate `name` (one alias → exactly one canonical, across all
files of the dataset) → configuration error (fail fast at startup, consistent
with ontology validation). The effective dataset map is the flat union of the
global block and all domain blocks — the domain+type gate stays at tier-3
lookup, so the resolver consumes the same `HashMap<String, String>` as before.
The bootstrap derives the map from the already-parsed ontology configs (no
extra file I/O).

Deliberately NOT mixed with the per-entity `<synonyms>`: those are
type-level surface forms (the name of the entity *type*), while `<aliases>`
are instance-name aliases (one entity name → another). Canonical need not
pre-exist: first extraction of either side creates the entity, the other
resolves to it.

### D5 — Canonical-form NER directive, both copies in sync

Add one sentence to the NER system prompt (embedded default
`crates/ingestion/src/ner/templates/system.tmpl` + workspace override
`workspace/configs/prompts/ner/system.tmpl`, kept byte-identical): report
entity names in dictionary form — nominative for inflected languages, bare
proper name without a leading article for English. Effect: fewer inflected
surface forms at the source; the NER cache key (template SHA-256) changes →
cache self-invalidates → the T7 re-ingest re-extracts with the new prompt.
No code change needed for the invalidation.

### D6 — CLI `db merge-entities <id> --into <id>`

`crates/cli/src/db.rs`: third action. Resolve dataset DB path (existing
helpers), load both entities, check same type+domain (clear error on
violation), print stats block (existing presentation), confirm on stdin
(`Confirm merge? [y/N]`), on `y` call `db::merge_entities`, print a summary
(re-pointed row counts, surviving name, recorded aliases). One-shot, no model
load, DB closed before exit — mirrors `db clear`.

### D7 — Within-domain cross-script candidates + merge action in the linker

`crates/graph/src/linker.rs`: new candidate generator — entities of the same
(domain, type) whose **dominant scripts differ** (first alphabetic rune class,
`utils::text::detect_script`) and whose tier-1/tier-2 keys both differ (i.e.
not already resolved by resolution). Pairs go to the `llm` method only,
reusing the existing templates, structured output, decision cache, and
per-pair failure isolation. Decision actioning (new, per pair):

- confidence ≥ `merge_confidence_threshold` (new ontology key, default 0.95,
  validated like `llm_confidence_threshold`) → `db::merge_entities`:
  canonical = greater `entity_sources` count → tie: longer name → tie: lower
  id; both names become aliases.
- confidence ≥ `llm_confidence_threshold` (0.7) → `same_entity` link
  (method='llm', evidence=reasoning).
- below → no action (decision still cached).

Why merge at all (vs links only): the product decision is "merge as primary";
a 0.95+ LLM verdict on a cross-lingual pair is the same evidence class as an
explicit alias entry. Why cross-script only: same-script variants are
covered by tiers 1–3 (measured), so generating same-script candidates would
burn LLM calls on pairs the deterministic tiers already settle.

### D8 — Additive `aliases` in MCP payloads

`DossierEntity` (dossier.rs), the catalog entity entry (entities_catalog.rs),
and the search result entity brief (search.rs) gain `aliases: Vec<String>`
(own name excluded; empty array when none). Field appended after the last
existing field (additive, field-order contract preserved for existing fields).
Fixtures re-recorded for the three tools. Read path: one
`SELECT alias FROM entity_aliases WHERE entity_id = ?` per entity (covered by
the table's PK; batched per response).

### D9 — Verification via re-ingest, not hand-edited data

The NER prompt change (D5) invalidates the NER cache; `db clear` + `serve`
re-ingests the dataset from scratch (fresh DB + vectors, per D6). The
previously duplicated pairs must then resolve to single entities: verified by
a DB query counting same-type+domain entity pairs that share a tier-1 or
tier-2 key (expected: 0) plus the alias table containing the merged surface
forms. The T4 command remains as the surgical repair path for datasets where
re-ingest is too expensive.

## Risks / Trade-offs

- **Stem homonym auto-merge** ("policy/police" class) → same (domain, type)
  gate + user opt-in; documented in the data-schema spec; the alias table
  keeps both names so a mistaken merge is visible (and repairable via
  re-ingest with a corrected alias map).
- **rust-stemmers is old (2019)** → the Snowball algorithms are frozen
  upstream; the crate is 31M-downloads, pure Rust, no CVE surface (no I/O,
  no unsafe); API is two functions. Isolation: all usage behind
  `utils::text` — a swap touches one module.
- **LLM merge is irreversible at row level** (the duplicate row is deleted) →
  both names survive as aliases; `entity_sources` history survives on the
  canonical; a wrong 0.95+ verdict can be repaired by re-ingest or by
  splitting via a new manual command (out of scope, noted).
- **Re-ingest cost** (full NER + embedding pass on the dataset) → one-time,
  laptop-scale dataset; the NER cache for unchanged templates is reused
  across datasets (cache DB is global).
- **MCP fixture churn** (three tools re-recorded) → additive field only;
  parity machine re-runs the diff.
- **`<aliases>` drift** (a canonical renamed later) → the map is static
  data; a stale canonical simply creates a new entity — no corruption,
  detected in review.

## Migration Plan

1. Ship the code (tiers, table, CLI, linker action, MCP field) — all
   additive; a fresh DB builds with the new migration; an existing running
   server is unaffected until restart.
2. Rebuild the affected dataset: `synopsis db clear` (confirm) →
   `synopsis serve` (startup reconcile re-ingests). The NER prompt change
   forces re-extraction; tiers 1–3 merge the known variant classes; the T5
   LLM pass handles the remaining cross-lingual pairs.
3. Rollback: revert the code; the `entity_aliases` table is additive and
   harmless if left in the rebuilt DB; a dataset rebuilt with the old code
   simply re-splits the variants (status quo ante). No data is destroyed by
   the change itself.

## Open Questions

(none — the two micro-decisions [merge threshold 0.95; cross-script-only
T5 candidates] and the NDA constraint were resolved with the user on
2026-09-10. Revised the same day: aliases moved from `ontology/aliases.yaml`
into the ontology XML files — see D4.)
