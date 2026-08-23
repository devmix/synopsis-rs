//! Cross-domain entity linking pipeline (design D6, task 1.9).
//!
//! Oracle mapping: `../synopsis/internal/relations/{entity_links.go,
//! expression_linker.go}` — a functional copy, re-architected for Rust
//! (migration principle: not a code copy).
//!
//! # Pipeline
//!
//! [`build_entity_links`] loads all entities, enumerates the candidate pairs
//! (same type + equal normalized name, different normalized domain) and
//! applies the ontology's methods in the configured order (`equals`,
//! `expression`, `llm`). Every method is idempotent: `entity_links` has the
//! composite primary key `(subject, target, relation_type)`, the DAO insert
//! is `INSERT OR IGNORE`, and self-links are rejected by the DAO — so a
//! repeated run creates no duplicates.
//!
//! # Methods
//!
//! - `equals` — normalized name match across domains. A name needs at least
//!   `min_words` words (default [`DEFAULT_EQUALS_MIN_WORDS`], the oracle's
//!   `DefaultEqualsMinWords`); shorter names are skipped silently (oracle
//!   parity: they count neither as created nor as skipped). Creates a
//!   `same_entity` link with confidence [`EQUALS_CONFIDENCE`].
//! - `expression` — ontology CEL rules evaluated against the pair bindings
//!   `A`/`B` (the entity map, the oracle's `entityToMap`), with all six
//!   contract functions (tasks 1.7/1.8) registered. Rules are applied in
//!   priority order (higher first; ties keep the ontology's order — the
//!   oracle's unstable sort made tie order nondeterministic), the first rule
//!   evaluating to `true` wins, and its `relation-type` becomes the link's
//!   relation with confidence [`RULE_CONFIDENCE`].
//! - `llm` — STUB (design D6, human decision 2026-08-21): no LLM client in
//!   this build. It records its skip in [`LinkResult::notes`] and writes
//!   nothing. `LinkerConfig::disabled` excludes the method entirely (the
//!   oracle's `Linker.Disabled` check in the ingestion runner).
//!
//! # Deviations from the oracle
//!
//! - No incremental mode (`since`): the Rust rebuild is always a full
//!   rebuild (YAGNI, design D3 — the in-memory index is rebuilt from
//!   scratch at startup anyway).
//! - The LLM method is a stub rather than a client call; the oracle's
//!   `linker == nil` error becomes the disabled exclusion above.
//! - Non-boolean rule results are detected at evaluation time: the oracle
//!   type-checks rules against `cel.BoolType` at compile time, but the
//!   `cel` crate's `Program::compile` is parse-only.
//! - The oracle's `metadata(entity, key)` helper is not in the frozen
//!   six-function contract (design D5); rules read `A.metadata_json`
//!   directly.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cel::Value;
use cel::objects::{Key as CelKey, Map as CelMap};
use config::ontology::{CrossDomainLinksConfig, LinkExpression, LinkMethod};
use config::preset::LinkerConfig;
use db::utils::normalize;
use db::{ConnectionOrTx, Db, Entity, EntityDao, EntityLink, EntityLinkDao};

use crate::cel::{CelEngine, register_data_functions, register_graph_functions};
use crate::error::GraphError;

/// The oracle's `config.DefaultEqualsMinWords`: names shorter than this many
/// words are too ambiguous for `equals` linking.
const DEFAULT_EQUALS_MIN_WORDS: i32 = 2;
/// The oracle's `config.DefaultRelationType`.
const DEFAULT_RELATION_TYPE: &str = "same_entity";
/// The oracle's `equalsConfidence`.
const EQUALS_CONFIDENCE: f64 = 0.9;
/// The oracle's `ruleConfidence`.
const RULE_CONFIDENCE: f64 = 1.0;

/// The outcome of one linking run (the oracle's `BuildEntityLinksResult`).
#[derive(Debug, Default, PartialEq)]
pub struct LinkResult {
    /// Candidate pairs for which at least one new link row was inserted (a
    /// bidirectional pair counts once).
    pub links_created: usize,
    /// Candidate pairs that produced no new rows: already linked (idempotent
    /// re-run), or skipped by the `llm` stub.
    pub links_skipped: usize,
    /// Informational notes (e.g. the `llm` stub's skip record). The crate
    /// has no logging dependency; the CLI layer can surface these.
    pub notes: Vec<String>,
    /// Non-fatal failures (a rule that failed to compile, a pair whose link
    /// insert errored). The run itself succeeds.
    pub errors: Vec<String>,
}

/// A candidate pair: two entities of the same type with equal normalized
/// names in different (normalized) domains (the oracle's `entityPair`).
#[derive(Debug, Clone)]
struct CandidatePair {
    /// The entity from the lexicographically smaller normalized domain.
    a: Entity,
    /// The entity from the lexicographically larger normalized domain.
    b: Entity,
}

/// Enumerate the candidate pairs (the oracle's `crossDomainEntityPairs`):
/// group by (type, normalized name), then within each group cross-product
/// the entities of different normalized domains.
///
/// Deterministic: groups, domains and intra-domain members are all sorted
/// (by id), so the pair order is stable across runs and machines.
#[must_use]
fn cross_domain_pairs(entities: &[Entity]) -> Vec<CandidatePair> {
    // (type, normalized name) → normalized domain → members.
    let mut groups: BTreeMap<(String, String), BTreeMap<String, Vec<Entity>>> = BTreeMap::new();
    for entity in entities {
        let key = (entity.entity_type.clone(), normalize(&entity.name));
        groups
            .entry(key)
            .or_default()
            .entry(normalize(&entity.domain))
            .or_default()
            .push(entity.clone());
    }

    let mut pairs = Vec::new();
    for domains in groups.values_mut() {
        if domains.len() < 2 {
            continue; // one domain only: no cross-domain candidates
        }
        for members in domains.values_mut() {
            members.sort_by_key(|entity| entity.id);
        }
        // `BTreeMap::values` yields the domain groups in sorted (key) order,
        // which fixes the pair order.
        let groups: Vec<&Vec<Entity>> = domains.values().collect();
        for (i, group_a) in groups.iter().enumerate() {
            for group_b in groups.iter().skip(i + 1) {
                for a in *group_a {
                    for b in *group_b {
                        pairs.push(CandidatePair {
                            a: a.clone(),
                            b: b.clone(),
                        });
                    }
                }
            }
        }
    }
    pairs
}

/// Insert the A→B and B→A rows (the oracle's `createBidirectionalLink`);
/// `true` when at least one row was newly inserted. A self-link cannot arise
/// from the pipeline (the pair's normalized domains differ) and is rejected
/// by the DAO regardless.
fn create_bidirectional_link(
    db: &Db,
    a_id: i64,
    b_id: i64,
    relation_type: &str,
    method: &str,
    confidence: f64,
    evidence: &str,
) -> Result<bool, GraphError> {
    let created = db.with_conn(|conn| -> Result<bool, db::DbError> {
        let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
        let mut created = false;
        for (subject, target) in [(a_id, b_id), (b_id, a_id)] {
            let link = EntityLink {
                subject_entity_id: subject,
                target_entity_id: target,
                relation_type: relation_type.to_owned(),
                method: method.to_owned(),
                confidence,
                evidence: Some(evidence.to_owned()),
            };
            created |= links.create(&link)?;
        }
        Ok(created)
    })??;
    Ok(created)
}

/// The `equals` method (the oracle's `buildEqualsLinks`).
fn run_equals(
    db: &Db,
    config: &CrossDomainLinksConfig,
    pairs: &[CandidatePair],
    result: &mut LinkResult,
) {
    let min_words = config
        .equals
        .map(|equals| equals.min_words)
        .filter(|&words| words > 0)
        .unwrap_or(DEFAULT_EQUALS_MIN_WORDS);

    for pair in pairs {
        let name = normalize(&pair.a.name);
        if name.split_whitespace().count() < min_words as usize {
            continue; // not enough words: oracle parity, counts nothing
        }
        let evidence = format!(
            "equals: {name} in {} and {}",
            normalize(&pair.a.domain),
            normalize(&pair.b.domain)
        );
        match create_bidirectional_link(
            db,
            pair.a.id,
            pair.b.id,
            DEFAULT_RELATION_TYPE,
            "equals",
            EQUALS_CONFIDENCE,
            &evidence,
        ) {
            Ok(true) => result.links_created += 1,
            Ok(false) => result.links_skipped += 1,
            Err(err) => {
                result.errors.push(format!(
                    "create equals link ({} <-> {}): {err}",
                    pair.a.id, pair.b.id
                ));
            }
        }
    }
}

/// One compiled ontology rule (the oracle's `CompiledExpr` keyed by name).
struct CompiledRule {
    /// The rule's `<name>`.
    name: String,
    /// The rule's `<relation-type>`: the link's relation.
    relation_type: String,
    /// The compiled CEL program of the rule's `<where>`.
    program: Arc<cel::Program>,
}

/// CEL rule evaluation for the `expression` method (the oracle's
/// `ExpressionLinker`): all six contract functions (tasks 1.7/1.8) are
/// registered on the engine, so rules can use `facts`, `has_fact`, `chunks`,
/// `chunk_contains`, `neighbors` and `path_exists` in addition to the `A`/`B`
/// entity fields.
struct ExpressionLinker {
    /// The engine with the contract functions installed.
    engine: CelEngine,
    /// The compiled rules in evaluation order: priority descending, ties in
    /// ontology order.
    rules: Vec<CompiledRule>,
}

impl ExpressionLinker {
    /// Compile the rules (the oracle's `Init`): a parse error in any rule
    /// fails the whole method (oracle parity).
    fn new(db: &Db, expressions: &[LinkExpression]) -> Result<Self, GraphError> {
        let mut engine = CelEngine::new();
        let db = Arc::new(db.clone());
        register_data_functions(&mut engine, Arc::clone(&db));
        register_graph_functions(&mut engine, db);

        // Priority order: higher first. `sort_by_key` is STABLE, so ties keep
        // the ontology's order — the oracle's unstable `sort.Slice` made tie
        // order nondeterministic (a Go bug, fixed here).
        let mut ordered: Vec<&LinkExpression> = expressions.iter().collect();
        ordered.sort_by_key(|expression| std::cmp::Reverse(expression.priority));

        let mut rules = Vec::with_capacity(ordered.len());
        for expression in ordered {
            let program = engine.compile(&expression.where_)?;
            rules.push(CompiledRule {
                name: expression.name.clone(),
                relation_type: expression.relation_type.clone(),
                program,
            });
        }
        Ok(Self { engine, rules })
    }

    /// Evaluate all rules against one pair in priority order; the first rule
    /// that evaluates to `true` wins (the oracle's `EvaluatePair`).
    fn evaluate_pair(&self, a: &Entity, b: &Entity) -> Result<Option<&CompiledRule>, GraphError> {
        let bindings = [("A", entity_to_value(a)), ("B", entity_to_value(b))];
        for rule in &self.rules {
            let value = self.engine.evaluate(&rule.program, &bindings)?;
            match value {
                Value::Bool(true) => return Ok(Some(rule)),
                Value::Bool(false) => continue,
                // The oracle type-checks rules against `cel.BoolType` at
                // compile time; the `cel` crate's compile is parse-only, so
                // the check happens here.
                _ => {
                    return Err(GraphError::NonBooleanRule {
                        name: rule.name.clone(),
                    });
                }
            }
        }
        Ok(None)
    }
}

/// The entity's CEL map (the oracle's `entityToMap`): the `A`/`B` bindings.
///
/// Keys: `id` (int), `type` (string), `name` (string), `domain` (string, raw
/// — not normalized), `confidence` (double or null), `description` (string
/// or null), `metadata_json` (string or null), `created_at` (string).
fn entity_to_value(entity: &Entity) -> Value {
    let key = |name: &str| CelKey::String(Arc::new(name.to_string()));
    let mut map = HashMap::with_capacity(8);
    map.insert(key("id"), Value::Int(entity.id));
    map.insert(
        key("type"),
        Value::String(Arc::new(entity.entity_type.clone())),
    );
    map.insert(key("name"), Value::String(Arc::new(entity.name.clone())));
    map.insert(
        key("domain"),
        Value::String(Arc::new(entity.domain.clone())),
    );
    map.insert(
        key("confidence"),
        entity.confidence.map(Value::Float).unwrap_or(Value::Null),
    );
    map.insert(
        key("description"),
        entity
            .description
            .as_ref()
            .map(|description| Value::String(Arc::new(description.clone())))
            .unwrap_or(Value::Null),
    );
    map.insert(
        key("metadata_json"),
        entity
            .metadata_json
            .as_ref()
            .map(|metadata| Value::String(Arc::new(metadata.clone())))
            .unwrap_or(Value::Null),
    );
    map.insert(
        key("created_at"),
        Value::String(Arc::new(entity.created_at.clone())),
    );
    Value::Map(CelMap { map: Arc::new(map) })
}

/// The `expression` method (the oracle's `buildExpressionLinks`).
fn run_expression(
    db: &Db,
    config: &CrossDomainLinksConfig,
    pairs: &[CandidatePair],
    result: &mut LinkResult,
) {
    if config.expressions.is_empty() || pairs.is_empty() {
        return;
    }
    let linker = match ExpressionLinker::new(db, &config.expressions) {
        Ok(linker) => linker,
        Err(err) => {
            result.errors.push(format!("init expression linker: {err}"));
            return;
        }
    };

    for pair in pairs {
        match linker.evaluate_pair(&pair.a, &pair.b) {
            Ok(Some(rule)) => {
                let evidence = format!("expression: {}", rule.name);
                match create_bidirectional_link(
                    db,
                    pair.a.id,
                    pair.b.id,
                    &rule.relation_type,
                    "expression",
                    RULE_CONFIDENCE,
                    &evidence,
                ) {
                    Ok(true) => result.links_created += 1,
                    Ok(false) => result.links_skipped += 1,
                    Err(err) => {
                        result.errors.push(format!(
                            "create expression link ({} <-> {}): {err}",
                            pair.a.id, pair.b.id
                        ));
                    }
                }
            }
            // No rule matched: oracle parity — the pair counts neither as
            // created nor as skipped.
            Ok(None) => {}
            Err(err) => {
                result.errors.push(format!(
                    "expression pair ({} <-> {}): {err}",
                    pair.a.id, pair.b.id
                ));
                result.links_skipped += 1;
            }
        }
    }
}

/// The `llm` method STUB (design D6, human decision 2026-08-21): no LLM
/// client in this build — the stub records its skip in [`LinkResult::notes`]
/// and writes nothing. The real implementation lands with the future LLM
/// client change.
fn run_llm_stub(pairs: &[CandidatePair], result: &mut LinkResult) {
    result.links_skipped += pairs.len();
    result.notes.push(format!(
        "llm: stub — no LLM client in this build, {} candidate pair(s) skipped",
        pairs.len()
    ));
}

/// Run the cross-domain linking pipeline (the oracle's `BuildEntityLinks`):
/// the methods in the ontology's configured order, each idempotent.
///
/// `linker_config.disabled` (the preset's `LinkerConfig`, the oracle's
/// `Linker.Disabled` check in the ingestion runner) excludes the `llm`
/// method.
pub fn build_entity_links(
    db: &Db,
    links_config: &CrossDomainLinksConfig,
    linker_config: &LinkerConfig,
) -> Result<LinkResult, GraphError> {
    let entities =
        db.with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).list())??;
    let pairs = cross_domain_pairs(&entities);

    let mut result = LinkResult::default();
    for method in &links_config.methods {
        match method {
            LinkMethod::Equals => run_equals(db, links_config, &pairs, &mut result),
            LinkMethod::Expression => run_expression(db, links_config, &pairs, &mut result),
            LinkMethod::Llm => {
                if linker_config.disabled {
                    result
                        .notes
                        .push("llm: excluded by linker.disabled".to_owned());
                } else {
                    run_llm_stub(&pairs, &mut result);
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (the fixtures are
    // compile-time constants).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use config::ontology::EqualsConfig;

    /// A test entity with the given (id, type, name, domain).
    fn entity(id: i64, entity_type: &str, name: &str, domain: &str) -> Entity {
        Entity {
            id,
            entity_type: entity_type.to_owned(),
            name: name.to_owned(),
            domain: domain.to_owned(),
            description: None,
            confidence: None,
            metadata_json: None,
            created_at: "2026-01-01 00:00:00".to_owned(),
        }
    }

    /// A links config with the given methods and (optionally) expressions.
    fn links_config(
        methods: Vec<LinkMethod>,
        expressions: Vec<LinkExpression>,
    ) -> CrossDomainLinksConfig {
        CrossDomainLinksConfig {
            methods,
            equals: None,
            llm_confidence_threshold: 0.7,
            batch_size: 5,
            expressions,
        }
    }

    /// Insert entities (type, name, domain) and return their ids in order.
    fn insert_entities(db: &Db, rows: &[(&str, &str, &str)]) -> Vec<i64> {
        db.with_conn(|conn| {
            let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
            rows.iter()
                .map(|(entity_type, name, domain)| {
                    entities.create(entity_type, name, domain, None, None, None)
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .unwrap()
    }

    /// The entity_links rows, in insertion (rowid) order.
    fn all_links(db: &Db) -> Vec<EntityLink> {
        db.with_conn(|conn| EntityLinkDao::new(ConnectionOrTx::Connection(conn)).list_all())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn cross_domain_pairs_enumeration() {
        let entities = vec![
            entity(1, "PERSON", "Alice Smith", "hr"),
            entity(2, "PERSON", "Alice Smith", "it"),
            entity(3, "PERSON", "Bob Jones", "hr"),
            entity(4, "PERSON", "Bob Jones", "it"),
            // Different type: never paired with 1.
            entity(5, "ORGANIZATION", "Alice Smith", "hr"),
            // Different name: never paired with 1.
            entity(6, "PERSON", "Alice Jones", "it"),
            // Same normalized domain as 1: no pair with 1, but still pairs
            // with 2 (a different domain).
            entity(7, "PERSON", "Alice Smith", " HR "),
        ];
        let pairs = cross_domain_pairs(&entities);
        assert_eq!(pairs.len(), 3);
        assert_eq!((pairs[0].a.id, pairs[0].b.id), (1, 2));
        assert_eq!((pairs[1].a.id, pairs[1].b.id), (7, 2));
        assert_eq!((pairs[2].a.id, pairs[2].b.id), (3, 4));
    }

    #[test]
    fn cross_domain_pairs_is_deterministic() {
        // Three domains, unsorted input: pairs must follow the sorted
        // (domain, id) order — alpha/beta, alpha/zeta, beta/zeta.
        let entities = vec![
            entity(6, "PERSON", "A B", "zeta"),
            entity(5, "PERSON", "A B", "alpha"),
            entity(4, "PERSON", "A B", "beta"),
        ];
        let pairs = cross_domain_pairs(&entities);
        assert_eq!(pairs.len(), 3);
        assert_eq!((pairs[0].a.id, pairs[0].b.id), (5, 4));
        assert_eq!((pairs[1].a.id, pairs[1].b.id), (5, 6));
        assert_eq!((pairs[2].a.id, pairs[2].b.id), (4, 6));
    }

    #[test]
    fn equals_links_matching_names_and_skips_short_or_different_names() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
                // One word < default min_words (2): pair exists, equals skips.
                ("PERSON", "Alice", "hr"),
                ("PERSON", "Alice", "it"),
                // Different name: no pair at all.
                ("PERSON", "Alice Jones", "it"),
            ],
        );

        let result = build_entity_links(
            &db,
            &links_config(vec![LinkMethod::Equals], Vec::new()),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(result.links_created, 1);
        assert_eq!(result.links_skipped, 0);
        assert!(result.errors.is_empty());

        let links = all_links(&db);
        assert_eq!(links.len(), 2, "one bidirectional pair");
        for link in &links {
            assert_eq!(link.method, "equals");
            assert_eq!(link.relation_type, "same_entity");
            assert_eq!(link.confidence, EQUALS_CONFIDENCE);
            assert_eq!(
                link.evidence.as_deref(),
                Some("equals: alice smith in hr and it")
            );
        }
    }

    #[test]
    fn equals_respects_configured_min_words() {
        let db = db::test_util::in_memory_db();
        insert_entities(&db, &[("PERSON", "Alice", "hr"), ("PERSON", "Alice", "it")]);

        let config = CrossDomainLinksConfig {
            equals: Some(EqualsConfig { min_words: 1 }),
            ..links_config(vec![LinkMethod::Equals], Vec::new())
        };
        let result = build_entity_links(&db, &config, &LinkerConfig::default()).unwrap();
        assert_eq!(result.links_created, 1);
        assert_eq!(all_links(&db).len(), 2);
    }

    #[test]
    fn expression_rule_creates_link_with_rule_attributes() {
        let db = db::test_util::in_memory_db();
        let ids = insert_entities(
            &db,
            &[
                ("PERSON", "John Doe", "hr"),
                ("PERSON", "John Doe", "it"),
                ("ORGANIZATION", "Acme", "hr"),
                // Rule-false pair: no facts.
                ("PERSON", "Jane Roe", "hr"),
                ("PERSON", "Jane Roe", "it"),
            ],
        );
        db.with_conn(|conn| {
            let facts = db::FactDao::new(ConnectionOrTx::Connection(conn));
            facts.create(
                Some(ids[0]),
                "works_at",
                Some(ids[2]),
                "hr",
                None,
                None,
                None,
            )
        })
        .unwrap()
        .unwrap();

        let rule = LinkExpression {
            name: "acme_workers".to_owned(),
            description: String::new(),
            priority: 5,
            where_: "has_fact(A.id, 'works_at', 'Acme')".to_owned(),
            relation_type: "works_at_same_org".to_owned(),
        };
        let result = build_entity_links(
            &db,
            &links_config(vec![LinkMethod::Expression], vec![rule]),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(result.links_created, 1);
        // The rule-false pair counts neither as created nor as skipped.
        assert_eq!(result.links_skipped, 0);
        assert!(result.errors.is_empty());

        let links = all_links(&db);
        assert_eq!(links.len(), 2, "one bidirectional pair");
        for link in &links {
            assert_eq!(link.method, "expression");
            assert_eq!(link.relation_type, "works_at_same_org");
            assert_eq!(link.confidence, RULE_CONFIDENCE);
            assert_eq!(link.evidence.as_deref(), Some("expression: acme_workers"));
            assert!(
                (link.subject_entity_id, link.target_entity_id) == (ids[0], ids[1])
                    || (link.subject_entity_id, link.target_entity_id) == (ids[1], ids[0])
            );
        }
    }

    #[test]
    fn expression_priority_order_and_first_true_wins() {
        // Ontology order: low (priority 1) FIRST, high (priority 10) second.
        let rules = vec![
            LinkExpression {
                name: "low".to_owned(),
                description: String::new(),
                priority: 1,
                where_: "A.domain == 'hr' || A.domain == 'it'".to_owned(),
                relation_type: "rel_low".to_owned(),
            },
            LinkExpression {
                name: "high".to_owned(),
                description: String::new(),
                priority: 10,
                where_: "A.domain == 'hr'".to_owned(),
                relation_type: "rel_high".to_owned(),
            },
        ];

        // Pair 1: A from "hr" — BOTH rules true; the higher priority must win.
        let db1 = db::test_util::in_memory_db();
        insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
        let r1 = build_entity_links(
            &db1,
            &links_config(vec![LinkMethod::Expression], rules.clone()),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(r1.links_created, 1);
        assert!(
            all_links(&db1)
                .iter()
                .all(|link| link.relation_type == "rel_high")
        );

        // Pair 2: A from "it" — high false, low true: falls back in priority
        // order.
        let db2 = db::test_util::in_memory_db();
        insert_entities(&db2, &[("PERSON", "U V", "it"), ("PERSON", "U V", "zz")]);
        let r2 = build_entity_links(
            &db2,
            &links_config(vec![LinkMethod::Expression], rules),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(r2.links_created, 1);
        assert!(
            all_links(&db2)
                .iter()
                .all(|link| link.relation_type == "rel_low")
        );
    }

    #[test]
    fn expression_errors_are_recorded_not_fatal() {
        // A parse error fails the whole method (oracle parity).
        let db1 = db::test_util::in_memory_db();
        insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
        let bad = LinkExpression {
            name: "broken".to_owned(),
            description: String::new(),
            priority: 0,
            where_: "A.name +".to_owned(),
            relation_type: "same_entity".to_owned(),
        };
        let r1 = build_entity_links(
            &db1,
            &links_config(vec![LinkMethod::Expression], vec![bad]),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(r1.links_created, 0);
        assert!(
            r1.errors
                .iter()
                .any(|err| err.starts_with("init expression linker:"))
        );
        assert!(all_links(&db1).is_empty());

        // A non-boolean result is a per-pair error (the `cel` crate cannot
        // type-check at compile time).
        let db2 = db::test_util::in_memory_db();
        insert_entities(&db2, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
        let non_bool = LinkExpression {
            name: "string".to_owned(),
            description: String::new(),
            priority: 0,
            where_: "A.name".to_owned(),
            relation_type: "same_entity".to_owned(),
        };
        let r2 = build_entity_links(
            &db2,
            &links_config(vec![LinkMethod::Expression], vec![non_bool]),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(r2.links_created, 0);
        assert_eq!(r2.links_skipped, 1);
        assert!(
            r2.errors
                .iter()
                .any(|err| err.starts_with("expression pair ("))
        );
        assert!(all_links(&db2).is_empty());
    }

    #[test]
    fn llm_stub_writes_nothing_and_disabled_excludes_the_method() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
                ("PERSON", "Bob Jones", "hr"),
                ("PERSON", "Bob Jones", "it"),
            ],
        );
        let config = links_config(vec![LinkMethod::Llm], Vec::new());

        // Stub active (disabled = false): skips are counted, nothing written,
        // a note records the skip.
        let active = build_entity_links(&db, &config, &LinkerConfig::default()).unwrap();
        assert_eq!(active.links_created, 0);
        assert_eq!(active.links_skipped, 2);
        assert!(
            active
                .notes
                .iter()
                .any(|note| note.starts_with("llm: stub"))
        );
        assert!(all_links(&db).is_empty());

        // Disabled: the method is excluded entirely — no skip count.
        let disabled = LinkerConfig {
            disabled: true,
            ..LinkerConfig::default()
        };
        let excluded = build_entity_links(&db, &config, &disabled).unwrap();
        assert_eq!(excluded.links_created, 0);
        assert_eq!(excluded.links_skipped, 0);
        assert!(
            excluded
                .notes
                .iter()
                .any(|note| note.contains("excluded by linker.disabled"))
        );
        assert!(all_links(&db).is_empty());
    }

    #[test]
    fn method_order_from_config_is_respected() {
        // The rule's relation type collides with equals' default: whichever
        // method runs first owns the row, the second one is skipped.
        let rule = LinkExpression {
            name: "dup".to_owned(),
            description: String::new(),
            priority: 0,
            where_: "A.domain == 'hr'".to_owned(),
            relation_type: DEFAULT_RELATION_TYPE.to_owned(),
        };

        let db1 = db::test_util::in_memory_db();
        insert_entities(&db1, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
        let first = build_entity_links(
            &db1,
            &links_config(
                vec![LinkMethod::Equals, LinkMethod::Expression],
                vec![rule.clone()],
            ),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(first.links_created, 1);
        assert_eq!(first.links_skipped, 1);
        assert!(all_links(&db1).iter().all(|link| link.method == "equals"));

        let db2 = db::test_util::in_memory_db();
        insert_entities(&db2, &[("PERSON", "X Y", "hr"), ("PERSON", "X Y", "it")]);
        let second = build_entity_links(
            &db2,
            &links_config(vec![LinkMethod::Expression, LinkMethod::Equals], vec![rule]),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(second.links_created, 1);
        assert_eq!(second.links_skipped, 1);
        assert!(
            all_links(&db2)
                .iter()
                .all(|link| link.method == "expression")
        );
    }

    #[test]
    fn self_link_never_created() {
        // Same normalized domain (case/whitespace variants): no pair at all.
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "HR"),
                ("PERSON", "Alice Smith", " hr "),
                // Same domain, different type: still no pair.
                ("ORGANIZATION", "Alice Smith", "hr"),
            ],
        );
        let result = build_entity_links(
            &db,
            &links_config(vec![LinkMethod::Equals], Vec::new()),
            &LinkerConfig::default(),
        )
        .unwrap();
        assert_eq!(result.links_created, 0);
        assert_eq!(result.links_skipped, 0);
        assert!(all_links(&db).is_empty());

        // The DAO rejects an explicit self-link as well (schema CHECK +
        // DAO pre-check).
        let id = db
            .with_conn(|conn| {
                let entities = EntityDao::new(ConnectionOrTx::Connection(conn));
                entities.create("PERSON", "Solo", "hr", None, None, None)
            })
            .unwrap()
            .unwrap();
        let created = db
            .with_conn(|conn| {
                let links = EntityLinkDao::new(ConnectionOrTx::Connection(conn));
                links.create(&EntityLink {
                    subject_entity_id: id,
                    target_entity_id: id,
                    relation_type: "same_entity".to_owned(),
                    method: "equals".to_owned(),
                    confidence: 0.9,
                    evidence: None,
                })
            })
            .unwrap()
            .unwrap();
        assert!(!created);
    }
}
