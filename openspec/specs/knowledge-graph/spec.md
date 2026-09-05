# knowledge-graph Specification

## Purpose

Local knowledge graph: a derived in-memory index over SQLite (the source of truth), BFS traversal with domain boundaries, entity lookup, CEL expression linking, and a cross-domain pipeline — the foundation of the graph MCP tools.

## Requirements

### Requirement: Index construction and loading

The `graph` crate builds the derived index from SQLite at startup (the `load_on_startup` flag): nodes are entities (`entities`), edges are relations (`entity_links`) + fact edges; name→ID ("domain:lowercase_name", O(1)) and type→nodes indexes are supported. Rebuilding the index is a full rebuild; incrementality is not required. The source of truth is SQLite; the index itself is not persistent.

#### Scenario: Loading at startup
- **WHEN** the server starts with `load_on_startup=true` and a non-empty database
- **THEN** the index is built from the entities/entity_links tables; node/edge counters match the database contents

#### Scenario: Graph disabled
- **WHEN** `enable_graph=false` or `load_on_startup=false`
- **THEN** the index is not built; graph queries return an explicit "graph unavailable" state (not an error)

#### Scenario: Empty database
- **WHEN** the index is built on a database without entities
- **THEN** an empty valid index is built; queries return empty results

### Requirement: BFS traversal

Traversal from an entity implements the following contract: max_depth (default 5, maximum 10), max_nodes (default 1000; edges are included only between nodes that made it in), direction outgoing/incoming/both. Domain boundaries are strict: fact edges never cross domains; crossing is possible ONLY via entity links and only when `FollowEntityLinks=true`.

#### Scenario: Depth traversal
- **WHEN** a traversal is executed with max_depth=N
- **THEN** nodes are returned level by level of BFS up to depth N; the result is deterministic

#### Scenario: Node count limit
- **WHEN** the traversal reaches max_nodes
- **THEN** the walk stops; len(edges) ≤ len(nodes)

#### Scenario: Domain boundary for fact edges
- **WHEN** BFS encounters a fact edge leading into another domain
- **THEN** the transition is blocked regardless of flags

#### Scenario: Transition via entity links
- **WHEN** `FollowEntityLinks=true` and an entity-link edge into another domain is encountered
- **THEN** the transition is allowed; with `FollowEntityLinks=false` it is blocked

### Requirement: Entity lookup

The index provides lookup: exact by (domain, name) — O(1) via name→ID, case-insensitive; partial — prefix + substring within a domain, results sorted.

#### Scenario: Exact lookup
- **WHEN** an entity is looked up by domain and name
- **THEN** the ID is returned in constant time; the case of the name does not matter

#### Scenario: Partial lookup
- **WHEN** a lookup is performed by prefix/substring
- **THEN** a sorted list of matching entities of the domain is returned

### Requirement: Metrics and DOT export

The index provides statistics (node count, edge count, average degree) and export in the DOT format, valid for graphviz.

#### Scenario: Statistics
- **WHEN** statistics of a built index are requested
- **THEN** node_count/edge_count/avg_degree match the contents

#### Scenario: DOT export
- **WHEN** the index is exported to DOT
- **THEN** the output parses in graphviz; nodes are attributed with domain/type

### Requirement: CEL expression linking

The crate evaluates CEL linking expressions from ontology.xml through the `cel` crate (replacing cel-interpreter, human decision 2026-08-22). The expression context provides the contract functions: `facts(entity)`, `has_fact(entity, key, value)`, `chunks(entity)`, `chunk_contains(entity, text)`, `neighbors(entity)`, `path_exists(from, to, max_depth)`; heavy indexes (FactIndex/ChunkIndex/GraphIndex) are loaded lazily (scope cache). The evaluation result is the decision "whether to link the pair of entities".

#### Scenario: Rule evaluation
- **WHEN** an ontology rule contains an expression with has_fact/facts
- **THEN** the function returns the entity's fact data from SQLite; the expression evaluates without panicking

#### Scenario: Graph functions
- **WHEN** an expression uses neighbors/path_exists
- **THEN** the functions read the in-memory index; path_exists is bounded by max_depth

#### Scenario: Lazy indexes
- **WHEN** an expression does not use chunks()
- **THEN** ChunkIndex is not loaded (scope cache)

### Requirement: Cross-domain linking pipeline

The pipeline applies the methods in configuration order: `equals` (exact name match), `expression` (CEL rules), `llm` (LLM comparison of the pair through an OpenAI-compatible client). Each method is idempotent: re-linking does not create duplicates (ON CONFLICT DO NOTHING via EntityLinkDao). The `linker.disabled` flag disables the LLM method. The LLM method, for each pair: loads the chunk context of both entities (up to 3 texts per entity), renders the system/user templates (loaded from `prompts_path`, with embedded fallback), calls the LLM client with structured output, parses the decision `{same_entity, confidence, reasoning}`, and creates a link only when confidence ≥ `llm_confidence_threshold` (method='llm', evidence=reasoning, confidence clamped to [0,1]); the decision is cached in the `llm_linker_cache` table (cache DB, `migrations/cache/1-init/up.sql`) by hash (entity pair + template hashes) — a repeat call for the same pair does not hit the LLM. A call/parse failure of one pair does not abort the pipeline (it is recorded in the results).

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
