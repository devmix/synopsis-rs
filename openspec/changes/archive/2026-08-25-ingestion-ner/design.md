# Design: ingestion-ner

Oracle references (read-only): `../synopsis/internal/ingestion/ner/*.go`,
`../synopsis/internal/ingestion/entities/*.go`,
`../synopsis/configs/prompts/ner/{system,user}.tmpl`.

## D1 — Modules live in `crates/ingestion`, not a new crate

The dependency graph D1 already declares `ingestion → llm, db`. NER is
ingestion-domain logic; a separate crate would add ceremony without a seam.
New layout:

```
crates/ingestion/src/
  ner/
    mod.rs        — NerProvider trait, NerEntity/NerFact/NerResult, stage enum
    regex.rs      — RegexNer
    prompts.rs    — embedded minijinja system/user templates + loader
    llm_schema.rs — GenerateJSONSchema port
    llm.rs        — LlmNer (render → cache → call → parse/validate)
    llm_cache.rs  — sha256 key + lazy llm_ner_cache table store
    composite.rs  — CompositeNer + auto-publish filter
  entities/
    mod.rs        — Resolver
    similarity.rs — jaro_winkler, bigrams, normalize_name
    cluster.rs    — union-find batch clustering, canonical_proto, scope_metadata
```

Alternatives: separate `crates/ner` (rejected: no independent consumer, D1 fixed);
putting resolver into `crates/graph` (rejected: oracle keeps it in ingestion and
the pipeline change consumes it from there; graph crate owns CEL linkers instead).

## D2 — Provider trait mirrors the oracle surface, Rust-ified

```rust
pub trait NerProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract_entities(&self, content: &str, metadata: &serde_json::Map<String, serde_json::Value>)
        -> Result<Option<NerResult>, IngestionError>;
}
```

`Option<NerResult>` models the oracle's `(*Result, error)` where `nil, nil`
means "nothing found" (empty content / no rules). Metadata bag stays
`serde_json::Map` — same type chunk metadata already uses (sources change B-tree
ordering), so enrichment is a plain extend. Object-safe (`Send + Sync`) for
storage in the future Runner.

## D3 — RegexNer: config-driven, compiled once

Domain configs already carry `RegexRuleDef` with pre-compiled `CompiledPattern`
(config-module deliverable). RegexNer flattens `(domain, rules)` into prepared
rules at construction; extraction iterates rules, prefers capture group 1 when
present (`NumSubexp() > 0` equivalent: `captures_len() > 1`), trims, dedups by
`(name, type, domain)`, stamps `rule_id` into entity metadata. Empty content or
zero rules → `Ok(None)`. Parity: port `regex_ner_test.go` cases.

## D4 — Prompts: embedded minijinja + explicit context block (BINDING)

Port `system.tmpl` / `user.tmpl` to minijinja templates embedded via
`include_str!`, loaded through a loader that prefers user overrides on disk
(same pattern as `crates/graph/src/prompts.rs`: embedded defaults + override
path + `TemplateHashes` for provenance).

**Binding requirement (human decision 2026-08-23):** the oracle's breadcrumbs
reached the LLM only implicitly — baked into `chunk.Text` prefixes by its
markdown chunker. Our chunks carry clean slices + structured metadata
(`breadcrumbs`, `section_title`). The rendered user prompt MUST include an
explicit context block, e.g.:

```
---
Document context:
  Section path: A > B > C
```

rendered from chunk metadata when present, omitted when absent. This restores
oracle *behavior* (LLM sees document position) without corrupting byte offsets
(our fix of the oracle bug recorded in ingestion-sources).

Deviation note: the oracle sends chunk content as a separate attachment message
(`Attachments: ["CONTENT:\n\n"+content]`); our `LlmClient::call(system, user,
schema, schema_name)` has no attachments parameter, so content renders directly
into the user prompt body. Wire shape differs; extracted-output parity is what
matters (fixtures compare parsed results, not HTTP bodies).

## D5 — LlmNer: per-domain loop with schema + validation

Construction requires ≥1 domain config (oracle errors otherwise). Extraction
loops domains in config order: render system (entity/relation defs, JSON example)
+ user (type lists + content + context block); build cache key; on miss call
`LlmClient::call` with `GenerateJSONSchema(cfg, requires_schema)` port and
schema name `ner_result`; parse response:

- entities: skip empty names; confidence default **0.5**, accepted only in [0,1];
  description truncated to ≤500 chars at last sentence boundary (.!?;) else hard cap;
- facts: skip any of the five required fields empty;
- metadata validation: drop string values containing "implied by context" /
  "not explicitly stated"; `version` field must match `^[vV]?[0-9]+([._-][a-zA-Z0-9]+)*$`
  after reject-list screening (" years", "-to-", "approximately", …);
- tag every entity/fact with the domain name.

## D6 — LLM cache: sha256 key, lazily created table

`BuildCacheKey(server, model, temperature, max_tokens, system, user, content)`
→ sha256 hex of `:`-joined parts (temperature formatted `%g`). Store is a thin
rusqlite wrapper over table `llm_ner_cache (cache_key TEXT PRIMARY KEY,
result TEXT NOT NULL)` created with `CREATE TABLE IF NOT EXISTS` on first use —
exactly the oracle's runtime behavior; the frozen v5 migration shape is untouched
(the table is absent from the oracle's own migrations too). Corrupted entries are
treated as misses; nil store = caching disabled. Key format is internal-only —
never compared across implementations.

## D7 — CompositeNer: ordered stages + threshold filter

Stages come from `GlobalNerConfig.methods` (`regex` | `llm`; unknown stage →
construction error listing valid values; `prose` rejected with a pointer to the
deferral decision). Runs providers in order, short-circuits on provider error,
enriches each entity/fact metadata with source metadata + `provider` name
(provider-set domain preserved). Then per-domain auto-publish filtering:
entities below their domain's `auto_publish_threshold` are dropped; facts whose
subject OR object was dropped are dropped (cascade). Entities from unknown
domains pass through. Parity: port `composite_test.go` scenarios.

## D8 — Resolution primitives: pure, rune-aware, parity-pinned

`normalize_name` (trim + lowercase + collapse whitespace), rune-aware bigrams
(names <2 runes map to themselves), Jaro-Winkler with match-window
`max(len)/2 - 1`, prefix bonus ≤4 runes × 0.1. Batch clustering: bigram blocking
(`domain:type:bigram` keys — cross-domain/cross-type never merge), union-find
with path compression over checked pairs, clusters keep first-seen order;
canonical prototype = longest name, ties → first. `scope_entity_metadata` drops
document-level fields (`url`, `image_paths`, `page_links`, `categories`),
keeps provenance (`source_file`, `source_type`, `space`), rewrites `title` to
the entity name when present as a string. Full differential parity vs oracle
`similarity_test.go` + pure parts of `resolver_test.go` (Cyrillic cases included).

## D9 — Resolver: hydrated blocking index over db DAOs

State: `Mutex`-protected maps (`domain:name → id`, `id → canonical name`,
`domain:type:bigram → ids`, `id → domain`) + `hydrated` flag + threshold from
`ResolverConfig.similarity_threshold` (default 0.8, preset.rs already applies it).
Lazy hydrate loads all entities via `EntityDao::list` once; incremental updates
index in-memory immediately. Operations (all take `&DbExecutor` — pool or tx,
mirroring the oracle's DBTX abstraction):

- `find_best_candidate`: exact normalized-name hit → score 1.0; else best
  Jaro-Winkler over bigram blocks (same type AND domain only);
- `lookup`: resolve-only, unmatched → `None`;
- `resolve_one`: merge-or-create; on merge, promote longer canonical names
  (update DAO + both normalized-name keys + new bigram blocks); missing candidate
  mid-flight (GC deleted it) → full rehydrate + retry once;
- `lookup_or_create(_with_stats)`: per-entity resolve-or-create, links created
  ids to doc via `EntitySourceDao::link_batch`, returns aligned ids (+ created count);
- `add_entities`: cluster batch first, one canonical per cluster, dedup ids,
  link all to doc, return resolved entities.

Creation persists scoped metadata JSON + description via `EntityDao::get_or_create`.
Parity: port DB-backed scenarios from `resolver_test.go` against in-memory SQLite.

## D10 — Error handling

Reuses `IngestionError` (thiserror). LLM/cache/DB failures are fatal for the
extraction call (oracle behavior: `return nil, err`); "nothing found" is
`Ok(None)`, never an error.
