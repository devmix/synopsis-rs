//! The `search_facts` and `get_fact_by_id` tools: fact search with filters
//! and the single-fact lookup with entities and sources (design D4).
//!
//! Thin handlers per design D2: parse the frozen-schema arguments → db DAOs
//! → the frozen wire JSON (`mcp-contract`). No business logic lives here.
//!
//! **Status filtering (design D2 "approved-only"):** `search_facts` applies
//! the frozen default `status = 'approved'` when the argument is omitted,
//! and honours an explicit `status` value (e.g. `'pending'`) as a filter.
//! `get_fact_by_id` intentionally has NO status filter: a pending fact is
//! retrievable by direct id, and its actual `status` is exposed in the
//! response.
//!
//! **Design decisions:**
//! 1. *Error text:* tool-error messages follow the [`McpError`] conventions
//!    established by tasks 5.1/5.3 (e.g. `"invalid arguments for tool
//!    'search_facts': …"`). The tool-error structure (is_error result with
//!    a text block) is the frozen contract; internal message text is not
//!    part of it.
//! 2. *`search_facts` empty `status`:* an explicit empty `status` applies
//!    the `'approved'` default, like a missing one — an empty value must
//!    not silently disable the approved-only filter and return
//!    pending/draft/rejected facts.
//! 3. *`get_fact_by_id` not-found message:* the message says
//!    `"fact with id N not found"` — id, not predicate.
//! 4. *`get_fact_by_id` endpoint lookup errors:* entity lookup errors are
//!    propagated as tool errors instead of silently omitting the entity. A
//!    missing entity (dangling id) is still omitted — unreachable under the
//!    v5 schema's endpoint FKs.
//! 5. *`search_facts` entity-name lookup errors:* the batch entity-name
//!    lookup error is propagated as a tool error.
//!
//! **Response shape:** field names, order and optionality follow the frozen
//! contract (`mcp-contract`), so the wire JSON field order is stable. The
//! `search_facts` entity-name filter is a correlated `EXISTS` in the db
//! crate (a fact whose subject AND object names both match appears once,
//! with a matching total; an `INNER JOIN` would duplicate it in the page
//! while the `COUNT(DISTINCT …)` total would not).

use std::collections::{HashMap, HashSet};

use db::{ConnectionOrTx, EntityDao, Fact, FactDao, FactFilter, FactSourceDao};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::McpError;
use crate::pagination::{DEFAULT_PAGE_SIZE, Page, decode_cursor, normalize_page_size};
use crate::tools::entity::{EntityBrief, entity_brief};

/// The frozen tool name (`mcp-contract`).
pub const SEARCH_FACTS: &str = "search_facts";
/// The frozen tool name (`mcp-contract`).
pub const GET_FACT_BY_ID: &str = "get_fact_by_id";

/// Default fact status for `search_facts` (frozen schema: "Default:
/// 'approved'").
const DEFAULT_FACT_STATUS: &str = "approved";

// ── shared helpers ──────────────────────────────────────────────────────────

/// Parse `page_size` (frozen schema: number, default 20, range 1-200).
/// Lenient parsing: missing or unparseable values fall back to the default
/// rather than erroring; floats truncate toward zero. Out-of-range values
/// are clamped by [`normalize_page_size`], not rejected.
/// (Same convention as `tools::catalog::parse_page_size`.)
fn parse_page_size(value: Option<&Value>) -> i64 {
    let Some(size) = value.and_then(|value| match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    }) else {
        return DEFAULT_PAGE_SIZE;
    };
    normalize_page_size(size)
}

/// The page window from the `cursor` argument: an EMPTY cursor starts the
/// first page with the REQUESTED page size; only a non-empty cursor
/// overrides the offset AND limit (the cursor carries its own page window).
fn page_from(cursor: &str, limit: i64, tool: &'static str) -> Result<Page, McpError> {
    if cursor.is_empty() {
        Ok(Page::first(limit))
    } else {
        decode_cursor(cursor).map_err(|err| McpError::InvalidArguments {
            tool,
            reason: format!("invalid cursor: {err}"),
        })
    }
}

/// Deserialize the argument object; `None` (no arguments) is the empty
/// object, so every filter is absent rather than a parse failure. Unknown
/// keys are ignored.
fn deserialize_args<T: DeserializeOwned>(
    args: Option<&Value>,
    tool: &'static str,
) -> Result<T, McpError> {
    let value = args
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: err.to_string(),
    })
}

/// Serialize the response payload; a failure here is unreachable (all fields
/// are primitives), but the no-panic rule (design D7) keeps an error path
/// instead of a panic (same convention as `tools::catalog`).
fn to_value<T: Serialize>(tool: &'static str, response: T) -> Result<Value, McpError> {
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: format!("serializing the response failed: {err}"),
    })
}

// ── search_facts ────────────────────────────────────────────────────────────

/// Parsed `search_facts` arguments (frozen schema: `predicate`,
/// `entity_name`, `status`, `domain`, `page_size`, `cursor`).
#[derive(Debug, Deserialize)]
struct SearchFactsArgs {
    /// Case-insensitive predicate substring filter (empty = no filter).
    predicate: Option<String>,
    /// Subject-or-object entity name filter (empty = no filter).
    entity_name: Option<String>,
    /// Status filter (default `'approved'`; empty = default).
    status: Option<String>,
    /// Domain filter (empty = all domains).
    domain: Option<String>,
    /// Number of items per page (number, default 20, range 1-200).
    page_size: Option<Value>,
    /// Opaque cursor (empty/omitted = first page).
    cursor: Option<String>,
}

/// One fact entry: field order follows the frozen contract
/// (`mcp-contract`), so the wire JSON field order is stable.
#[derive(Debug, Serialize)]
struct SearchFactOut {
    /// Fact row id.
    id: i64,
    /// The relation predicate.
    predicate: String,
    /// Subject entity id; absent when the fact has no subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_entity_id: Option<i64>,
    /// Subject entity name; absent when there is no subject or it could not
    /// be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_name: Option<String>,
    /// Object entity id; absent when the fact has no object.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_entity_id: Option<i64>,
    /// Object entity name; absent when there is no object or it could not be
    /// resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_name: Option<String>,
    /// Fact domain (`''` = global).
    domain: String,
    /// Fact status.
    status: String,
    /// Validity interval start, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_from: Option<String>,
    /// Validity interval end, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_to: Option<String>,
    /// Source-count weight.
    weight: i64,
}

/// The `search_facts` response.
#[derive(Debug, Serialize)]
struct SearchFactsResponse {
    /// The page of facts (empty, not null, when there are no matches).
    facts: Vec<SearchFactOut>,
    /// Total number of matching facts (before pagination).
    total_count: i64,
    /// Cursor for the next page; present only when more rows exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Handle the `search_facts` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the response payload the server serializes into the tool
/// response's text block.
pub fn handle_search_facts(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: SearchFactsArgs = deserialize_args(args, SEARCH_FACTS)?;

    let limit = parse_page_size(args.page_size.as_ref());
    let page = page_from(
        args.cursor.as_deref().unwrap_or_default(),
        limit,
        SEARCH_FACTS,
    )?;

    // The frozen default is approved-only; an explicit empty `status`
    // applies the default too — an empty string must not silently disable
    // the filter and leak non-approved facts.
    let status = args
        .status
        .filter(|status| !status.is_empty())
        .unwrap_or_else(|| DEFAULT_FACT_STATUS.to_owned());
    let filter = FactFilter {
        predicate: args.predicate.filter(|predicate| !predicate.is_empty()),
        entity_name: args.entity_name.filter(|name| !name.is_empty()),
        status: Some(status),
        domain: args.domain.filter(|domain| !domain.is_empty()),
    };

    let (facts, total_count, entity_names) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let (facts, total_count) =
                FactDao::new(exec).search_paginated(page.offset, page.limit, &filter)?;
            // Batch entity-name lookup for the page's endpoints (ids
            // de-duplicated).
            let ids: HashSet<i64> = facts
                .iter()
                .flat_map(|fact| [fact.subject_entity_id, fact.object_entity_id])
                .flatten()
                .collect();
            let entities = if ids.is_empty() {
                Vec::new()
            } else {
                let entity_ids: Vec<i64> = ids.into_iter().collect();
                EntityDao::new(exec).get_by_ids(&entity_ids)?
            };
            let entity_names: HashMap<i64, String> = entities
                .into_iter()
                .map(|entity| (entity.id, entity.name))
                .collect();
            Ok((facts, total_count, entity_names))
        })
        .and_then(|result| result)?;

    let response = SearchFactsResponse {
        facts: facts
            .iter()
            .map(|fact| search_fact_out(fact, &entity_names))
            .collect(),
        total_count,
        next_cursor: page.next_cursor(total_count),
    };
    to_value(SEARCH_FACTS, response)
}

/// Map a stored fact to the wire entry: the endpoint ids are present when
/// the fact has endpoints (NULL → absent), and the names only when the
/// endpoint resolved.
fn search_fact_out(fact: &Fact, entity_names: &HashMap<i64, String>) -> SearchFactOut {
    let name = |id: Option<i64>| {
        id.and_then(|id| entity_names.get(&id))
            .filter(|name| !name.is_empty())
            .cloned()
    };
    SearchFactOut {
        id: fact.id,
        predicate: fact.predicate.clone(),
        subject_entity_id: fact.subject_entity_id,
        subject_name: name(fact.subject_entity_id),
        object_entity_id: fact.object_entity_id,
        object_name: name(fact.object_entity_id),
        domain: fact.domain.clone(),
        status: fact.status.clone(),
        valid_from: fact.valid_from.clone(),
        valid_to: fact.valid_to.clone(),
        weight: fact.weight,
    }
}

// ── get_fact_by_id ──────────────────────────────────────────────────────────

/// Parsed `get_fact_by_id` arguments (frozen schema: `fact_id`).
#[derive(Debug, Deserialize)]
struct GetFactByIdArgs {
    /// The fact id (required, integer as string).
    fact_id: Option<String>,
}

/// The fact data: field order follows the frozen contract (`mcp-contract`).
#[derive(Debug, Serialize)]
struct FactInfo {
    /// Fact row id.
    id: i64,
    /// The relation predicate.
    predicate: String,
    /// Subject entity id; absent when the fact has no subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_entity_id: Option<i64>,
    /// Object entity id; absent when the fact has no object.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_entity_id: Option<i64>,
    /// Fact domain (`''` = global).
    domain: String,
    /// The raw `metadata` string (not parsed in this tool); absent when
    /// there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
    /// Fact status — any status (no approved-only filter).
    status: String,
    /// Validity interval start, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_from: Option<String>,
    /// Validity interval end, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_to: Option<String>,
    /// Source-count weight.
    weight: i64,
}

/// A fact source, shared with the entity dossier (task 5.8).
#[derive(Debug, Serialize)]
pub(crate) struct FactSourceInfo {
    /// The source document id.
    pub(crate) document_id: i64,
    /// The exact quote; absent when the source has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) quote: Option<String>,
    /// Extraction timestamp.
    pub(crate) extracted_at: String,
}

/// The `get_fact_by_id` response: field order follows the frozen contract
/// (`mcp-contract`).
#[derive(Debug, Serialize)]
struct FactByIdResponse {
    /// The fact data.
    fact: FactInfo,
    /// The subject entity; absent when the fact has no subject or it could
    /// not be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_entity: Option<EntityBrief>,
    /// The object entity; absent when the fact has no object or it could not
    /// be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_entity: Option<EntityBrief>,
    /// The fact's sources; absent when it has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<FactSourceInfo>,
}

/// Handle the `get_fact_by_id` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the response payload the server serializes into the tool
/// response's text block.
pub fn handle_get_fact_by_id(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: GetFactByIdArgs = deserialize_args(args, GET_FACT_BY_ID)?;
    let raw = args.fact_id.unwrap_or_default();
    if raw.is_empty() {
        return Err(McpError::InvalidArguments {
            tool: GET_FACT_BY_ID,
            reason: "'fact_id' argument is required".to_owned(),
        });
    }
    let fact_id: i64 = raw.parse().map_err(|_| McpError::InvalidArguments {
        tool: GET_FACT_BY_ID,
        reason: format!("'fact_id' must be an integer, got {raw:?}"),
    })?;

    let (fact, subject, object, sources) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            // No status filter: any status is retrievable by direct id.
            let Some(fact) = FactDao::new(exec).get_by_id(fact_id)? else {
                return Ok(None);
            };
            let entities = EntityDao::new(exec);
            let subject = match fact.subject_entity_id {
                Some(id) => entities.get_by_id(id)?,
                None => None,
            };
            let object = match fact.object_entity_id {
                Some(id) => entities.get_by_id(id)?,
                None => None,
            };
            let sources = FactSourceDao::new(exec).get_by_fact_id(fact_id)?;
            Ok(Some((fact, subject, object, sources)))
        })
        .and_then(|result| result)?
        .ok_or_else(|| McpError::NotFound {
            what: format!("fact with id {fact_id} not found"),
        })?;

    let response = FactByIdResponse {
        fact: fact_info(&fact),
        subject_entity: subject.as_ref().map(entity_brief),
        object_entity: object.as_ref().map(entity_brief),
        sources: sources
            .into_iter()
            .map(|source| FactSourceInfo {
                document_id: source.document_id,
                quote: source.quote.filter(|quote| !quote.is_empty()),
                extracted_at: source.extracted_at,
            })
            .collect(),
    };
    to_value(GET_FACT_BY_ID, response)
}

/// Map a stored fact to the wire `fact` object.
fn fact_info(fact: &Fact) -> FactInfo {
    FactInfo {
        id: fact.id,
        predicate: fact.predicate.clone(),
        subject_entity_id: fact.subject_entity_id,
        object_entity_id: fact.object_entity_id,
        domain: fact.domain.clone(),
        metadata: fact
            .metadata_json
            .as_ref()
            .filter(|metadata| !metadata.is_empty())
            .cloned(),
        status: fact.status.clone(),
        valid_from: fact.valid_from.clone(),
        valid_to: fact.valid_to.clone(),
        weight: fact.weight,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::{DocumentDao, FactSourceDao, test_util};

    use super::*;

    // ── fixtures ──────────────────────────────────────────────────────────

    /// Search-facts fixture: 3 entities (Alice, Engineering, NDA) + 2
    /// approved facts (Alice `works_in` Engineering, Alice `owns` NDA).
    fn seeded_facts_db() -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let alice =
                entities.create("employee", "Alice", "", Some("Employee entity"), None, None)?;
            let engineering = entities.create("department", "Engineering", "", None, None, None)?;
            let nda = entities.create("policy", "NDA", "", None, None, None)?;
            facts.create(
                Some(alice),
                "works_in",
                Some(engineering),
                "",
                None,
                None,
                None,
            )?;
            facts.create(Some(alice), "owns", Some(nda), "", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// Pagination fixture: 3 entities + 2 approved facts.
    fn seeded_pagination_db() -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let alice = entities.create("employee", "Alice", "", None, None, None)?;
            let engineering = entities.create("department", "Engineering", "", None, None, None)?;
            let nda = entities.create("policy", "NDA", "", None, None, None)?;
            facts.create(
                Some(alice),
                "works_in",
                Some(engineering),
                "",
                None,
                None,
                None,
            )?;
            facts.create(Some(alice), "owns", Some(nda), "", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// `n` approved facts (Alice → Dept_X, distinct predicates).
    fn seeded_n_facts_db(n: usize) -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let alice = entities.create("employee", "Alice", "", None, None, None)?;
            for i in 0..n {
                let target = entities.create(
                    "department",
                    &format!("Dept_{}", char::from(b'A' + i as u8)),
                    "",
                    None,
                    None,
                    None,
                )?;
                facts.create(
                    Some(alice),
                    &format!("pred_{i}"),
                    Some(target),
                    "",
                    None,
                    None,
                    None,
                )?;
            }
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// Set a fact's status directly (the DAO has no status update; the db
    /// crate's own tests use the same raw-SQL pattern).
    fn set_fact_status(db: &db::Db, fact_id: i64, status: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE facts SET status = ? WHERE id = ?",
                (status, fact_id),
            )
        })
        .expect("checkout")
        .expect("status update");
    }

    /// Get-by-id fixture: Alice `works_at` Acme Corp (hr) + one source
    /// (document + quote). Returns (db, fact_id).
    fn seeded_get_by_id_db() -> (db::Db, i64) {
        let db = test_util::in_memory_db();
        let fact_id = db
            .exec_tx(|tx| -> Result<i64, db::DbError> {
                let exec = ConnectionOrTx::Transaction(&*tx);
                let entities = EntityDao::new(exec);
                let documents = DocumentDao::new(exec);
                let facts = FactDao::new(exec);
                let sources = FactSourceDao::new(exec);
                let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
                let acme = entities.create("ORGANIZATION", "Acme Corp", "hr", None, None, None)?;
                let fact_id =
                    facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
                let doc = documents.create("markdown", "/docs/hr.md", None, None)?;
                sources.create(fact_id, doc, Some("Alice works at Acme Corp"), None)?;
                Ok(fact_id)
            })
            .expect("seed transaction commits");
        (db, fact_id)
    }

    fn search(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_search_facts(db, args.as_ref())
    }

    fn by_id(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_get_fact_by_id(db, args.as_ref())
    }

    // ── search_facts: filters ─────────────────────────────────────────────

    /// Default returns approved facts: no arguments → the seeded approved
    /// facts; every returned fact is approved.
    #[test]
    fn facts_default_returns_approved_facts() {
        let response = search(&seeded_facts_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
        assert_eq!(response["facts"].as_array().unwrap().len(), 2);
        for fact in response["facts"].as_array().unwrap() {
            assert_eq!(fact["status"], "approved");
        }
    }

    /// Predicate substring filter: case-insensitive, and a non-matching
    /// predicate is a valid empty page.
    #[test]
    fn facts_predicate_filter_is_case_insensitive_substring() {
        let db = &seeded_facts_db();
        for (query, want) in [("works", 1), ("WORKS", 1), ("works_in", 1), ("located", 0)] {
            let response = search(db, Some(serde_json::json!({ "predicate": query }))).unwrap();
            assert_eq!(response["total_count"], serde_json::json!(want), "{query}");
        }
    }

    /// The `entity_name` filter finds Alice's facts: Alice is the subject
    /// of both seeded facts.
    #[test]
    fn facts_entity_name_filter_finds_subject_facts() {
        let response = search(
            &seeded_facts_db(),
            Some(serde_json::json!({ "entity_name": "Alice" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
    }

    /// Alice is subject in one fact and object in another — both match,
    /// and each returned fact carries at least one resolved entity name.
    #[test]
    fn facts_entity_name_filter_matches_subject_or_object() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let alice = entities.create("employee", "Alice", "", None, None, None)?;
            let engineering = entities.create("department", "Engineering", "", None, None, None)?;
            let laptop = entities.create("system", "Laptop", "", None, None, None)?;
            facts.create(
                Some(alice),
                "works_in",
                Some(engineering),
                "",
                None,
                None,
                None,
            )?;
            facts.create(Some(laptop), "used_by", Some(alice), "", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = search(&db, Some(serde_json::json!({ "entity_name": "Alice" }))).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
        for fact in response["facts"].as_array().unwrap() {
            let has_subject = fact.get("subject_name").is_some();
            let has_object = fact.get("object_name").is_some();
            assert!(
                has_subject || has_object,
                "at least one entity name must be set: {fact}"
            );
        }
    }

    /// Domain filter (exact match).
    #[test]
    fn facts_domain_filter() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
            let acme = entities.create("ORGANIZATION", "Acme", "hr", None, None, None)?;
            let beta = entities.create("ORGANIZATION", "Beta", "it", None, None, None)?;
            facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
            facts.create(Some(alice), "visits", Some(beta), "it", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = search(&db, Some(serde_json::json!({ "domain": "hr" }))).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(response["facts"][0]["predicate"], "works_at");

        let none = search(&db, Some(serde_json::json!({ "domain": "nope" }))).unwrap();
        assert_eq!(none["total_count"], serde_json::json!(0));
        assert_eq!(none["facts"], serde_json::json!([]));
    }

    // ── search_facts: status (approved-only) ──────────────────────────────

    /// Pending facts never leak into the default (approved) search, and
    /// `status: "pending"` selects them explicitly.
    #[test]
    fn facts_pending_never_leaks_into_default_search() {
        let db = seeded_facts_db();
        // Promote one seeded fact to pending (raw SQL; the DAO only creates
        // approved facts).
        let pending_id = search(&db, Some(serde_json::json!({ "predicate": "owns" })))
            .unwrap()["facts"][0]["id"]
            .as_i64()
            .unwrap();
        set_fact_status(&db, pending_id, "pending");

        let default = search(&db, None).unwrap();
        assert_eq!(default["total_count"], serde_json::json!(1));
        assert_eq!(default["facts"][0]["predicate"], "works_in");
        assert_eq!(default["facts"][0]["status"], "approved");

        let pending = search(&db, Some(serde_json::json!({ "status": "pending" }))).unwrap();
        assert_eq!(pending["total_count"], serde_json::json!(1));
        assert_eq!(pending["facts"][0]["predicate"], "owns");
        assert_eq!(pending["facts"][0]["status"], "pending");
    }

    /// An explicit empty `status` applies the 'approved' default — an
    /// empty string must not disable the filter and return every status.
    #[test]
    fn facts_empty_status_applies_approved_default() {
        let db = seeded_facts_db();
        let pending_id = search(&db, Some(serde_json::json!({ "predicate": "owns" })))
            .unwrap()["facts"][0]["id"]
            .as_i64()
            .unwrap();
        set_fact_status(&db, pending_id, "pending");

        let response = search(&db, Some(serde_json::json!({ "status": "" }))).unwrap();
        assert_eq!(
            response["total_count"],
            serde_json::json!(1),
            "the approved-only default must hold for an empty status"
        );
        assert_eq!(response["facts"][0]["status"], "approved");
    }

    // ── search_facts: pagination ──────────────────────────────────────────

    #[test]
    fn facts_pagination_walk() {
        let db = seeded_pagination_db();

        let first = search(&db, Some(serde_json::json!({ "page_size": 1 }))).unwrap();
        assert_eq!(first["facts"].as_array().unwrap().len(), 1);
        assert_eq!(first["total_count"], serde_json::json!(2));
        assert!(first["next_cursor"].is_string(), "{first}");

        let second = search(
            &db,
            Some(serde_json::json!({ "page_size": 1, "cursor": first["next_cursor"].as_str().unwrap() })),
        )
        .unwrap();
        assert_eq!(second["facts"].as_array().unwrap().len(), 1);
        assert_ne!(
            first["facts"][0]["id"], second["facts"][0]["id"],
            "pages must not repeat facts"
        );
        assert!(
            second.get("next_cursor").is_none(),
            "no next_cursor on the last page: {second}"
        );
    }

    /// `page_size` leniency: out-of-range (or unparseable) values clamp to
    /// the default 20.
    #[test]
    fn facts_page_size_out_of_range_defaults_to_twenty() {
        let db = seeded_n_facts_db(25);
        for page_size in [
            serde_json::json!(0),
            serde_json::json!(500),
            serde_json::json!("x"),
        ] {
            let response =
                search(&db, Some(serde_json::json!({ "page_size": page_size }))).unwrap();
            assert_eq!(
                response["facts"].as_array().unwrap().len(),
                20,
                "page_size {page_size} must clamp to the default"
            );
            assert!(response["next_cursor"].is_string(), "{response}");
        }
    }

    /// An invalid cursor is a tool error.
    #[test]
    fn facts_invalid_cursor_is_an_error() {
        let db = test_util::in_memory_db();
        let err = search(
            &db,
            Some(serde_json::json!({ "cursor": "not-valid-base64!!!" })),
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("invalid cursor"), "got: {err}");
    }

    /// The predicate filter returns the fact with resolved subject AND
    /// object names.
    #[test]
    fn facts_response_fields() {
        let response = search(
            &seeded_facts_db(),
            Some(serde_json::json!({ "predicate": "works_in" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        let fact = &response["facts"][0];
        assert!(fact["id"].as_i64().unwrap() > 0, "id must not be zero");
        assert_eq!(fact["predicate"], "works_in");
        assert_eq!(fact["status"], "approved");
        assert_eq!(fact["subject_name"], "Alice");
        assert_eq!(fact["object_name"], "Engineering");
        assert_eq!(fact["domain"], "");
        assert!(fact["weight"].is_i64());
    }

    /// Optionality: a fact without a subject omits the subject fields; the
    /// always-present fields stay present.
    #[test]
    fn facts_endpoint_optionality_shapes() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let facts = FactDao::new(exec);
            let target = entities.create("system", "Laptop", "", None, None, None)?;
            facts.create(
                None,
                "located_in",
                Some(target),
                "geo",
                None,
                Some("2024-01-01"),
                None,
            )?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = search(&db, None).unwrap();
        let fact = &response["facts"][0];
        assert!(fact.get("subject_entity_id").is_none(), "{fact}");
        assert!(fact.get("subject_name").is_none(), "{fact}");
        assert!(fact["object_entity_id"].is_i64(), "{fact}");
        assert_eq!(fact["object_name"], "Laptop");
        assert_eq!(fact["valid_from"], "2024-01-01");
        assert!(fact.get("valid_to").is_none(), "{fact}");
        for key in ["id", "predicate", "domain", "status", "weight"] {
            assert!(fact.get(key).is_some(), "{key} must be present");
        }
    }

    /// An empty db is a valid empty page (facts is `[]`, not null).
    #[test]
    fn facts_empty_db_is_empty_page() {
        let response = search(&test_util::in_memory_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["facts"], serde_json::json!([]));
        assert!(response.get("next_cursor").is_none(), "{response}");
    }

    // ── get_fact_by_id ────────────────────────────────────────────────────

    #[test]
    fn by_id_missing_fact_id_is_an_error() {
        let (db, _) = seeded_get_by_id_db();
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "fact_id": "" })),
        ] {
            let err = by_id(&db, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(
                err.to_string().contains("'fact_id' argument is required"),
                "got: {err}"
            );
        }
    }

    /// A non-integer fact id is an error.
    #[test]
    fn by_id_non_integer_fact_id_is_an_error() {
        let (db, _) = seeded_get_by_id_db();
        for raw in ["abc", "1.5", " 1"] {
            let err = by_id(&db, Some(serde_json::json!({ "fact_id": raw }))).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(err.to_string().contains("must be an integer"), "got: {err}");
        }
    }

    /// A nonexistent fact id is a not-found tool error; the message says
    /// "id", not "Predicate".
    #[test]
    fn by_id_nonexistent_fact_is_not_found() {
        let (db, _) = seeded_get_by_id_db();
        let err = by_id(&db, Some(serde_json::json!({ "fact_id": "99999" }))).unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("fact with id 99999 not found"),
            "got: {err}"
        );
        let result = err.into_tool_result();
        let rmcp::model::CallToolResponse::Complete(call) = result else {
            panic!("expected a complete tool result");
        };
        assert_eq!(call.is_error, Some(true));
    }

    /// A valid fact returns data with entities and sources.
    #[test]
    fn by_id_response_fields() {
        let (db, fact_id) = seeded_get_by_id_db();
        let response = by_id(
            &db,
            Some(serde_json::json!({ "fact_id": fact_id.to_string() })),
        )
        .unwrap();

        assert_eq!(response["fact"]["id"], fact_id);
        assert_eq!(response["fact"]["predicate"], "works_at");
        assert_eq!(response["fact"]["status"], "approved");
        assert_eq!(response["fact"]["domain"], "hr");
        assert!(response["fact"].get("metadata").is_none(), "{response}");
        assert!(response["fact"]["subject_entity_id"].is_i64());
        assert!(response["fact"]["object_entity_id"].is_i64());

        assert_eq!(response["subject_entity"]["name"], "Alice");
        assert_eq!(response["subject_entity"]["type"], "PERSON");
        assert_eq!(response["subject_entity"]["domain"], "hr");
        assert_eq!(response["object_entity"]["name"], "Acme Corp");
        assert_eq!(response["object_entity"]["type"], "ORGANIZATION");

        assert_eq!(response["sources"].as_array().unwrap().len(), 1);
        let source = &response["sources"][0];
        assert!(source["document_id"].is_i64());
        assert_eq!(source["quote"], "Alice works at Acme Corp");
        assert!(source["extracted_at"].is_string());
    }

    /// The `metadata` field is the RAW string (not parsed in this tool),
    /// present when set.
    #[test]
    fn by_id_metadata_is_the_raw_string() {
        let db = test_util::in_memory_db();
        let fact_id = db
            .exec_tx(|tx| -> Result<i64, db::DbError> {
                let exec = ConnectionOrTx::Transaction(&*tx);
                let entities = EntityDao::new(exec);
                let facts = FactDao::new(exec);
                let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
                let acme = entities.create("ORGANIZATION", "Acme", "hr", None, None, None)?;
                facts.create(
                    Some(alice),
                    "works_at",
                    Some(acme),
                    "hr",
                    Some(r#"{"threshold_amount":100}"#),
                    Some("2024-01-01"),
                    Some("2024-12-31"),
                )
            })
            .expect("seed transaction commits");

        let response = by_id(
            &db,
            Some(serde_json::json!({ "fact_id": fact_id.to_string() })),
        )
        .unwrap();
        assert_eq!(
            response["fact"]["metadata"], r#"{"threshold_amount":100}"#,
            "metadata must be the raw string, not parsed"
        );
        assert_eq!(response["fact"]["valid_from"], "2024-01-01");
        assert_eq!(response["fact"]["valid_to"], "2024-12-31");
        // No sources seeded → the field is omitted.
        assert!(response.get("sources").is_none(), "{response}");
    }

    /// CRITICAL behavior: a PENDING fact is retrievable by direct id — no
    /// approved-only filter — and its actual status is exposed.
    #[test]
    fn by_id_returns_pending_fact_with_its_status() {
        let (db, fact_id) = seeded_get_by_id_db();
        set_fact_status(&db, fact_id, "pending");

        let response = by_id(
            &db,
            Some(serde_json::json!({ "fact_id": fact_id.to_string() })),
        )
        .unwrap();
        assert_eq!(response["fact"]["status"], "pending");
    }

    /// Sources shape: a source without a quote omits the `quote` field.
    #[test]
    fn by_id_source_without_quote_omits_quote_field() {
        let db = test_util::in_memory_db();
        let fact_id = db
            .exec_tx(|tx| -> Result<i64, db::DbError> {
                let exec = ConnectionOrTx::Transaction(&*tx);
                let entities = EntityDao::new(exec);
                let documents = DocumentDao::new(exec);
                let facts = FactDao::new(exec);
                let sources = FactSourceDao::new(exec);
                let alice = entities.create("PERSON", "Alice", "hr", None, None, None)?;
                let acme = entities.create("ORGANIZATION", "Acme", "hr", None, None, None)?;
                let fact_id =
                    facts.create(Some(alice), "works_at", Some(acme), "hr", None, None, None)?;
                let doc = documents.create("markdown", "/docs/hr.md", None, None)?;
                sources.create(fact_id, doc, None, Some("2026-01-02 03:04:05"))?;
                Ok(fact_id)
            })
            .expect("seed transaction commits");

        let response = by_id(
            &db,
            Some(serde_json::json!({ "fact_id": fact_id.to_string() })),
        )
        .unwrap();
        let source = &response["sources"][0];
        assert!(source.get("quote").is_none(), "{source}");
        assert_eq!(source["extracted_at"], "2026-01-02 03:04:05");
    }
}
