# Design: translate-to-english

## Decisions

### D1: Translate prose, not functional data

**Decision.** Translate Russian PROSE (`openspec/config.yaml`, the 14 specs, the 5
ADRs, the `.opencode/` agent + plan files, and Rust comments) to English. Leave
functional DATA in Russian: the ontology XML `description` / `<synonym>` values,
the demo corpus, and Rust test-data string literals.

**Why.** The ontology's Russian synonyms/descriptions are its functional purpose
(entity-linking of Russian text); translating them would break the ontology and
its byte-identical test fixtures (`crates/config/tests/data/**` mirrors
`workspace/datasets/edtech/ontology/**`). The Rust test data (Russian names/text)
is realistic input for a Russian-context RAG; translating it changes what the
tests exercise. Prose carries no behavior — translating it is safe.

**Alternative rejected.** Translating everything (including data) breaks the
ontology + tests; translating nothing leaves the repo half-Russian.

### D2: Language-only spec translation; headings translated, behavior identical

**Decision.** Translate each spec's prose (requirement/scenario text) to English,
preserving every requirement, scenario, and Given/When/Then exactly in meaning.
Section headings are translated (e.g. "Набор инструментов" → "Tool set").

**Why.** The specs are the frozen contract's record; the contract is the
BEHAVIOR, which is unchanged. Headings are labels, not behavior. Translating them
makes the specs consistent with the English code/README.

**Alternative rejected.** Keeping Russian headings leaves the specs half-Russian
and inconsistent with the English code comments.

### D3: Fixed English section names (cross-reference consistency)

**Decision.** The 2 spec names referenced by code get fixed English names,
defined here so the spec tasks (3/4) and the Rust-comment task (5) stay in sync:

- `mcp-contract` "Набор инструментов" → **"Tool set"**
- `config-format` "Отсутствующий onnx.yaml" → **"Missing onnx.yaml"**

**Why.** `crates/mcp/src/server.rs:325`, `crates/mcp/tests/server_units.rs:20`
(mcp-contract) and `crates/config/src/onnx.rs:12` (config-format) quote these
section names in comments; after translation the comments must match the specs.
Fixing the names here avoids drift between tasks 3/4 and 5.

**Verified.** These are the ONLY code→spec cross-references: a full grep of
quoted Cyrillic in `crates/**` comments found no others.

### D4: Criterion label chars normalized to Latin (same position)

**Decision.** Cyrillic criterion labels in Rust comments map to the same-position
Latin letter (the standard sequential-label convention): а→a, б→b, в→c, г→d, д→e,
е→f, ж→g, з→h, и→i, к→j, л→l.

**Why.** These are sequential criterion labels (1st, 2nd, 3rd …) in comments; the
Latin equivalent is the same-position Latin letter. They are local to the test
files (no external reference), so a consistent per-file mapping is safe.

**Note.** Some labels carry a suffix inside the parens (e.g. `(и, cont.)`); only
the leading letter is normalized → `(i, cont.)`.

### D5: Translation tone — natural technical English

**Decision.** Translate to natural, precise technical English (preserve meaning,
not word-for-word). Keep technical terms, identifiers, code spans, and proper
names (crate names, tool names, ADR numbers, file paths, `D1…D8` / `ADR 0001…`
refs) unchanged.

**Why.** Word-for-word translation reads awkwardly in technical docs; natural
English preserves the meaning while matching the existing English docs.

## Reference

- The current Russian prose (the existing `config.yaml`, specs, ADRs, agent/plan
  files) is the source of truth for each translation.
- The 2 fixed section names (D3) are the reference for the Rust comment
  cross-references.
- No recorded fixtures are affected (the translation touches no test data).
