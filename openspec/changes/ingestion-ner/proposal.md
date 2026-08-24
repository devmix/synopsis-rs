# Proposal: ingestion-ner

## Change name
`ingestion-ner`

## Why

Change 2 of the approved 3-change ingestion series (sources → **ner** → pipeline,
human decision 2026-08-23). The sources change delivered the `Source` composites
producing chunks with clean text + metadata; nothing yet extracts knowledge from
those chunks. The oracle extracts entities/facts per chunk via a staged NER
pipeline (`../synopsis/internal/ingestion/ner/`) and deduplicates them into the
graph via an entity resolver (`../synopsis/internal/ingestion/entities/`). Both
are prerequisites for the pipeline change (3/3), which wires parse → chunk → NER
→ resolve → persist.

## What changes

New modules in `crates/ingestion` (dependency graph D1 already allows
ingestion → llm, db):

1. **Core types + Provider trait** — `NerEntity`, `NerFact`, `NerResult`,
   object-safe `NerProvider` (`name()` + `extract_entities()`).
2. **RegexNer** — rules from domain configs (`extraction.regex_rules`,
   `CompiledPattern` already in config crate); capture-group preference,
   dedup by name+type+domain, `rule_id` provenance.
3. **LLM prompts** — embedded minijinja ports of `configs/prompts/ner/{system,user}.tmpl`
   with user-override loading (graph crate precedent). **Binding requirement
   (human decision 2026-08-23): the prompt must explicitly render breadcrumb /
   section context from chunk metadata** — the oracle got this implicitly through
   prefixed chunk text; our chunks carry clean slices, so context is injected as
   an explicit prompt block.
4. **LlmNer** — per-domain loop, JSON-schema structured output
   (`GenerateJSONSchema` port), response parsing/validation (default confidence
   0.5, description truncation ~500 chars at sentence boundary, uncertainty-comment
   filtering, version-string validation), SHA-256 cache over a lazily created
   `llm_ner_cache` table (oracle behavior: runtime `CREATE TABLE IF NOT EXISTS`;
   NOT part of the frozen v5 migration shape).
5. **CompositeNer** — ordered stages from `GlobalNerConfig.methods`
   (`regex` | `llm`; `prose` deferred — see Non-goals), source-metadata
   enrichment + provider tag, per-domain auto-publish-threshold filtering with
   fact cascade.
6. **Entity resolution primitives** — rune-aware Jaro-Winkler, bigram blocking,
   union-find batch clustering, canonical = longest name, entity-metadata scoping.
7. **Resolver** — hydrated in-memory blocking index over the DB, exact-name and
   similarity matching, `Lookup` / `LookupOrCreate(+WithStats)` / `AddEntities`
   backed by existing db-crate DAOs (`entity`, `entity_sources`).

## Non-goals

- **Prose-NER provider** (`prose_ner.go`): tsawler/prose is Go-only (cgo bindings
  to go-prose models); no maintained Rust equivalent exists. gline-rs / rust-bert
  rejected — they would drag in a second ONNX/tokenizer stack against the frozen
  stack decision (human decision 2026-08-23). LLM-NER covers the same need.
  Revisit only if a credible Rust candidate appears.
- No pipeline wiring (change 3/3 owns Runner orchestration).
- No new DB migration: `llm_ner_cache` is created lazily at runtime exactly like
  the oracle's cache.Store does (it is not in the oracle's 5 migrations either —
  verified `migrations/001_schema.sql` + `005_app_kv.sql`).
- No relation resolution/graph linking beyond what Resolver does (linker CEL
  expressions belong to graph crate, already delivered).

## Risks

- LLM-NER tests need HTTP mocking — reuse the llm crate's mock-TcpListener pattern.
- Jaro-Winkler must be rune-aware (Cyrillic) — differential parity vs oracle
  `similarity_test.go` pins this.
- Cache-key format must stay internal (never compared across implementations);
  documented as such.
