//! Cross-domain entity linking pipeline (design D6, task 1.9).
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
//!   `min_words` words (default [`DEFAULT_EQUALS_MIN_WORDS`]); shorter names
//!   are skipped silently (they count neither as created nor as skipped).
//!   Creates a `same_entity` link with confidence [`EQUALS_CONFIDENCE`].
//! - `expression` — ontology CEL rules evaluated against the pair bindings
//!   `A`/`B` (the entity map), with all six contract functions (tasks
//!   1.7/1.8) registered. Rules are applied in priority order (higher
//!   first; ties keep the ontology's order — `sort_by_key` is stable), the
//!   first rule evaluating to `true` wins, and its `relation-type` becomes
//!   the link's relation with confidence [`RULE_CONFIDENCE`].
//! - `llm` — one chat completion per pair (llm change, design D6): up to
//!   three context chunk texts per entity (truncated: description 200 chars,
//!   chunk 500 chars) are rendered into the user
//!   prompt; the system prompt carries no data and is rendered once per run.
//!   The response is parsed strictly to `{same_entity, confidence,
//!   reasoning}` with the confidence clamped to [0, 1], and a `same_entity`
//!   link (method `llm`, evidence = the reasoning) is created only when the
//!   decision is `same_entity` with confidence ≥
//!   `CrossDomainLinksConfig::llm_confidence_threshold`. Decisions are
//!   cached in `llm_linker_cache` (the CACHE database, task 1.10) under the
//!   LLM request signature — `sha256(model:temperature:max_tokens:
//!   rendered_system_prompt:rendered_user_prompt)` — no entity IDs, no
//!   dataset: the cache is checked BEFORE the call and written AFTER the
//!   decision, including below-threshold ones. Without a cache database the
//!   method runs uncached. A pair whose
//!   context load, call, or parse fails is a non-fatal [`LinkResult::errors`]
//!   entry; the run itself succeeds.
//!   `LinkerConfig::disabled` excludes the method entirely (the ingestion
//!   runner checks the same flag).
//!
//! # Design decisions
//!
//! - No incremental mode (`since`): the rebuild is always a full rebuild
//!   (YAGNI, design D3 — the in-memory index is rebuilt from scratch at
//!   startup anyway).
//! - The LLM decision cache is keyed by the LLM **request signature**
//!   (`model:temperature:max_tokens:rendered_system_prompt:
//!   rendered_user_prompt`, task 1.10): global-safe — no entity IDs, no
//!   dataset — so a rebuilt (renumbered) knowledge database still hits the
//!   shared cache database. The rendered prompts subsume both the entity
//!   data and the template content, so no separate entity key is needed.
//! - Non-boolean rule results are detected at evaluation time: the `cel`
//!   crate's `Program::compile` is parse-only, so the boolean check happens
//!   at evaluation.
//! - A `metadata(entity, key)` helper is not in the frozen six-function
//!   contract (design D5); rules read `A.metadata_json` directly.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cel::Value;
use cel::objects::{Key as CelKey, Map as CelMap};
use config::ontology::{CrossDomainLinksConfig, LinkExpression, LinkMethod};
use config::preset::{LinkerConfig, LlmConfig};
use db::utils::normalize;
use db::{
    ChunkEntityDao, ConnectionOrTx, Db, DbExecutor, Entity, EntityDao, EntityLink, EntityLinkDao,
};
use llm::LlmClient;
use serde::{Deserialize, Serialize};

use crate::cel::{CelEngine, register_data_functions, register_graph_functions};
use crate::error::GraphError;
use crate::prompts::{
    EntityData, EntityLinkerPrompts, LinkerInput, load_entity_linker_prompts, sha256_hex, truncate,
};

/// Default minimum word count for `equals` linking: names shorter than this
/// many words are too ambiguous.
const DEFAULT_EQUALS_MIN_WORDS: i32 = 2;
/// The default link relation type.
const DEFAULT_RELATION_TYPE: &str = "same_entity";
/// Confidence of `equals` links.
const EQUALS_CONFIDENCE: f64 = 0.9;
/// Confidence of `expression` links.
const RULE_CONFIDENCE: f64 = 1.0;
/// Max context chunk texts per entity in the LLM prompt.
const LLM_CONTEXT_LIMIT: i64 = 3;
/// Max description length in the LLM prompt.
const LLM_DESCRIPTION_LEN: i64 = 200;
/// Max length of one context chunk in the LLM prompt.
const LLM_CHUNK_LEN: i64 = 500;
/// The static `json_schema` payload for the LLM decision (design D6). Sent
/// on every call in `json_schema` response-format mode; the client ignores
/// it in `json_object` mode.
const LINK_DECISION_SCHEMA: &str = r#"{
  "title": "EntityComparison",
  "type": "object",
  "properties": {
    "same_entity": {
      "type": "boolean",
      "description": "Indicates whether entities are the same"
    },
    "confidence": {
      "type": "number",
      "minimum": 0.0,
      "maximum": 1.0,
      "description": "The level of confidence in the answer is from 0.0 to 1.0"
    },
    "reasoning": {
      "type": "string",
      "description": "Explanation or logic for decision making"
    }
  },
  "required": ["same_entity", "confidence"],
  "additionalProperties": false
}"#;

/// The outcome of one linking run.
#[derive(Debug, Default, PartialEq)]
pub struct LinkResult {
    /// Candidate pairs for which at least one new link row was inserted (a
    /// bidirectional pair counts once).
    pub links_created: usize,
    /// Candidate pairs that produced no new rows: already linked (idempotent
    /// re-run), or decided not linkable by the `llm` method (`same_entity`
    /// false or confidence below the threshold).
    pub links_skipped: usize,
    /// Informational notes (e.g. the `llm` stub's skip record). The crate
    /// has no logging dependency; the CLI layer can surface these.
    pub notes: Vec<String>,
    /// Non-fatal failures (a rule that failed to compile, a pair whose link
    /// insert errored). The run itself succeeds.
    pub errors: Vec<String>,
}

/// A candidate pair: two entities of the same type with equal normalized
/// names in different (normalized) domains.
#[derive(Debug, Clone)]
struct CandidatePair {
    /// The entity from the lexicographically smaller normalized domain.
    a: Entity,
    /// The entity from the lexicographically larger normalized domain.
    b: Entity,
}

/// Enumerate the candidate pairs:
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

/// Insert the A→B and B→A rows;
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

/// The `equals` method.
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
            continue; // not enough words: counts nothing
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

/// One compiled ontology rule (keyed by name).
struct CompiledRule {
    /// The rule's `<name>`.
    name: String,
    /// The rule's `<relation-type>`: the link's relation.
    relation_type: String,
    /// The compiled CEL program of the rule's `<where>`.
    program: Arc<cel::Program>,
}

/// CEL rule evaluation for the `expression` method: all six contract
/// functions (tasks 1.7/1.8) are registered on the engine, so rules can use
/// `facts`, `has_fact`, `chunks`, `chunk_contains`, `neighbors` and
/// `path_exists` in addition to the `A`/`B` entity fields.
struct ExpressionLinker {
    /// The engine with the contract functions installed.
    engine: CelEngine,
    /// The compiled rules in evaluation order: priority descending, ties in
    /// ontology order.
    rules: Vec<CompiledRule>,
}

impl ExpressionLinker {
    /// Compile the rules: a parse error in any rule fails the whole method.
    fn new(db: &Db, expressions: &[LinkExpression]) -> Result<Self, GraphError> {
        let mut engine = CelEngine::new();
        let db = Arc::new(db.clone());
        register_data_functions(&mut engine, Arc::clone(&db));
        register_graph_functions(&mut engine, db);

        // Priority order: higher first. `sort_by_key` is STABLE, so ties
        // keep the ontology's order (deterministic across runs).
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
    /// that evaluates to `true` wins.
    fn evaluate_pair(&self, a: &Entity, b: &Entity) -> Result<Option<&CompiledRule>, GraphError> {
        let bindings = [("A", entity_to_value(a)), ("B", entity_to_value(b))];
        for rule in &self.rules {
            let value = self.engine.evaluate(&rule.program, &bindings)?;
            match value {
                Value::Bool(true) => return Ok(Some(rule)),
                Value::Bool(false) => continue,
                // Rules must evaluate to a boolean; the `cel` crate's
                // compile is parse-only, so the check happens here.
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

/// The entity's CEL map: the `A`/`B` bindings.
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

/// The `expression` method.
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
            // No rule matched: the pair counts neither as created nor as
            // skipped.
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

/// The structured LLM decision (design D6).
///
/// Also the wire format of the `llm_linker_cache` value (task 1.10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkDecision {
    /// Whether the entities refer to the same real-world entity.
    pub same_entity: bool,
    /// Confidence in [0, 1] (clamped at parse time).
    pub confidence: f64,
    /// The model's explanation; the link's `evidence`. Optional on the wire
    /// (the schema requires only `same_entity`/`confidence`); absent → empty.
    #[serde(default)]
    pub reasoning: String,
}

/// Strictly parse the model's JSON decision and clamp the confidence to
/// [0, 1] (design D6). A non-JSON response or a wrong shape is a pair error
/// (non-fatal), never a panic.
fn parse_link_decision(raw: &str) -> Result<LinkDecision, String> {
    let mut decision: LinkDecision =
        serde_json::from_str(raw.trim()).map_err(|err| format!("invalid decision JSON: {err}"))?;
    decision.confidence = decision.confidence.clamp(0.0, 1.0);
    Ok(decision)
}

/// The `llm_linker_cache` table (task 1.10): the canonical home is the cache
/// database schema (`migrations/cache/1-init/up.sql`, created by
/// `Db::open_cache`); the `IF NOT EXISTS` guard keeps the DAO usable on
/// databases created before the table was added to the migration.
const LINKER_CACHE_TABLE: &str = "llm_linker_cache";

/// Persistent LLM-linker decision cache over the `llm_linker_cache` table
/// (task 1.10, storage-layout-restructure).
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction via [`ConnectionOrTx`] — the same shape as the db
/// crate's DAOs (cf. `db::AppKv`) and
/// `ingestion::ner::llm_cache::LlmNerCache`. Entries map the LLM request
/// signature key (see [`llm_cache_key`]) to a serialized [`LinkDecision`].
pub struct LlmLinkerCache<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> LlmLinkerCache<'conn> {
    /// Bind the cache to a shared connection or an in-flight transaction.
    #[must_use]
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Return the cached decision for `key`, or `None` on a miss.
    ///
    /// A miss is: no row, or a row whose JSON payload does not deserialize
    /// into [`LinkDecision`] (corrupted entry — the next [`set`](Self::set)
    /// overwrites it). Database failures are NOT misses:
    /// they propagate as [`GraphError::Db`].
    pub fn get(&self, key: &str) -> Result<Option<LinkDecision>, GraphError> {
        self.ensure_table()?;
        let rows: Vec<String> = self.exec.query(
            &format!("SELECT decision FROM {LINKER_CACHE_TABLE} WHERE cache_key = ?"),
            [key],
            |row| row.get(0),
        )?;
        // The primary key guarantees at most one row: an empty result set is
        // a plain miss.
        let Some(json) = rows.into_iter().next() else {
            return Ok(None);
        };
        match serde_json::from_str(&json) {
            Ok(decision) => Ok(Some(decision)),
            Err(_) => Ok(None),
        }
    }

    /// Store `decision` under `key`, replacing any existing entry (`INSERT
    /// OR REPLACE` semantics).
    pub fn set(&self, key: &str, decision: &LinkDecision) -> Result<(), GraphError> {
        let json = serde_json::to_string(decision)
            .map_err(|source| GraphError::DecisionJson { source })?;
        self.ensure_table()?;
        self.exec.execute(
            &format!(
                "INSERT OR REPLACE INTO {LINKER_CACHE_TABLE} (cache_key, decision) VALUES (?, ?)"
            ),
            (key, json),
        )?;
        Ok(())
    }

    /// Create the decision-cache table if it does not exist yet (safety net:
    /// `Db::open_cache` already creates it from the cache migration).
    fn ensure_table(&self) -> Result<(), GraphError> {
        self.exec.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {LINKER_CACHE_TABLE} \
                 (cache_key TEXT PRIMARY KEY, decision TEXT NOT NULL)"
            ),
            [],
        )?;
        Ok(())
    }
}

/// The decision-cache key (task 1.10): the SHA-256 hex of the LLM request
/// signature — `model:temperature:max_tokens:rendered_system_prompt:
/// rendered_user_prompt` — NO entity IDs, NO dataset (global-safe, mirrors
/// the NER cache key). Any change to the model, its parameters, or the
/// rendered prompts invalidates every entry.
fn llm_cache_key(
    model: &str,
    temperature: f64,
    max_tokens: i32,
    system_prompt: &str,
    user_prompt: &str,
) -> String {
    let payload = format!("{model}:{temperature}:{max_tokens}:{system_prompt}:{user_prompt}");
    sha256_hex(payload.as_bytes())
}

/// Read the cached decision for `key` from `cache` (the cache database), or
/// `None` on a miss — including when `cache` is `None` (caching disabled).
fn read_cached_decision(cache: Option<&Db>, key: &str) -> Result<Option<LinkDecision>, String> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    cache
        .with_conn(|conn| LlmLinkerCache::new(ConnectionOrTx::Connection(conn)).get(key))
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// Store the decision under `key` in `cache` (a no-op when `cache` is `None`
/// — caching disabled).
fn write_cached_decision(
    cache: Option<&Db>,
    key: &str,
    decision: &LinkDecision,
) -> Result<(), String> {
    let Some(cache) = cache else {
        return Ok(());
    };
    cache
        .with_conn(|conn| LlmLinkerCache::new(ConnectionOrTx::Connection(conn)).set(key, decision))
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// One entity's prompt data (name/type/domain + truncated description + up to
/// [`LLM_CONTEXT_LIMIT`] truncated chunk texts).
fn load_entity_data(db: &Db, entity: &Entity) -> Result<EntityData, String> {
    let chunks = db
        .with_conn(|conn| {
            let dao = ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            dao.get_chunk_texts_by_entity(entity.id, LLM_CONTEXT_LIMIT)
                .map_err(|err| format!("load context: {err}"))
        })
        .map_err(|err| err.to_string())??;
    let context = chunks
        .into_iter()
        .map(|text| truncate(&text, LLM_CHUNK_LEN))
        .collect();
    let description = truncate(
        entity.description.as_deref().unwrap_or_default(),
        LLM_DESCRIPTION_LEN,
    );
    Ok(EntityData {
        name: entity.name.clone(),
        entity_type: entity.entity_type.clone(),
        domain: entity.domain.clone(),
        description,
        context,
    })
}

/// The per-run `llm` context: the initialized client, the loaded prompt
/// templates, the pre-rendered system prompt (it carries no data, so one
/// render serves the whole run), and the sampling parameters (together with
/// the rendered prompts, the request-signature cache-key inputs, task 1.10).
struct LlmRun {
    client: LlmClient,
    prompts: EntityLinkerPrompts,
    system_prompt: String,
    temperature: f64,
    max_tokens: i32,
}

impl LlmRun {
    /// Initialize the client and load the prompts. The caller records
    /// `init llm linker: {err}` on failure (the `expression` init pattern).
    fn new(config: &LlmConfig, prompts_path: &str) -> Result<Self, String> {
        let client = LlmClient::new(config).map_err(|err| err.to_string())?;
        let prompts = load_entity_linker_prompts(prompts_path).map_err(|err| err.to_string())?;
        let system_prompt = prompts
            .render_system()
            .map_err(|err| format!("render system prompt: {err}"))?;
        Ok(Self {
            client,
            prompts,
            system_prompt,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        })
    }
}

/// Load both entities' prompt data and render the user prompt (task 1.10:
/// the rendered prompt is part of the request-signature cache key, so
/// rendering happens BEFORE the cache check).
fn render_pair_prompts(db: &Db, pair: &CandidatePair, run: &LlmRun) -> Result<String, String> {
    let entity_a = load_entity_data(db, &pair.a)?;
    let entity_b = load_entity_data(db, &pair.b)?;
    run.prompts
        .render_user(&LinkerInput { entity_a, entity_b })
        .map_err(|err| err.to_string())
}

/// The miss path: call the model with the pre-rendered prompts and strictly
/// parse the decision.
fn call_llm(run: &LlmRun, user_prompt: &str) -> Result<LinkDecision, String> {
    let raw = run
        .client
        .call(
            &run.system_prompt,
            user_prompt,
            Some(LINK_DECISION_SCHEMA),
            Some("entity_linker"),
        )
        .map_err(|err| err.to_string())?;
    parse_link_decision(&raw)
}

/// The outcome of one pair under the `llm` method.
enum PairOutcome {
    /// A new link row was inserted for the pair.
    Linked,
    /// No new row: the pair was already linked, or the decision was
    /// `same_entity` false / below the confidence threshold.
    NotLinked,
}

/// One candidate pair under the `llm` method (plus the threshold gate):
/// cache check BEFORE the call, decision, cache write AFTER the decision
/// (including below-threshold ones), then the threshold gate.
///
/// `Err` carries a pair-level failure message (non-fatal for the run); a
/// failed cache write is recorded in `result` and does not fail the pair.
fn process_llm_pair(
    db: &Db,
    cache: Option<&Db>,
    pair: &CandidatePair,
    run: &LlmRun,
    threshold: f64,
    result: &mut LinkResult,
) -> Result<PairOutcome, String> {
    // Render the pair's user prompt FIRST: the cache key is the LLM request
    // signature (task 1.10), so the rendered prompt must exist before the
    // cache check.
    let user_prompt = render_pair_prompts(db, pair, run)?;
    let key = llm_cache_key(
        run.client.model(),
        run.temperature,
        run.max_tokens,
        &run.system_prompt,
        &user_prompt,
    );

    // Cache check BEFORE the call (task 1.10): a hit skips the HTTP round-trip.
    let decision = match read_cached_decision(cache, &key)? {
        Some(decision) => decision,
        None => {
            let decision = call_llm(run, &user_prompt)?;
            // Cache AFTER the decision — including below-threshold ones: a
            // "not the same" verdict is as reusable as a match (no TTL). A
            // failed write is non-fatal (recorded as a pair error).
            if let Err(err) = write_cached_decision(cache, &key, &decision) {
                result.errors.push(format!("cache write: {err}"));
            }
            decision
        }
    };

    // The threshold gate (design D6): both flags must hold.
    if !decision.same_entity || decision.confidence < threshold {
        return Ok(PairOutcome::NotLinked);
    }
    let created = create_bidirectional_link(
        db,
        pair.a.id,
        pair.b.id,
        DEFAULT_RELATION_TYPE,
        "llm",
        decision.confidence,
        &decision.reasoning,
    )
    .map_err(|err| err.to_string())?;
    Ok(if created {
        PairOutcome::Linked
    } else {
        PairOutcome::NotLinked
    })
}

/// The `llm` method: one chat completion per pair, decisions cached in
/// `llm_linker_cache` on the cache database (task 1.10).
///
/// A broken LLM configuration or prompt load fails the whole method
/// (recorded in [`LinkResult::errors`], the `expression` init pattern); a
/// per-pair failure never aborts the run. `cache` is the cache database
/// (`None` runs the method uncached).
fn run_llm(
    db: &Db,
    cache: Option<&Db>,
    links_config: &CrossDomainLinksConfig,
    linker_config: &LinkerConfig,
    prompts_path: &str,
    pairs: &[CandidatePair],
    result: &mut LinkResult,
) {
    if pairs.is_empty() {
        return;
    }
    let run = match LlmRun::new(&linker_config.llm, prompts_path) {
        Ok(run) => run,
        Err(err) => {
            result.errors.push(format!("init llm linker: {err}"));
            return;
        }
    };
    // A loaded override is recorded in the result (the LinkResult.notes
    // pattern; the crate has no logger).
    result.notes.extend(run.prompts.notes().iter().cloned());

    let threshold = links_config.llm_confidence_threshold;
    for pair in pairs {
        match process_llm_pair(db, cache, pair, &run, threshold, result) {
            Ok(PairOutcome::Linked) => result.links_created += 1,
            Ok(PairOutcome::NotLinked) => result.links_skipped += 1,
            Err(msg) => {
                result
                    .errors
                    .push(format!("llm pair ({} <-> {}): {msg}", pair.a.id, pair.b.id));
                result.links_skipped += 1;
            }
        }
    }
}

/// Run the cross-domain linking pipeline: the methods in the ontology's
/// configured order, each idempotent.
///
/// `linker_config.disabled` (the preset's `LinkerConfig`) excludes the `llm`
/// method. `prompts_path` is the preset's `paths.prompts_path` (design D3):
/// the `llm` method loads its prompt templates from
/// `{prompts_path}/entity-linker/` (embedded defaults when the files are
/// absent); it is ignored by every other method. `cache` is the cache
/// database (task 1.10): the `llm` method stores its decisions in its
/// `llm_linker_cache` table; `None` runs the method uncached.
pub fn build_entity_links(
    db: &Db,
    cache: Option<&Db>,
    links_config: &CrossDomainLinksConfig,
    linker_config: &LinkerConfig,
    prompts_path: &str,
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
                    run_llm(
                        db,
                        cache,
                        links_config,
                        linker_config,
                        prompts_path,
                        &pairs,
                        &mut result,
                    );
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

    use std::io::{Read, Write};

    use super::*;
    use config::preset::{LlmConfig, ResponseFormat};

    /// A nonexistent prompts path: the `llm` method falls back to the
    /// embedded templates (the normal case, design D3).
    const TEST_PROMPTS_PATH: &str = "/nonexistent/prompts";

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

    // ── LLM method (task 2.2) ──────────────────────────────────────────────

    /// A `LinkerConfig` pointing the LLM client at `base_url` (no retries:
    /// one attempt per pair, so the mock's request count is deterministic).
    fn llm_linker_config(base_url: &str) -> LinkerConfig {
        LinkerConfig {
            disabled: false,
            llm: LlmConfig {
                api_base_url: base_url.to_owned(),
                api_key: String::new(),
                model_name: "test-model".to_owned(),
                temperature: 0.0,
                max_tokens: 256,
                seed: 1,
                response_format: ResponseFormat::JsonObject,
                timeout_ms: 5000,
                max_retries: 0,
            },
        }
    }

    /// A chat-completions response whose content is the given decision JSON.
    fn decision_body(same_entity: bool, confidence: f64, reasoning: &str) -> Vec<u8> {
        let content = serde_json::json!({
            "same_entity": same_entity,
            "confidence": confidence,
            "reasoning": reasoning,
        })
        .to_string();
        serde_json::json!({
            "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
        })
        .to_string()
        .into_bytes()
    }

    /// A file-backed cache database (the production shape: `Db::open_cache`
    /// on a real file), removed with its sidecars on drop.
    struct TempCacheDb {
        db: db::Db,
        path: std::path::PathBuf,
    }

    impl TempCacheDb {
        fn new(name: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "graph-linker-cache-{name}-{}-{id}",
                std::process::id()
            ));
            let db = db::Db::open_cache(&path).unwrap();
            Self { db, path }
        }
    }

    impl Drop for TempCacheDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    /// The decision-cache entries (key, value) in `llm_linker_cache`.
    fn cached_decisions(cache: &db::Db) -> Vec<(String, String)> {
        cache
            .with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT cache_key, decision FROM llm_linker_cache")
                    .unwrap();
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            })
            .unwrap()
    }

    /// A minimal HTTP/1.1 mock LLM server on 127.0.0.1 (the `crates/llm`
    /// pattern; keeps CI network-free): counts every request, captures the
    /// raw requests, and serves a fixed `(status, body)` with
    /// `Connection: close`.
    struct MockLlm {
        url: String,
        requests: Arc<std::sync::atomic::AtomicUsize>,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockLlm {
        fn start(status: u16, body: Vec<u8>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (thread_requests, thread_captured, thread_shutdown) = (
                Arc::clone(&requests),
                Arc::clone(&captured),
                Arc::clone(&shutdown),
            );
            let thread = std::thread::spawn(move || {
                loop {
                    if thread_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            thread_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            handle_mock_llm(stream, status, &body, &thread_captured);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                requests,
                captured,
                shutdown,
                thread: Some(thread),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// The `user` message (the rendered user prompt) of request `i`.
        fn user_message(&self, i: usize) -> String {
            let raw = self.captured.lock().unwrap()[i].clone();
            let body = raw
                .split_once("\r\n\r\n")
                .map(|(_, body)| body.to_string())
                .unwrap_or_default();
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            value["messages"][1]["content"].as_str().unwrap().to_owned()
        }
    }

    impl Drop for MockLlm {
        fn drop(&mut self) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // The accept loop polls the shutdown flag, so the join returns
            // promptly.
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Serve one connection: read the full request, record it, answer.
    fn handle_mock_llm(
        mut stream: std::net::TcpStream,
        status: u16,
        body: &[u8],
        captured: &std::sync::Mutex<Vec<String>>,
    ) {
        let _ = stream.set_nonblocking(false);
        let mut received = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            if let Some(total) = mock_request_len(&received)
                && received.len() >= total
            {
                break;
            }
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buffer[..n]),
            }
        }
        if let Ok(text) = std::str::from_utf8(&received) {
            captured.lock().unwrap().push(text.to_string());
        }
        let reason = if (200..=299).contains(&status) {
            "OK"
        } else {
            "Error"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
        // Let the client drain the response before the socket is closed.
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    /// Total expected request length (headers + body) once the header block
    /// is complete; `None` while more header bytes are still needed.
    fn mock_request_len(received: &[u8]) -> Option<usize> {
        let header_end = received
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)?;
        let headers = std::str::from_utf8(&received[..header_end]).ok()?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if !name.trim().eq_ignore_ascii_case("content-length") {
                    return None;
                }
                value.trim().parse::<usize>().ok()
            })
            .unwrap_or(0);
        Some(header_end + content_length)
    }

    #[test]
    fn llm_above_threshold_creates_link() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        let server = MockLlm::start(200, decision_body(true, 0.95, "same name"));
        let linker = llm_linker_config(&server.url);

        let result = build_entity_links(
            &db,
            None,
            &links_config(vec![LinkMethod::Llm], Vec::new()),
            &linker,
            TEST_PROMPTS_PATH,
        )
        .unwrap();

        assert_eq!(result.links_created, 1);
        assert_eq!(result.links_skipped, 0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(server.request_count(), 1, "one LLM call for the pair");

        let links = all_links(&db);
        assert_eq!(links.len(), 2, "one bidirectional pair");
        for link in &links {
            assert_eq!(link.method, "llm");
            assert_eq!(link.relation_type, "same_entity");
            assert_eq!(link.confidence, 0.95);
            assert_eq!(link.evidence.as_deref(), Some("same name"));
        }
    }

    #[test]
    fn llm_below_threshold_no_link_but_cache_written() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        // 0.3 < the links config threshold (0.7).
        let server = MockLlm::start(200, decision_body(true, 0.3, "probably not"));
        let linker = llm_linker_config(&server.url);
        let cache = TempCacheDb::new("below-threshold");

        let result = build_entity_links(
            &db,
            Some(&cache.db),
            &links_config(vec![LinkMethod::Llm], Vec::new()),
            &linker,
            TEST_PROMPTS_PATH,
        )
        .unwrap();

        assert_eq!(result.links_created, 0);
        assert_eq!(result.links_skipped, 1);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert!(all_links(&db).is_empty());

        // The below-threshold decision is still cached (task 1.10).
        let entries = cached_decisions(&cache.db);
        assert_eq!(entries.len(), 1);
        let (key, value) = &entries[0];
        assert_eq!(key.len(), 64, "the key is a bare sha256 hex digest: {key}");
        let decision: LinkDecision = serde_json::from_str(value).unwrap();
        assert_eq!(
            decision,
            LinkDecision {
                same_entity: true,
                confidence: 0.3,
                reasoning: "probably not".to_owned(),
            }
        );
    }

    #[test]
    fn llm_same_entity_false_gates_the_link() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        // High confidence, but same_entity = false: the gate needs both.
        let server = MockLlm::start(200, decision_body(false, 0.99, "different people"));
        let linker = llm_linker_config(&server.url);
        let cache = TempCacheDb::new("same-entity-false");

        let result = build_entity_links(
            &db,
            Some(&cache.db),
            &links_config(vec![LinkMethod::Llm], Vec::new()),
            &linker,
            TEST_PROMPTS_PATH,
        )
        .unwrap();

        assert_eq!(result.links_created, 0);
        assert_eq!(result.links_skipped, 1);
        assert!(all_links(&db).is_empty());
        let entries = cached_decisions(&cache.db);
        assert_eq!(entries.len(), 1);
        let decision: LinkDecision = serde_json::from_str(&entries[0].1).unwrap();
        assert!(!decision.same_entity);
    }

    #[test]
    fn llm_cache_hit_skips_the_call() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        let server = MockLlm::start(200, decision_body(true, 0.95, "same name"));
        let linker = llm_linker_config(&server.url);
        let config = links_config(vec![LinkMethod::Llm], Vec::new());
        let cache = TempCacheDb::new("cache-hit");

        let first =
            build_entity_links(&db, Some(&cache.db), &config, &linker, TEST_PROMPTS_PATH).unwrap();
        assert_eq!(first.links_created, 1);
        assert_eq!(server.request_count(), 1);

        // Re-run: the cached decision applies, the row already exists, and
        // the server must not see a second request.
        let second =
            build_entity_links(&db, Some(&cache.db), &config, &linker, TEST_PROMPTS_PATH).unwrap();
        assert_eq!(second.links_created, 0);
        assert_eq!(
            second.links_skipped, 1,
            "cached decision, row already exists"
        );
        assert!(second.errors.is_empty(), "errors: {:?}", second.errors);
        assert_eq!(server.request_count(), 1, "cache hit: no second LLM call");
    }

    #[test]
    fn llm_call_failure_is_recorded_and_pipeline_alive() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        // A 200 with a valid envelope but a content that is not the decision
        // JSON: a strict decision-parse error.
        let bad_content = serde_json::json!({
            "choices": [{ "message": { "content": "not a decision" } }],
        })
        .to_string()
        .into_bytes();
        let server = MockLlm::start(200, bad_content);
        let linker = llm_linker_config(&server.url);
        // equals runs first and must still create its link (pipeline alive).
        let config = links_config(vec![LinkMethod::Equals, LinkMethod::Llm], Vec::new());
        let cache = TempCacheDb::new("call-failure");

        let result =
            build_entity_links(&db, Some(&cache.db), &config, &linker, TEST_PROMPTS_PATH).unwrap();

        assert_eq!(result.links_created, 1, "equals still links the pair");
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.starts_with("llm pair (") && err.contains("invalid decision JSON")),
            "errors: {:?}",
            result.errors
        );
        let links = all_links(&db);
        assert!(
            !links.iter().any(|link| link.method == "llm"),
            "no llm links after a failed pair"
        );
        // A failed pair is not cached.
        assert!(cached_decisions(&cache.db).is_empty());
    }

    #[test]
    fn llm_disabled_excludes_the_method() {
        let db = db::test_util::in_memory_db();
        insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        let server = MockLlm::start(200, decision_body(true, 0.95, "same name"));
        let disabled = LinkerConfig {
            disabled: true,
            ..llm_linker_config(&server.url)
        };

        let result = build_entity_links(
            &db,
            None,
            &links_config(vec![LinkMethod::Llm], Vec::new()),
            &disabled,
            TEST_PROMPTS_PATH,
        )
        .unwrap();

        assert_eq!(result.links_created, 0);
        assert_eq!(result.links_skipped, 0);
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("excluded by linker.disabled"))
        );
        assert_eq!(server.request_count(), 0, "disabled: no LLM call");
        assert!(all_links(&db).is_empty());
    }

    #[test]
    fn llm_prompt_carries_up_to_three_chunk_contexts() {
        let db = db::test_util::in_memory_db();
        let ids = insert_entities(
            &db,
            &[
                ("PERSON", "Alice Smith", "hr"),
                ("PERSON", "Alice Smith", "it"),
            ],
        );
        db.with_conn(|conn| -> Result<(), db::DbError> {
            let documents = db::DocumentDao::new(ConnectionOrTx::Connection(conn));
            let doc = documents.create("text", "doc.txt", None, None)?;
            let chunks = db::ChunkDao::new(ConnectionOrTx::Connection(conn));
            let links = db::ChunkEntityDao::new(ConnectionOrTx::Connection(conn));
            // Entity A: FOUR chunks — only the first three may reach the prompt.
            for (i, text) in [
                "alpha context",
                "beta context",
                "gamma context",
                "delta context",
            ]
            .iter()
            .enumerate()
            {
                let chunk = chunks.create(doc, text, i as i64, None, None)?;
                links.link(chunk, ids[0])?;
            }
            // Entity B: one chunk.
            let chunk = chunks.create(doc, "epsilon context", 0, None, None)?;
            links.link(chunk, ids[1])?;
            Ok(())
        })
        .unwrap()
        .unwrap();

        let server = MockLlm::start(200, decision_body(true, 0.95, "same name"));
        let linker = llm_linker_config(&server.url);
        let result = build_entity_links(
            &db,
            None,
            &links_config(vec![LinkMethod::Llm], Vec::new()),
            &linker,
            TEST_PROMPTS_PATH,
        )
        .unwrap();
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

        let user = server.user_message(0);
        assert!(
            user.contains("Context [0]: alpha context"),
            "prompt:\n{user}"
        );
        assert!(user.contains("Context [1]: beta context"));
        assert!(user.contains("Context [2]: gamma context"));
        assert!(
            !user.contains("delta context"),
            "the 4th chunk must not reach the prompt:\n{user}"
        );
        assert!(user.contains("epsilon context"));
    }
}
