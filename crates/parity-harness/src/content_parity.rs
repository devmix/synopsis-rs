//! Content-parity record/verify + normalization (parity-fixture-expansion D1/D4).
//!
//! Machine-checked **content** parity for the MCP tools: a tool response
//! recorded once from the Go oracle (`../synopsis`) is committed as a golden
//! JSON fixture, and the Rust tool response is verified against it with the
//! strict structural comparator [`crate::diff::json_diff`] **after** a
//! normalization step that strips volatile, implementation-defined fields.
//!
//! The module provides the mechanism (no product code, no frozen contract):
//!
//! - [`record_response`] — drive a running MCP server (the Go binary during the
//!   one-time recording step) with one tool + args and commit the response to a
//!   fixture file;
//! - [`load_fixture`] — read a committed fixture back;
//! - [`normalize`] — strip volatile fields per tool so `json_diff` focuses on
//!   contract-relevant content;
//! - [`assert_content_parity`] — normalize both sides and assert `json_diff`
//!   is empty (panics with the full diff list on a real divergence).
//!
//! Fixtures are stored **raw** (the oracle's exact response); normalization is
//! applied to BOTH the fixture and the live response at verify time, never to
//! the committed bytes.
//!
//! The stripped/sorted set is defined and documented per tool in
//! [`normalize`].

use std::path::Path;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value};

use crate::HarnessError;
use crate::diff::json_diff;
use crate::mcp_client::McpClient;

// ── record / load ───────────────────────────────────────────────────────────

/// Record a tool response from a running MCP server into a committed JSON
/// fixture (design D1, record step).
///
/// Connects an [`McpClient`] to `url`, calls `tool` with `args`, and writes the
/// oracle-shaped response payload (pretty-printed, keys sorted) to `out`,
/// creating parent directories as needed. Used by the one-time recording step
/// (task 1.3) against the **Go** server; the verify path never records.
///
/// `args` must be a JSON object; a non-object value is treated as no arguments.
/// Failures are typed: a transport/protocol problem, an unknown tool, or a
/// tool-level error result all surface as [`HarnessError`].
pub async fn record_response(
    url: &str,
    tool: &str,
    args: &Value,
    out: &Path,
) -> Result<(), HarnessError> {
    let mut client = McpClient::connect(url).await?;
    let arguments = args.as_object().cloned().unwrap_or_default();
    let result = client.call_tool(tool, arguments).await?;
    let payload = extract_payload(&result)?;
    client.close().await?;

    // `serde_json::Map` is a BTreeMap by default, so keys serialize in sorted
    // order; pretty-print for a reviewable, committed fixture (one trailing
    // newline).
    let rendered = serde_json::to_string_pretty(&payload).map_err(|err| HarnessError::Fixture {
        path: out.to_path_buf(),
        reason: format!("serialize: {err}"),
    })?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|err| HarnessError::Fixture {
            path: out.to_path_buf(),
            reason: format!("create parent directory: {err}"),
        })?;
    }
    std::fs::write(out, format!("{rendered}\n")).map_err(|err| HarnessError::Fixture {
        path: out.to_path_buf(),
        reason: format!("write: {err}"),
    })?;
    Ok(())
}

/// Load a committed JSON fixture from `path` (design D1, verify step).
///
/// Every failure — missing/unreadable file or invalid JSON — surfaces as
/// [`HarnessError::Fixture`] carrying the file path.
pub fn load_fixture(path: &Path) -> Result<Value, HarnessError> {
    let bytes = std::fs::read(path).map_err(|err| HarnessError::Fixture {
        path: path.to_path_buf(),
        reason: format!("read: {err}"),
    })?;
    serde_json::from_slice(&bytes).map_err(|err| HarnessError::Fixture {
        path: path.to_path_buf(),
        reason: format!("parse: {err}"),
    })
}

// ── normalization ───────────────────────────────────────────────────────────

/// Strip volatile, implementation-defined fields from a tool response before
/// `json_diff` (design D4). Applied to **both** the fixture and the live
/// response.
///
/// The stripped/sorted set is minimal and per-tool, documented here:
///
/// - **`search`** — stripped: top-level `search_time_ms` (wall-clock duration);
///   per result, `score` (implementation-defined float) and every non-identity
///   field (`text`, `sequence_num`, `start_offset`, `end_offset`,
///   `document_path`, `source_type`, `domains`, `entities`). Kept: `total_count`,
///   `warning`, and the per-result identity (`document_id`, `chunk_id`) in rank
///   order. Rank order IS contract-relevant, so the `results` array is NOT
///   re-sorted — only its non-identity fields are dropped.
/// - **`catalog_overview`** — stripped: none (all fields are deterministic
///   counts/maps). Sorted: `domains` and `entity_types` (both sets; order is not
///   contract-relevant). Kept: every count and the `*_by_type` / `*_by_domain`
///   maps.
/// - **`catalog_documents`** — stripped: per document `created_at` / `updated_at`
///   (timestamps). Sorted: the `documents` page by `id` (stable key) and each
///   document's `domain` array. Kept: `id`, `source_type`, `original_path`,
///   `domain`, `metadata`, `total_count`, `next_cursor`.
/// - **`catalog_entities`** — stripped: per entity `confidence`
///   (implementation-defined float). Sorted: the `entities` page by `id`
///   (stable key). Kept: `id`, `name`, `type`, `domain`, `description`,
///   `metadata`, `total_count`, `next_cursor`.
///
/// An unknown tool is returned unchanged (no normalization).
pub fn normalize(value: Value, tool: &str) -> Value {
    match tool {
        "search" => normalize_search(value),
        "catalog_overview" => normalize_catalog_overview(value),
        "catalog_documents" => normalize_catalog_documents(value),
        "catalog_entities" => normalize_catalog_entities(value),
        _ => value,
    }
}

/// `search`: strip the duration and reduce each result to its identity fields,
/// keeping rank order (design D4).
fn normalize_search(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    // Wall-clock duration: environment-defined, never comparable.
    obj.remove("search_time_ms");
    // Rank order is contract-relevant: keep the array order, but compare only
    // the identity fields (doc/chunk id) of each result.
    if let Some(Value::Array(results_ref)) = obj.get_mut("results") {
        let results = std::mem::take(results_ref);
        let identity = results
            .into_iter()
            .map(|item| {
                let Value::Object(entry) = item else {
                    return item;
                };
                let mut out = Map::new();
                for key in ["document_id", "chunk_id"] {
                    if let Some(field) = entry.get(key) {
                        out.insert(key.to_owned(), field.clone());
                    }
                }
                Value::Object(out)
            })
            .collect();
        *results_ref = identity;
    }
    Value::Object(obj)
}

/// `catalog_overview`: sort the two set-valued list fields (design D4).
fn normalize_catalog_overview(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    sort_string_array_in_place(&mut obj, "domains");
    sort_string_array_in_place(&mut obj, "entity_types");
    Value::Object(obj)
}

/// `catalog_documents`: strip per-document timestamps and order the page by the
/// stable `id` key (design D4).
fn normalize_catalog_documents(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    if let Some(Value::Array(docs_ref)) = obj.get_mut("documents") {
        let docs = std::mem::take(docs_ref);
        let mut kept = docs
            .into_iter()
            .map(|doc| {
                let Value::Object(mut entry) = doc else {
                    return doc;
                };
                // Timestamps: environment-defined, never comparable.
                entry.remove("created_at");
                entry.remove("updated_at");
                sort_string_array_in_place(&mut entry, "domain");
                Value::Object(entry)
            })
            .collect::<Vec<_>>();
        kept.sort_by_key(row_id);
        *docs_ref = kept;
    }
    Value::Object(obj)
}

/// `catalog_entities`: strip per-entity confidence and order the page by the
/// stable `id` key (design D4).
fn normalize_catalog_entities(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    if let Some(Value::Array(entities_ref)) = obj.get_mut("entities") {
        let entities = std::mem::take(entities_ref);
        let mut kept = entities
            .into_iter()
            .map(|entity| {
                let Value::Object(mut entry) = entity else {
                    return entity;
                };
                // Extraction confidence: implementation-defined float.
                entry.remove("confidence");
                Value::Object(entry)
            })
            .collect::<Vec<_>>();
        kept.sort_by_key(row_id);
        *entities_ref = kept;
    }
    Value::Object(obj)
}

/// Sort a top-level array-of-strings field in place; a no-op when the field is
/// absent, not an array, or holds a non-string member (left untouched).
fn sort_string_array_in_place(obj: &mut Map<String, Value>, key: &str) {
    let Some(Value::Array(items)) = obj.get_mut(key) else {
        return;
    };
    let mut strings: Vec<String> = Vec::with_capacity(items.len());
    for item in items.iter() {
        match item.as_str() {
            Some(text) => strings.push(text.to_owned()),
            None => return,
        }
    }
    strings.sort();
    *items = strings.into_iter().map(Value::String).collect();
}

/// The integer `id` of a row object, used as a stable sort key (0 when the
/// field is absent or not an integer — the catalog rows always carry an id).
fn row_id(row: &Value) -> i64 {
    row.get("id").and_then(Value::as_i64).unwrap_or(0)
}

// ── verify ──────────────────────────────────────────────────────────────────

/// Assert content parity between a recorded fixture and a live response
/// (design D1, verify step).
///
/// Both values are normalized for `tool` (see [`normalize`]), then compared
/// with [`json_diff`]. Panics with the full diff list when the normalized
/// values diverge — the intended signal of a real behavior divergence.
pub fn assert_content_parity(fixture: &Value, actual: &Value, tool: &str) {
    let expected = normalize(fixture.clone(), tool);
    let got = normalize(actual.clone(), tool);
    let diffs = json_diff(&expected, &got);
    assert!(
        diffs.is_empty(),
        "content parity for `{tool}` diverged ({} difference(s)):\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// The tool's oracle-shaped JSON payload, parsed from the first text content
/// block of a `tools/call` result.
fn extract_payload(result: &CallToolResult) -> Result<Value, HarnessError> {
    let text = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .ok_or_else(|| HarnessError::McpService {
            detail: "tool result has no text content".to_owned(),
        })?;
    serde_json::from_str(&text.text).map_err(|err| HarnessError::McpService {
        detail: format!("tool result is not valid JSON: {err}"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    // ── normalize: search ──────────────────────────────────────────────────

    #[test]
    fn search_strips_score_and_keeps_identity_in_rank_order() {
        let value = json!({
            "results": [
                {
                    "document_id": 10,
                    "chunk_id": 1,
                    "text": "a",
                    "score": 0.9,
                    "source_type": "hybrid",
                    "sequence_num": 0
                },
                {
                    "document_id": 20,
                    "chunk_id": 2,
                    "text": "b",
                    "score": 0.5,
                    "source_type": "lexical"
                }
            ],
            "total_count": 2,
            "search_time_ms": 42
        });
        let normalized = normalize(value, "search");

        // Duration stripped; total_count kept.
        assert!(normalized.get("search_time_ms").is_none());
        assert_eq!(normalized["total_count"], json!(2));

        // Rank order preserved; each result reduced to its identity fields.
        let results = normalized["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["document_id"], json!(10));
        assert_eq!(results[0]["chunk_id"], json!(1));
        assert!(results[0].get("score").is_none(), "score must be stripped");
        assert!(
            results[0].get("text").is_none(),
            "non-identity fields must be stripped"
        );
        assert_eq!(results[1]["document_id"], json!(20));
        assert_eq!(results[1]["chunk_id"], json!(2));
    }

    // ── normalize: unordered arrays sorted ─────────────────────────────────

    #[test]
    fn catalog_overview_sorts_set_lists() {
        let value = json!({
            "document_count": 8,
            "domains": ["product", "hr", "engineering"],
            "entity_types": ["policy", "employee"]
        });
        let normalized = normalize(value, "catalog_overview");
        assert_eq!(
            normalized["domains"],
            json!(["engineering", "hr", "product"])
        );
        assert_eq!(normalized["entity_types"], json!(["employee", "policy"]));
        // Counts untouched.
        assert_eq!(normalized["document_count"], json!(8));
    }

    #[test]
    fn catalog_entities_sorts_by_id_and_strips_confidence() {
        let value = json!({
            "entities": [
                { "id": 2, "name": "Bob", "type": "employee", "confidence": 0.7 },
                { "id": 1, "name": "Alice", "type": "employee", "confidence": 0.9 }
            ],
            "total_count": 2
        });
        let normalized = normalize(value, "catalog_entities");

        let entities = normalized["entities"].as_array().unwrap();
        // Ordered by id (1 before 2); confidence stripped.
        assert_eq!(entities[0]["id"], json!(1));
        assert_eq!(entities[0]["name"], "Alice");
        assert!(
            entities[0].get("confidence").is_none(),
            "confidence must be stripped"
        );
        assert_eq!(entities[1]["id"], json!(2));
        assert_eq!(entities[1]["name"], "Bob");
        assert_eq!(normalized["total_count"], json!(2));
    }

    #[test]
    fn catalog_documents_strips_timestamps_and_sorts_by_id() {
        let value = json!({
            "documents": [
                {
                    "id": 2,
                    "original_path": "/b.md",
                    "domain": ["hr", "policy"],
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-02T00:00:00Z"
                },
                {
                    "id": 1,
                    "original_path": "/a.md",
                    "domain": ["eng"],
                    "created_at": "2026-01-03T00:00:00Z",
                    "updated_at": "2026-01-04T00:00:00Z"
                }
            ],
            "total_count": 2
        });
        let normalized = normalize(value, "catalog_documents");

        let docs = normalized["documents"].as_array().unwrap();
        // Ordered by id (1 before 2); timestamps stripped.
        assert_eq!(docs[0]["id"], json!(1));
        assert_eq!(docs[0]["original_path"], "/a.md");
        assert!(
            docs[0].get("created_at").is_none(),
            "created_at must be stripped"
        );
        assert!(
            docs[0].get("updated_at").is_none(),
            "updated_at must be stripped"
        );
        assert_eq!(docs[1]["id"], json!(2));
        assert_eq!(docs[1]["domain"], json!(["hr", "policy"]));
        assert_eq!(normalized["total_count"], json!(2));
    }

    #[test]
    fn unknown_tool_is_returned_unchanged() {
        let value = json!({ "score": 1.0, "created_at": "t" });
        assert_eq!(normalize(value.clone(), "some_other_tool"), value);
    }

    #[test]
    fn non_object_value_is_returned_unchanged() {
        let value = json!(42);
        assert_eq!(normalize(value.clone(), "search"), value);
    }

    // ── assert_content_parity ──────────────────────────────────────────────

    #[test]
    fn parity_passes_when_equal_after_normalization() {
        // The two inputs differ only in volatile fields (exact scores and the
        // wall-clock duration) — after normalization they must compare equal.
        let fixture = json!({
            "results": [
                { "document_id": 10, "chunk_id": 1, "score": 0.9 },
                { "document_id": 20, "chunk_id": 2, "score": 0.5 }
            ],
            "total_count": 2,
            "search_time_ms": 100
        });
        let actual = json!({
            "results": [
                { "document_id": 10, "chunk_id": 1, "score": 0.9000001 },
                { "document_id": 20, "chunk_id": 2, "score": 0.4999999 }
            ],
            "total_count": 2,
            "search_time_ms": 37
        });
        // Must not panic.
        assert_content_parity(&fixture, &actual, "search");
    }

    #[test]
    #[should_panic(expected = "diverged")]
    fn parity_fails_on_real_identity_divergence() {
        // A real divergence: the second-ranked chunk differs in identity (not
        // just score) — normalization must NOT hide it.
        let fixture = json!({
            "results": [
                { "document_id": 10, "chunk_id": 1 },
                { "document_id": 20, "chunk_id": 2 }
            ],
            "total_count": 2
        });
        let actual = json!({
            "results": [
                { "document_id": 10, "chunk_id": 1 },
                { "document_id": 99, "chunk_id": 7 }
            ],
            "total_count": 2
        });
        assert_content_parity(&fixture, &actual, "search");
    }
}
