# Tasks: multilingual-entity-resolution

Conventions (AGENTS.md): each task is self-contained for a fresh agent
(~100k context) — goal, File scope, Dependencies, machine-checkable
Acceptance criteria; final diff per task ≤ ~500 lines of code + tests;
gates = `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`. **Test placement (user decision 2026-09-10):** tests go in the
crate's `tests/` directory as integration tests against the public API +
public test support; a `#[cfg(test)]` module in `src/` only for tests that
need private internals (reason in the module doc). **NDA:** subject-matter entity names must NOT appear in code,
tests, or any artifact of this change — tests use generic words only;
dataset-specific pairs are discovered by DB query at runtime, never
hardcoded.

## 1. Stemming utility (crates/utils)

- [x] 1.1 Add the `utils::text` stemming module.
  - **Goal:** script-based word stemming shared by ingestion and graph (design D1 of the change design).
  - **File scope:** root `Cargo.toml` (workspace deps: `rust-stemmers = "1.2.0"`), `crates/utils/Cargo.toml`, `crates/utils/src/text.rs` (new), `crates/utils/src/lib.rs` (module export + docs).
  - **Dependencies:** none.
  - **Acceptance:** `text` exposes `detect_script(word) -> Script` (Latin / Cyrillic / Other, by first alphabetic rune), `stem_word(word) -> String` (Latin→English Porter, Cyrillic→Russian Snowball, Other→lowercased identity), `stem_name(name) -> String` (per word, rejoined with single spaces; input lowercased before stemming). Tests: EN vectors (`"gates"`→`"gate"`, `"fruitlessly"`→`"fruitless"`), RU vectors from the crate's own Snowball test vocabulary (generic words only, e.g. a city-noun prepositional/plural pair), CJK identity, multi-word name. `cargo test -p utils` green; workspace gates green.

## 2. Schema and transactional merge (crates/db)

- [x] 2.1 Add the `entity_aliases` migration and DAO.
  - **Goal:** new table + read/write DAO (data-schema spec: `entity_aliases table and transactional merge`).
  - **File scope:** `migrations/knowledge/2-entity-aliases/up.sql` (new — `1-init` is shipped and never edited), `crates/db/src/` (new `EntityAliasDao`: `insert_or_ignore(entity_id, alias)`, `aliases_of(entity_id) -> Vec<String>`, `alias_map() -> HashMap<String, i64>` for resolver hydration), DAO module wiring.
  - **Dependencies:** none.
  - **Acceptance:** migration SQL exactly per design D2 (columns `entity_id` NOT NULL REFERENCES entities(id) ON DELETE CASCADE, `alias` TEXT NOT NULL, UNIQUE(entity_id, alias), UNIQUE index on alias). Fresh-DB test: table present, constraints reject a duplicate (entity_id, alias) and a duplicate alias of another entity. `PRAGMA user_version` advances to 2. Gates green.
- [x] 2.2 Implement transactional `merge_entities(into, from)`.
  - **Goal:** the single merge primitive used by the CLI (6.1) and the linker (7.2).
  - **File scope:** `crates/db/src/` (merge module + public API `merge_entities(conn, into_id, from_id) -> Result<MergeSummary>`; `MergeSummary` = re-pointed row counts per table + surviving name + recorded aliases).
  - **Dependencies:** 2.1.
  - **Acceptance:** preconditions (both exist, same type+domain, into≠from) checked before any write — violation returns Err and leaves the DB byte-identical (test via snapshot or row counts). One transaction re-points `facts` (subject AND object; respect UNIQUE (subject, object, predicate) — colliding rows are dropped, not duplicated), `chunk_entities` (PK collisions collapsed), `entity_sources` (INSERT OR IGNORE), `entity_links` (self-links after re-point skipped, duplicates ignored), records `from.name` + `into.name` as aliases of `into` (INSERT OR IGNORE), deletes the `from` row. Tests: full fixture (both entities with facts/chunks/sources/links in both directions) → no row references the deleted id, both names aliased, canonical row unchanged; every precondition violation → Err + unmodified DB. Gates green.

## 3. Resolution tiers (crates/ingestion)

- [x] 3.1 Add `match_key` / `stem_key` to `utils::text`; keep `similarity.rs` as the JW home.
  - **Goal:** the tier keys, shared with the linker (graph cannot depend on ingestion — design D1 of the change).
  - **File scope:** `crates/utils/src/text.rs` (`normalize` (trim + lowercase + whitespace collapse — same semantics as `ingestion::ner::normalize`), `strip_articles` (leading `the `/`a `/`an `, word-boundary, on the normalized name), `match_key(name) = strip_articles(normalize(name))`, `stem_key(name) = stem_name(strip_articles(normalize(name)))`), `crates/ingestion/src/entities/similarity.rs` (re-export the key functions; `jaro_winkler`/`bigrams` stay here), call-site updates inside ingestion.
  - **Dependencies:** 1.1.
  - **Acceptance:** `ner::normalize` behavior unchanged (existing tests pass; it may delegate to `utils::text::normalize`). New tests (generic words): `"The City of Ash"` ≡ `"city of ash"` match_key; `"A"`/`"An"` stripping; RU case-variant pair yields equal stem_key with JW of the originals < 0.85 (assert the actual JW value in the test to pin the scenario); EN `"gates"`/`"gate"` stem_key equal; CJK name → stem_key = normalized identity; `match_key` of a name without a leading article is unchanged. Gates green.
- [x] 3.2 Wire the tier order into the resolver and in-batch clustering, with alias memory.
  - **Goal:** four-tier `find_best_candidate` (article-stripped exact → stem → alias → JW) and the same order in `cluster_batch`; every non-creating resolution records the surface form in `entity_aliases` (design D3).
  - **File scope:** `crates/ingestion/src/entities/resolver.rs` (BlockingIndex: add stem map `(domain, type, stem_key) → ids` and alias map `name → entity_id` hydrated from `EntityAliasDao::alias_map()` alongside the existing DB hydration; tier order in `find_best_candidate`; alias write-through on resolution), `crates/ingestion/src/entities/cluster.rs` (same tier order in `cluster_batch`), DB-handle plumbing for hydration/writes (follow the existing hydration seam).
  - **Dependencies:** 1.1, 2.1, 3.1.
  - **Acceptance:** tests (generic names): (a) `The X`/`X` same type+domain → one entity, longest name, regardless of threshold; (b) RU case-variant pair with JW < 0.85 → one entity via stem tier; (c) cross-script pair → NOT merged by tiers 1–2 (stays separate absent an alias); (d) JW near-miss pair scoring ≥ 0.851 (distinct entities) → still not merged; (e) JW pair ≥ 0.85 → merged (existing behavior preserved); (f) domain isolation preserved for all tiers; (g) after a resolution, the surface form is in `entity_aliases` and a second lookup of it resolves by the alias map without touching JW (assert via a flag/spy or by pre-seeding the alias map); (h) `cluster_batch` merges the same pairs `find_best_candidate` does (differential test on a mixed batch). Gates green.

## 4. Dataset alias map (crates/config + crates/ingestion)

- [x] 4.1 Load `ontology/aliases.yaml`. — **SUPERSEDED by 4.3** (user decision 2026-09-10: aliases move into the ontology XML; the YAML loader is reverted).
  - **Goal:** optional per-dataset alias→canonical map, validated at startup (config-format spec: `Dataset alias map (aliases.yaml)`).
  - **File scope:** `crates/config/src/` (new loader next to the ontology loader; dataset config plumbing so the map reaches the ingestion bootstrap; error variants), `crates/config/tests/` (loader tests).
  - **Dependencies:** none (parallel to 3.x).
  - **Acceptance:** missing file → empty map, no error; empty file → empty map; valid map → loaded; file that is not a mapping → configuration error; duplicate alias key (same alias, two canonicals) → configuration error (detect explicitly — do not rely on the YAML crate's silent last-wins). Tests cover all five. Gates green.
- [x] 4.2 Resolver tier 3: alias → canonical.
  - **Goal:** tier 3 of `find_best_candidate` consults the alias map (design D4, revised: the ontology `<aliases>` map); creation under the canonical name when no entity exists yet.
  - **File scope:** `crates/ingestion/src/entities/resolver.rs` (tier 3 between stem and JW; alias map passed via the existing config/RunnerParams plumbing), bootstrap wiring (CLI bootstrap derives the map from the already-parsed `GlobalConfig`/`DomainConfig` ontology configs — no separate file read; the 4.1 `load_aliases` call is removed).
  - **Dependencies:** 3.2, 4.3.
  - **Acceptance:** tests: (a) alias name + canonical entity exists (same type+domain) → resolves to it with no similarity work (assert via spy/flag); (b) alias name, no entity yet → entity created **under the canonical name**, alias recorded; (c) canonical name extracted directly later → resolves to the same entity; (d) alias entry for a different type+domain → no cross-type resolution (the map keys on name only; the type+domain gate still applies); (e) empty map → behavior identical to 3.2. Gates green.
- [x] 4.3 Move the alias map into the ontology XML; revert the `aliases.yaml` loader.
  - **Goal:** design D4 (revised 2026-09-10): `<aliases>` blocks in `global.xml` and `domain_*.xml` replace `ontology/aliases.yaml` (config-format spec: `Dataset alias map (ontology <aliases>)`).
  - **File scope:** `crates/config/src/ontology.rs` + `crates/config/src/domain.rs` (parse an optional top-level `<aliases>` block into both `GlobalConfig` and `DomainConfig`; `AliasDef { name, canonical }`; load-time validation: non-empty name/canonical, duplicate `name` → configuration error), a public helper that derives the flat `HashMap<String, String>` union (global + all domains; duplicate `name` across files → error), `crates/config/src/aliases.rs` + `crates/config/src/io_util.rs` + `crates/config/tests/aliases.rs` (revert the 4.1 YAML loader: remove `load_aliases`/`ALIASES_YAML_FILE`, restore `read_yaml_file` to its pre-4.1 shape, remove/replace the YAML loader tests with XML parsing tests), `crates/config/tests/` (ontology/domain parsing tests), `crates/config/src/lib.rs` (exports).
  - **Dependencies:** none (replaces 4.1).
  - **Acceptance:** (a) ontology with no `<aliases>` → empty map; (b) global block and/or domain block present → union map loaded (a global alias and a domain alias both resolvable); (c) duplicate `name` within one file → validation error; (d) duplicate `name` across `global.xml` and a domain file → validation error; (e) empty `name` or `canonical` → validation error; (f) `load_aliases`/`ALIASES_YAML_FILE` no longer exported, `read_yaml_file` byte-equivalent to its pre-4.1 behavior (existing config tests pass). Tests use generic words. Gates green.

## 5. NER canonical-form directive

- [x] 5.1 Add the dictionary-form directive to the NER system prompt (both copies).
  - **Goal:** entity names extracted in dictionary/nominative form, bare proper names without a leading article (entity-extraction spec: `NER prompt canonical-form directive`).
  - **File scope:** `crates/ingestion/src/ner/templates/system.tmpl`, `workspace/configs/prompts/ner/system.tmpl` (the two MUST stay byte-identical — one added sentence, generic wording, no dataset names), `crates/ingestion/src/ner/` (template-pairing test if present; cache-key test asserting the template hash changed).
  - **Dependencies:** none (independent of 3.x/4.x).
  - **Acceptance:** both files contain the directive; byte-identity check between the two copies passes (existing or new test); the NER cache key (template SHA-256) differs from the pre-change value (test pins the new hash or asserts inequality against the old constant). Gates green.

## 6. CLI `db merge-entities`

- [x] 6.1 Add the `db merge-entities <id> --into <id>` action.
  - **Goal:** surgical manual merge (cli-surface spec: `db subcommand`).
  - **File scope:** `crates/cli/src/cli.rs` (subcommand definition next to `stats|clear`), `crates/cli/src/db.rs` (handler: resolve dataset DB, load both entities, same type+domain check with a clear error, stats block, `Confirm merge? [y/N]` on stdin, `merge_entities` call, summary: re-pointed counts + surviving name + recorded aliases), `crates/cli/tests/db_cli.rs` (new cases).
  - **Dependencies:** 2.2.
  - **Acceptance:** `--help` shows the new action; tests (stdin-driven, existing `db_cli.rs` pattern): merge with `y` → summary printed, duplicate row gone, aliases recorded; `n`/other → no modification; nonexistent id → error, no modification; different type or domain → error, no modification; one-shot: no model/ONNX load (the command opens only the dataset-bound DB). Gates green.

## 7. Within-domain cross-script linking (crates/config + crates/graph)

- [x] 7.1 Ontology key `merge_confidence_threshold`.
  - **Goal:** the merge threshold for the cross-script merge action (config-format spec: `Ontology merge confidence threshold`).
  - **File scope:** `crates/config/src/ontology.rs` (`CrossDomainLinksConfig` field + default 0.95 + validation (0,1] consistent with `llm_confidence_threshold`), `crates/config/tests/ontology.rs`.
  - **Dependencies:** none.
  - **Acceptance:** absent → validates, effective 0.95; set to a value in (0,1] → used; 0 or >1 → validation error naming the key. Gates green.
- [x] 7.2 Cross-script candidate generation + merge/link actioning in the linker.
  - **Goal:** same-domain, same-type, different-script pairs judged by the LLM method; ≥ merge threshold → `merge_entities`; ≥ link threshold → `same_entity` link; below → no action (knowledge-graph spec: `Cross-domain linking pipeline`, new scenarios).
  - **File scope:** `crates/graph/src/linker.rs` (candidate generator: same domain+type, `utils::text::detect_script` differs, `match_key`/`stem_key` both differ (i.e. not resolved by the resolution tiers); pairs routed to the llm method only; decision actioning with canonical selection — greater `entity_sources` count → longer name → lower id; a seam (conn/dao) for `db::merge_entities` and the source-count query in the linker params), `crates/graph/tests/` (unit + pipeline tests, existing stub-LLM pattern).
  - **Dependencies:** 1.1, 2.2, 3.1, 7.1.
  - **Acceptance:** tests (generic names, stub LLM): (a) cross-script same-type pair with stub confidence 0.97 → merged, canonical = more sources (assert the tie-breakers in dedicated cases), both names aliased; (b) confidence 0.8 → `same_entity` link, no merge; (c) confidence 0.5 → no action, decision cached; (d) same-script pair → NOT generated as a candidate; (e) pair already sharing match_key or stem_key → NOT generated; (f) repeat run → cached decision, no second LLM call, no duplicate link/merge; (g) per-pair stub failure → recorded, pipeline continues; (h) `linker.disabled=true` → no cross-script actioning. Gates green.

## 8. MCP aliases (crates/mcp)

- [x] 8.1 Additive `aliases` field in entity payloads.
  - **Goal:** `aliases` (array of strings, own name excluded, empty when none) in `get_entity_dossier`'s dossier entity, `catalog_entities` entries, and `search` result entities (mcp-contract spec: `Tool parameter and response schemas`).
  - **File scope:** `crates/mcp/src/tools/dossier.rs` (`DossierEntity` + `dossier_entity`), `crates/mcp/src/tools/entities_catalog.rs`, `crates/mcp/src/tools/search.rs`, db access for the batched alias lookup (PK lookup per entity, batched per response), fixture re-recording for the three tools (project fixture process).
  - **Dependencies:** 2.1.
  - **Acceptance:** field appended after the last existing field (existing field order untouched); empty array when no aliases; unit tests for the mapping; re-recorded fixtures pass the machine parity diff (tools/list parameter schemas unchanged). Gates green.

## 9. Documentation, dataset data, re-ingest

- [x] 9.1 Update user-facing documentation.
  - **Goal:** docs in the same change (AGENTS.md rule): README + site docs.
  - **File scope:** `README.md` (CLI table: `db merge-entities`; config layout: ontology `<aliases>`), `site/docs/` (entity-resolution concept: the four tiers + alias memory; guide: manual merge + alias map authoring; reference: `merge_confidence_threshold`, ontology `<aliases>` schema). No subject-matter names — generic examples only.
  - **Dependencies:** 6.1, 4.3, 7.1 (documented surface must exist).
  - **Acceptance:** every documented command/flag/key exists in the code (spot-check against `--help` and the config loader); no dataset entity names in any doc file. Gates green (docs do not break cargo).
- [x] 9.2 Dataset alias map + re-ingest + verification.
  - **Goal:** the affected dataset ends with one entity per real-world entity (design D9).
  - **File scope:** `workspace/datasets/<affected-dataset>/ontology/` (`<aliases>` block in `global.xml` and/or the domain files — local data; pairs discovered by DB query, NOT hardcoded from specs), re-ingest run, verification queries. Pre-state snapshot: before touching anything, query the dataset knowledge DB for (a) same-type+domain pairs sharing a tier-1 or tier-2 key, and (b) same-type+domain cross-script pairs; save the ids locally (e.g. a scratch file outside the repo or in `/tmp`).
  - **Dependencies:** 3.2, 4.2, 5.1, 7.2, 8.1 (all code in place first).
  - **Acceptance:** the `<aliases>` block contains the deterministic pairs (abbreviation/cross-lingual pairs the LLM must not be needed for), canonical = the entity with more source documents; `synopsis db clear` (confirm) + `synopsis serve` re-ingest completes; post-state: (1) zero same-type+domain pairs sharing a tier-1 or tier-2 key; (2) every pre-state duplicated pair from the snapshot is a single entity with the variants recorded in `entity_aliases`; (3) the cross-script pairs are either merged (LLM ≥ 0.95) or `same_entity`-linked (≥ 0.7) — none left as silent duplicates; (4) MCP `get_entity_dossier` for a merged entity shows the aliases. If the user's LLM endpoint is unavailable at run time, record which cross-script pairs remain unlinked and stop (do not fake the LLM pass).
