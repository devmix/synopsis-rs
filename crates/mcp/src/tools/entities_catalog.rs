//! The `catalog_entities` and `search_entities_by_type` tools: the paginated
//! entity listings (design D4).
//!
//! Thin handlers per design D2: parse the frozen-schema arguments →
//! [`EntityDao::list_paginated`] → the frozen wire JSON (`mcp-contract`).
//! No business logic lives here.
//!
//! **DRY:** the two tools return field-identical entity entries, and their
//! handlers differ only in the required-`entity_type` check and the `name`
//! filter. This module keeps ONE entity wire struct, ONE response struct and
//! ONE list core (`list_entities`); the two public handlers differ only in
//! argument parsing.
//!
//! **Design (error text):** tool-error messages follow the [`McpError`]
//! conventions established by tasks 5.1/5.3 (e.g. `"invalid arguments for
//! tool 'catalog_entities': …"`). The tool-error structure (is_error result
//! with a text block) is the frozen contract; internal message text is not
//! part of it.
//!
//! **Response shape:** field names, order and optionality follow the frozen
//! contract (`mcp-contract`) for both tools. `description` is omitted when
//! absent OR empty; `metadata` is the parsed `metadata_json`, the raw string
//! when it is not valid JSON, and omitted for absent/empty/JSON-`null`
//! metadata. `aliases` is an additive field (multilingual-entity-resolution
//! D8): the recorded surface names, own name excluded, always present (empty
//! array when there are none), appended after the last frozen field.

use std::collections::HashMap;

use db::{ConnectionOrTx, DbError, Entity, EntityAliasDao, EntityDao, EntityFilter};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::McpError;
use crate::pagination::{DEFAULT_PAGE_SIZE, Page, decode_cursor, normalize_page_size};

/// The frozen tool name (`mcp-contract`).
pub const CATALOG_ENTITIES: &str = "catalog_entities";
/// The frozen tool name (`mcp-contract`).
pub const SEARCH_ENTITIES_BY_TYPE: &str = "search_entities_by_type";

// ── shared wire shape ───────────────────────────────────────────────────────

/// One entity entry, shared by both tools (field-identical shapes): field
/// order follows the frozen contract (`mcp-contract`), so the wire JSON
/// field order is stable.
#[derive(Debug, Serialize)]
struct EntityEntry {
    /// Entity row id.
    id: i64,
    /// Entity name.
    name: String,
    /// Entity type.
    #[serde(rename = "type")]
    r#type: String,
    /// Entity domain (empty = global).
    domain: String,
    /// Free-text description; absent when NULL or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Extraction confidence; absent when NULL.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f64>,
    /// The parsed `metadata_json`; the raw string when it is not valid JSON;
    /// absent when there is no metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
    /// The recorded surface names that resolved to this entity (the entity's
    /// own name excluded); always present, empty when there are none
    /// (additive field, multilingual-entity-resolution D8 — appended after
    /// the last frozen field, existing field order untouched).
    aliases: Vec<String>,
}

/// The entity-listing response, shared by both tools (identical shapes).
#[derive(Debug, Serialize)]
struct EntitiesResponse {
    /// The page of entities (empty, not null, when there are no matches).
    entities: Vec<EntityEntry>,
    /// Total number of matching entities (before pagination).
    total_count: i64,
    /// Cursor for the next page; present only when more rows exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Map a stored entity to the wire entry. `aliases` is the entity's recorded
/// surface names with the own name removed (fetched by the list core,
/// batched per response).
fn entity_entry(entity: &Entity, aliases: Vec<String>) -> EntityEntry {
    EntityEntry {
        id: entity.id,
        name: entity.name.clone(),
        r#type: entity.entity_type.clone(),
        domain: entity.domain.clone(),
        description: entity
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .map(str::to_owned),
        confidence: entity.confidence,
        metadata: metadata_field(entity.metadata_json.as_deref()),
        aliases,
    }
}

/// The recorded aliases of a page of entities (multilingual-entity-resolution
/// D8): one PK lookup per entity (the `entity_aliases` table's PK covers the
/// per-id query), all inside the caller's single pool checkout (batched per
/// response). The entity's own name is excluded from its list — a merge
/// records the surviving name as an alias of itself.
fn alias_map_for(
    exec: ConnectionOrTx<'_>,
    entities: &[Entity],
) -> Result<HashMap<i64, Vec<String>>, DbError> {
    let dao = EntityAliasDao::new(exec);
    let mut map = HashMap::with_capacity(entities.len());
    for entity in entities {
        let aliases = dao.aliases_of(entity.id)?;
        map.insert(
            entity.id,
            aliases
                .into_iter()
                .filter(|alias| alias != &entity.name)
                .collect(),
        );
    }
    Ok(map)
}

/// The parsed `metadata_json` for the wire: absent/empty → no field; valid
/// JSON `null` → no field; valid JSON → the parsed value; malformed JSON →
/// the raw string.
fn metadata_field(metadata_json: Option<&str>) -> Option<Value> {
    let raw = metadata_json.filter(|raw| !raw.is_empty())?;
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => (!value.is_null()).then_some(value),
        Err(_) => Some(Value::String(raw.to_owned())),
    }
}

// ── shared list core ────────────────────────────────────────────────────────

/// One page of entities matching `filter`, as the response payload. Shared
/// by both tools (the handlers differ only in argument parsing).
fn list_entities(
    db: &db::Db,
    tool: &'static str,
    filter: &EntityFilter,
    page_size: Option<&Value>,
    cursor: Option<&str>,
) -> Result<Value, McpError> {
    let limit = parse_page_size(page_size);
    let cursor = cursor.unwrap_or_default();
    // An EMPTY cursor starts the first page with the REQUESTED page size;
    // only a non-empty cursor overrides the offset AND limit (the cursor
    // carries its own page window).
    let page = if cursor.is_empty() {
        Page::first(limit)
    } else {
        decode_cursor(cursor).map_err(|err| McpError::InvalidArguments {
            tool,
            reason: format!("invalid cursor: {err}"),
        })?
    };

    let (entities, total_count, mut alias_map) = db
        .with_conn(|conn| {
            let exec = ConnectionOrTx::Connection(conn);
            let entities = EntityDao::new(exec);
            let (entities, total_count) =
                entities.list_paginated(page.offset, page.limit, filter)?;
            // Recorded aliases for the page (multilingual-entity-resolution
            // D8): one PK lookup per entity, batched into this response's
            // single pool checkout.
            let alias_map = alias_map_for(exec, &entities)?;
            Ok((entities, total_count, alias_map))
        })
        .and_then(|result| result)?;

    let response = EntitiesResponse {
        entities: entities
            .iter()
            .map(|entity| entity_entry(entity, alias_map.remove(&entity.id).unwrap_or_default()))
            .collect(),
        total_count,
        next_cursor: page.next_cursor(total_count),
    };
    to_value(tool, response)
}

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

/// Serialize the response payload; a failure here is unreachable (all fields
/// are primitives), but the no-panic rule (design D7) keeps an error path
/// instead of a panic (same convention as `tools::catalog`).
fn to_value<T: Serialize>(tool: &'static str, response: T) -> Result<Value, McpError> {
    serde_json::to_value(response).map_err(|err| McpError::InvalidArguments {
        tool,
        reason: format!("serializing the response failed: {err}"),
    })
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

// ── catalog_entities ────────────────────────────────────────────────────────

/// Parsed `catalog_entities` arguments (frozen schema: `page_size`, `cursor`,
/// `type`, `domain`, `name`).
#[derive(Debug, Deserialize)]
struct CatalogEntitiesArgs {
    /// Number of items per page (number, default 20, range 1-200).
    page_size: Option<Value>,
    /// Opaque cursor (empty/omitted = first page).
    cursor: Option<String>,
    /// Optional entity-type filter (empty = all types).
    #[serde(rename = "type")]
    entity_type: Option<String>,
    /// Optional domain filter (empty = all domains).
    domain: Option<String>,
    /// Optional name substring filter (empty = no filter).
    name: Option<String>,
}

/// Handle the `catalog_entities` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the response payload the server serializes into the tool
/// response's text block.
pub fn handle_catalog_entities(db: &db::Db, args: Option<&Value>) -> Result<Value, McpError> {
    let args: CatalogEntitiesArgs = deserialize_args(args, CATALOG_ENTITIES)?;
    let filter = EntityFilter {
        entity_type: args
            .entity_type
            .filter(|entity_type| !entity_type.is_empty()),
        domain: args.domain.filter(|domain| !domain.is_empty()),
        name: args.name.filter(|name| !name.is_empty()),
    };
    list_entities(
        db,
        CATALOG_ENTITIES,
        &filter,
        args.page_size.as_ref(),
        args.cursor.as_deref(),
    )
}

// ── search_entities_by_type ─────────────────────────────────────────────────

/// Parsed `search_entities_by_type` arguments (frozen schema:
/// `entity_type`, `domain`, `page_size`, `cursor`).
#[derive(Debug, Deserialize)]
struct SearchEntitiesByTypeArgs {
    /// Entity type to filter by (required, non-empty).
    entity_type: Option<String>,
    /// Optional domain filter (empty = all domains).
    domain: Option<String>,
    /// Number of items per page (number, default 20, range 1-200).
    page_size: Option<Value>,
    /// Opaque cursor (empty/omitted = first page).
    cursor: Option<String>,
}

/// Handle the `search_entities_by_type` tool call (design D2/D4).
///
/// `args` is the raw JSON argument object (`None` = no arguments). The
/// result is the response payload the server serializes into the tool
/// response's text block.
pub fn handle_search_entities_by_type(
    db: &db::Db,
    args: Option<&Value>,
) -> Result<Value, McpError> {
    let args: SearchEntitiesByTypeArgs = deserialize_args(args, SEARCH_ENTITIES_BY_TYPE)?;
    let entity_type = args.entity_type.unwrap_or_default();
    if entity_type.is_empty() {
        return Err(McpError::InvalidArguments {
            tool: SEARCH_ENTITIES_BY_TYPE,
            reason: "'entity_type' argument is required and must not be empty".to_owned(),
        });
    }
    let filter = EntityFilter {
        entity_type: Some(entity_type),
        domain: args.domain.filter(|domain| !domain.is_empty()),
        ..EntityFilter::default()
    };
    list_entities(
        db,
        SEARCH_ENTITIES_BY_TYPE,
        &filter,
        args.page_size.as_ref(),
        args.cursor.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::test_util;

    use super::*;

    /// Catalog-entities fixture: 2 employees (Alice with description +
    /// metadata, Bob with description only) + 1 policy.
    fn seeded_entities_db() -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            entities.create(
                "employee",
                "Alice",
                "hr",
                Some("Senior engineer"),
                None,
                Some(r#"{"role":"senior_engineer"}"#),
            )?;
            entities.create(
                "employee",
                "Bob",
                "engineering",
                Some("Team lead"),
                None,
                None,
            )?;
            entities.create("policy", "NDA", "hr", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");
        db
    }

    /// `n` employees in the `hr` domain (pagination fixture).
    fn seeded_n_employees_db(n: usize) -> db::Db {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            for i in 0..n {
                entities.create(
                    "employee",
                    &format!("Char_{}", char::from(b'A' + i as u8)),
                    "hr",
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

    fn catalog(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_catalog_entities(db, args.as_ref())
    }

    fn by_type(db: &db::Db, args: Option<Value>) -> Result<Value, McpError> {
        handle_search_entities_by_type(db, args.as_ref())
    }

    // ── catalog_entities: filters ─────────────────────────────────────────

    #[test]
    fn entities_empty_request_returns_all() {
        let response = catalog(&seeded_entities_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(3));
        assert_eq!(response["entities"].as_array().unwrap().len(), 3);
        assert!(
            response.get("next_cursor").is_none(),
            "all rows fit one page: {response}"
        );
    }

    #[test]
    fn entities_type_filter() {
        let response = catalog(
            &seeded_entities_db(),
            Some(serde_json::json!({ "type": "employee" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
        assert_eq!(response["entities"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn entities_domain_filter() {
        let response = catalog(
            &seeded_entities_db(),
            Some(serde_json::json!({ "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
    }

    /// Type and domain filters combine (both reach the same DAO `WHERE`).
    #[test]
    fn entities_type_and_domain_filters_combine() {
        let response = catalog(
            &seeded_entities_db(),
            Some(serde_json::json!({ "type": "employee", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(response["entities"][0]["name"], "Alice");
    }

    /// Name substring filter (frozen schema `name`): case-insensitive on the
    /// entity name.
    #[test]
    fn entities_name_filter_is_case_insensitive_substring() {
        let db = &seeded_entities_db();
        for (query, want) in [("ali", 1), ("NDA", 1), ("bob", 1), ("nonexistent", 0)] {
            let response = catalog(db, Some(serde_json::json!({ "name": query }))).unwrap();
            assert_eq!(response["total_count"], serde_json::json!(want), "{query}");
        }
    }

    // ── catalog_entities: cursor pagination ───────────────────────────────

    #[test]
    fn entities_cursor_pagination_walk() {
        let db = seeded_n_employees_db(4);

        // First page.
        let first = catalog(&db, Some(serde_json::json!({ "page_size": 2 }))).unwrap();
        assert_eq!(first["entities"].as_array().unwrap().len(), 2);
        assert_eq!(first["total_count"], serde_json::json!(4));
        assert!(first["next_cursor"].is_string(), "{first}");

        // Second page (via the cursor; it carries its own page window).
        let second = catalog(
            &db,
            Some(serde_json::json!({ "cursor": first["next_cursor"].as_str().unwrap() })),
        )
        .unwrap();
        assert_eq!(second["entities"].as_array().unwrap().len(), 2);

        // No overlap between pages.
        let ids = |page: &Value| {
            page["entities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["id"].as_i64().unwrap())
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(
            ids(&first).is_disjoint(&ids(&second)),
            "pages must not overlap"
        );

        // Second page is the last page: no next_cursor.
        assert!(
            second.get("next_cursor").is_none(),
            "no next_cursor on the last page: {second}"
        );
    }

    /// An empty cursor honours the REQUESTED page size (the cursor's own
    /// window only applies to a non-empty cursor).
    #[test]
    fn entities_empty_cursor_uses_requested_page_size() {
        let db = seeded_n_employees_db(5);
        let response = catalog(
            &db,
            Some(serde_json::json!({ "page_size": 3, "cursor": "" })),
        )
        .unwrap();
        assert_eq!(response["entities"].as_array().unwrap().len(), 3);
        assert!(response["next_cursor"].is_string(), "{response}");
    }

    /// An invalid cursor is a tool error.
    #[test]
    fn entities_invalid_cursor_is_an_error() {
        let db = test_util::in_memory_db();
        let err = catalog(
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

    /// `page_size` out of range (or unparseable) clamps to the default 20
    /// (`normalize_page_size`) instead of erroring.
    #[test]
    fn entities_page_size_out_of_range_defaults_to_twenty() {
        let db = seeded_n_employees_db(25);
        for page_size in [
            serde_json::json!(0),
            serde_json::json!(500),
            serde_json::json!("x"),
        ] {
            let response =
                catalog(&db, Some(serde_json::json!({ "page_size": page_size }))).unwrap();
            assert_eq!(
                response["entities"].as_array().unwrap().len(),
                20,
                "page_size {page_size} must clamp to the default"
            );
            assert!(response["next_cursor"].is_string(), "{response}");
        }
    }

    // ── catalog_entities: shapes (field mapping) ──────────────────────────

    #[test]
    fn entities_empty_db_is_empty_page_without_cursor() {
        let response = catalog(&test_util::in_memory_db(), None).unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["entities"], serde_json::json!([]));
        assert!(response.get("next_cursor").is_none(), "{response}");
    }

    /// Field mapping: `description`/`confidence`/`metadata` optionality —
    /// valid JSON metadata, malformed metadata (raw string fallback),
    /// JSON-`null` metadata (omitted), empty description (omitted), NULL
    /// confidence (omitted), set confidence (present).
    #[test]
    fn entities_metadata_and_optionality_shapes() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            entities.create("employee", "Conf", "hr", Some("d"), Some(0.9), None)?;
            entities.create("employee", "BadMeta", "hr", None, None, Some("not-json{"))?;
            entities.create("employee", "NullMeta", "hr", None, None, Some("null"))?;
            entities.create("employee", "EmptyDesc", "hr", Some(""), None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = catalog(&db, None).unwrap();
        let by_name: std::collections::HashMap<String, Value> = response["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap().to_owned(), e.clone()))
            .collect();

        let conf = &by_name["Conf"];
        assert_eq!(conf["confidence"], serde_json::json!(0.9));
        assert_eq!(conf["description"], "d");
        assert!(conf.get("metadata").is_none(), "{conf}");

        let bad = &by_name["BadMeta"];
        assert_eq!(bad["metadata"], "not-json{");
        assert!(bad.get("description").is_none(), "{bad}");
        assert!(bad.get("confidence").is_none(), "{bad}");

        let null_meta = &by_name["NullMeta"];
        assert!(
            null_meta.get("metadata").is_none(),
            "JSON null metadata must be omitted: {null_meta}"
        );

        let empty_desc = &by_name["EmptyDesc"];
        assert!(
            empty_desc.get("description").is_none(),
            "empty description must be omitted: {empty_desc}"
        );

        // Always-present fields stay present.
        for entity in by_name.values() {
            assert!(entity.get("id").is_some());
            assert!(entity.get("name").is_some());
            assert!(entity.get("type").is_some());
            assert!(entity.get("domain").is_some());
        }
    }

    /// The seeded fixture maps end-to-end: Alice carries description and
    /// parsed metadata; the type field carries the stored type.
    #[test]
    fn entities_seeded_fixture_field_mapping() {
        let response = catalog(&seeded_entities_db(), None).unwrap();
        let alice = response["entities"][0].clone();
        assert_eq!(alice["name"], "Alice");
        assert_eq!(alice["type"], "employee");
        assert_eq!(alice["domain"], "hr");
        assert_eq!(alice["description"], "Senior engineer");
        assert_eq!(alice["metadata"]["role"], "senior_engineer");
        // Additive aliases field: no recorded aliases → present and empty.
        assert_eq!(alice["aliases"], serde_json::json!([]));
    }

    /// The entity entry carries the recorded aliases (additive field,
    /// multilingual-entity-resolution D8): the entity's own name is excluded
    /// from the list (a merge records the surviving name as an alias of
    /// itself), and an entity without aliases gets an empty array that is
    /// still present.
    #[test]
    fn entities_aliases_field_mapping() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let exec = ConnectionOrTx::Transaction(&*tx);
            let entities = EntityDao::new(exec);
            let aliases = EntityAliasDao::new(exec);
            let alice = entities.create("employee", "Alice", "hr", None, None, None)?;
            aliases.insert_or_ignore(alice, "Alicia")?;
            aliases.insert_or_ignore(alice, "Alice")?; // own name — must be excluded
            entities.create("employee", "Bob", "hr", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = catalog(&db, None).unwrap();
        let by_name: std::collections::HashMap<String, Value> = response["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap().to_owned(), e.clone()))
            .collect();

        assert_eq!(
            by_name["Alice"]["aliases"],
            serde_json::json!(["Alicia"]),
            "own name excluded: {response}"
        );
        assert_eq!(
            by_name["Bob"]["aliases"],
            serde_json::json!([]),
            "no aliases → present and empty: {response}"
        );
    }

    // ── search_entities_by_type ───────────────────────────────────────────

    #[test]
    fn by_type_missing_entity_type_is_an_error() {
        let db = test_util::in_memory_db();
        for args in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "entity_type": "" })),
        ] {
            let err = by_type(&db, args).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidArguments { .. }),
                "got: {err:?}"
            );
            assert!(
                err.to_string()
                    .contains("'entity_type' argument is required and must not be empty"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn by_type_valid_type_returns_entities() {
        let response = by_type(
            &seeded_entities_db(),
            Some(serde_json::json!({ "entity_type": "employee" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(2));
        assert_eq!(response["entities"].as_array().unwrap().len(), 2);
    }

    /// An unknown type is a valid empty page, not an error.
    #[test]
    fn by_type_nonexistent_type_is_empty_page() {
        let response = by_type(
            &seeded_entities_db(),
            Some(serde_json::json!({ "entity_type": "nonexistent_type_xyz" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["entities"], serde_json::json!([]));
        assert!(response.get("next_cursor").is_none(), "{response}");
    }

    /// The response carries the frozen fields for a matching entity.
    #[test]
    fn by_type_response_fields() {
        let response = by_type(
            &seeded_entities_db(),
            Some(serde_json::json!({ "entity_type": "policy" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(response["entities"][0]["name"], "NDA");
        assert_eq!(response["entities"][0]["type"], "policy");
        assert_eq!(response["entities"][0]["domain"], "hr");
    }

    /// 5 employees, page size 2 → 2/2/1 pages, no overlap, no cursor on the
    /// last page.
    #[test]
    fn by_type_pagination_walk() {
        let db = seeded_n_employees_db(5);
        let args = |cursor: Option<&str>| {
            let mut value = serde_json::json!({
                "entity_type": "employee",
                "page_size": 2
            });
            if let Some(cursor) = cursor {
                value["cursor"] = serde_json::json!(cursor);
            }
            value
        };

        let first = by_type(&db, Some(args(None))).unwrap();
        assert_eq!(first["entities"].as_array().unwrap().len(), 2);
        assert_eq!(first["total_count"], serde_json::json!(5));
        assert!(first["next_cursor"].is_string(), "{first}");

        let second = by_type(
            &db,
            Some(args(Some(first["next_cursor"].as_str().unwrap()))),
        )
        .unwrap();
        assert_eq!(second["entities"].as_array().unwrap().len(), 2);

        // No overlap between pages.
        let names = |page: &Value| {
            page["entities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_owned())
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(
            names(&first).is_disjoint(&names(&second)),
            "pages must not overlap"
        );

        let third = by_type(
            &db,
            Some(args(Some(second["next_cursor"].as_str().unwrap()))),
        )
        .unwrap();
        assert_eq!(third["entities"].as_array().unwrap().len(), 1);
        assert!(
            third.get("next_cursor").is_none(),
            "no next_cursor on the last page: {third}"
        );
    }

    /// The domain filter combines with the required entity type.
    #[test]
    fn by_type_domain_filter() {
        let db = test_util::in_memory_db();
        db.exec_tx(|tx| -> Result<(), db::DbError> {
            let entities = EntityDao::new(ConnectionOrTx::Transaction(&*tx));
            entities.create("employee", "Alice", "hr", None, None, None)?;
            entities.create("employee", "Bob", "it", None, None, None)?;
            Ok(())
        })
        .expect("seed transaction commits");

        let response = by_type(
            &db,
            Some(serde_json::json!({ "entity_type": "employee", "domain": "hr" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(1));
        assert_eq!(response["entities"][0]["name"], "Alice");
    }

    /// An invalid cursor is a tool error.
    #[test]
    fn by_type_invalid_cursor_is_an_error() {
        let db = test_util::in_memory_db();
        let err = by_type(
            &db,
            Some(serde_json::json!({
                "entity_type": "employee",
                "cursor": "not-valid-base64!!!"
            })),
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::InvalidArguments { .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("invalid cursor"), "got: {err}");
    }

    #[test]
    fn by_type_empty_db_is_empty_page() {
        let response = by_type(
            &test_util::in_memory_db(),
            Some(serde_json::json!({ "entity_type": "employee" })),
        )
        .unwrap();
        assert_eq!(response["total_count"], serde_json::json!(0));
        assert_eq!(response["entities"], serde_json::json!([]));
        assert!(response.get("next_cursor").is_none(), "{response}");
    }
}
