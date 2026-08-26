//! Cursor-based pagination shared by the paginated catalog tools
//! (`catalog_documents`, `catalog_entities`, `search_entities_by_type`,
//! `search_facts`).
//!
//! Oracle mapping: `../synopsis/internal/mcp/handlers/pagination.go`.
//!
//! The cursor is an opaque base64 string carrying the `{offset, limit}`
//! pair the db crate's offset-based `list_paginated` DAOs consume. The
//! wire format is byte-compatible with the Go oracle (base64 of
//! `{"offset":N,"limit":M}`), so a cursor issued by either implementation
//! decodes identically in the other.
//!
//! **Recorded deviation:** design.md D3 describes the cursor as "base64 of
//! the last-seen sort key", but the oracle's `pagination.go` — the file
//! this task ports — encodes the offset/limit pair, and the db crate's
//! `list_paginated` DAOs (frozen by the db change) are offset-based.
//! Keyset pagination would require new DAO methods outside this task's
//! scope; the oracle's wire format wins (the frozen contract requires
//! response parity with the Go binary, including cursor strings).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default number of items per page (oracle `DefaultPageSize`).
pub const DEFAULT_PAGE_SIZE: i64 = 20;
/// Minimum allowed page size (oracle `MinPageSize`).
pub const MIN_PAGE_SIZE: i64 = 1;
/// Maximum allowed page size (oracle `MaxPageSize`).
pub const MAX_PAGE_SIZE: i64 = 200;

/// Cursor decode failure (oracle `DecodeCursor` error paths).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CursorError {
    /// The cursor is not valid standard base64.
    #[error("cursor is not valid base64")]
    InvalidBase64,
    /// The decoded payload is not the expected `{"offset":N,"limit":M}` JSON object.
    #[error("cursor payload is not valid JSON")]
    MalformedPayload,
}

/// The cursor wire format (oracle `Cursor` struct): base64 of
/// `{"offset":N,"limit":M}`. Field order matches Go's `json.Marshal`
/// output; `#[serde(default)]` mirrors Go's `json.Unmarshal` zero values
/// for missing fields.
#[derive(Debug, Serialize, Deserialize)]
struct CursorPayload {
    /// Rows to skip before the page.
    #[serde(default)]
    offset: i64,
    /// Rows to return.
    #[serde(default)]
    limit: i64,
}

/// A decoded page window: how many rows to skip and how many to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Rows to skip before the page (always `>= 0`).
    pub offset: i64,
    /// Rows to return (within [`MIN_PAGE_SIZE`]..=[`MAX_PAGE_SIZE`]).
    pub limit: i64,
}

impl Page {
    /// The first page: no rows skipped, `limit` rows returned.
    pub fn first(limit: i64) -> Self {
        Self { offset: 0, limit }
    }

    /// The cursor for the page following this one, or `None` when no more
    /// rows exist (`offset + limit >= total`) — the oracle's rule that
    /// `next_cursor` is present only when more rows exist.
    pub fn next_cursor(&self, total: i64) -> Option<String> {
        (self.offset + self.limit < total)
            .then(|| encode_cursor(self.offset + self.limit, self.limit))
    }
}

/// Decode a cursor string into a page window (oracle `DecodeCursor`).
///
/// An empty string is the "start from the beginning" sentinel: the first
/// page with [`DEFAULT_PAGE_SIZE`] rows.
///
/// A negative offset (only possible in a hand-crafted cursor) is clamped
/// to zero — a robustness fix over the oracle, which passed the raw value
/// through to SQLite's `OFFSET` clause.
pub fn decode_cursor(cursor: &str) -> Result<Page, CursorError> {
    if cursor.is_empty() {
        return Ok(Page::first(DEFAULT_PAGE_SIZE));
    }
    let bytes = STANDARD
        .decode(cursor)
        .map_err(|_| CursorError::InvalidBase64)?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| CursorError::MalformedPayload)?;
    Ok(Page {
        offset: payload.offset.max(0),
        limit: normalize_page_size(payload.limit),
    })
}

/// Clamp a page size to [`MIN_PAGE_SIZE`]..=[`MAX_PAGE_SIZE`]; an
/// out-of-range value falls back to [`DEFAULT_PAGE_SIZE`] (oracle
/// `NormalizePageSize`).
pub fn normalize_page_size(size: i64) -> i64 {
    if (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&size) {
        size
    } else {
        DEFAULT_PAGE_SIZE
    }
}

/// Encode the `{offset, limit}` pair as the opaque cursor string (oracle
/// `EncodeCursor`). The JSON payload can never fail to serialize (two i64
/// fields), so the fallback only keeps the no-panic rule (design D7).
fn encode_cursor(offset: i64, limit: i64) -> String {
    let json = serde_json::to_string(&CursorPayload { offset, limit })
        .unwrap_or_else(|_| r#"{"offset":0,"limit":20}"#.to_owned());
    STANDARD.encode(json)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn empty_cursor_starts_from_the_beginning() {
        assert_eq!(decode_cursor("").unwrap(), Page::first(DEFAULT_PAGE_SIZE));
    }

    #[test]
    fn cursor_roundtrip_preserves_offset_and_limit() {
        let cursor = encode_cursor(40, 50);
        let page = decode_cursor(&cursor).unwrap();
        assert_eq!(
            page,
            Page {
                offset: 40,
                limit: 50
            }
        );
        // Encoding is total and deterministic.
        assert_eq!(encode_cursor(40, 50), cursor);
    }

    /// The wire format is byte-compatible with the Go oracle's
    /// `EncodeCursor` (base64 of `{"offset":N,"limit":M}`, standard
    /// alphabet with padding).
    #[test]
    fn wire_format_matches_the_go_oracle() {
        assert_eq!(encode_cursor(20, 20), "eyJvZmZzZXQiOjIwLCJsaW1pdCI6MjB9");
        assert_eq!(encode_cursor(40, 5), "eyJvZmZzZXQiOjQwLCJsaW1pdCI6NX0=");
        // A cursor produced by the Go oracle decodes to the same page.
        let page = decode_cursor("eyJvZmZzZXQiOjIwLCJsaW1pdCI6MjB9").unwrap();
        assert_eq!(
            page,
            Page {
                offset: 20,
                limit: 20
            }
        );
    }

    #[test]
    fn next_cursor_is_present_only_when_more_rows_exist() {
        let first = Page::first(20);
        assert_eq!(first.next_cursor(45), Some(encode_cursor(20, 20)));
        assert_eq!(first.next_cursor(21), Some(encode_cursor(20, 20)));
        assert_eq!(first.next_cursor(20), None); // exact fit: last page
        assert_eq!(first.next_cursor(19), None); // fewer rows than a page
        assert_eq!(
            Page::first(50).next_cursor(120),
            Some(encode_cursor(50, 50))
        );

        let last = Page {
            offset: 40,
            limit: 20,
        };
        assert_eq!(last.next_cursor(45), None); // 40 + 20 >= 45
        assert_eq!(last.next_cursor(60), None);
    }

    /// Walking pages from the first cursor to the last covers every row
    /// and stops exactly one page past the end (oracle handler walk).
    #[test]
    fn pagination_walk_covers_all_rows() {
        let total = 45;
        let mut page = Page::first(20);
        let mut offset = 0;
        loop {
            let cursor = page.next_cursor(total);
            assert_eq!(page.offset, offset, "cursor must carry the running offset");
            offset += page.limit;
            match cursor {
                Some(cursor) => page = decode_cursor(&cursor).unwrap(),
                None => break,
            }
        }
        assert!(offset >= total, "walk must pass the end: {offset}");
        assert!(
            offset <= total + 20,
            "walk must stop within one page of the end: {offset}"
        );
    }

    #[test]
    fn normalize_page_size_clamps_out_of_range_to_default() {
        for size in [0i64, -1, 201, 1_000_000] {
            assert_eq!(normalize_page_size(size), DEFAULT_PAGE_SIZE, "{size}");
        }
        for size in [1i64, 20, 100, 200] {
            assert_eq!(normalize_page_size(size), size, "{size}");
        }
    }

    /// Oracle `DecodeCursor`: the limit carried in the cursor is
    /// re-normalized on decode, not trusted as-is.
    #[test]
    fn out_of_range_limit_in_cursor_is_normalized() {
        assert_eq!(
            decode_cursor(&encode_cursor(0, 0)).unwrap(),
            Page::first(DEFAULT_PAGE_SIZE)
        );
        assert_eq!(
            decode_cursor(&encode_cursor(10, 500)).unwrap(),
            Page {
                offset: 10,
                limit: DEFAULT_PAGE_SIZE
            }
        );
    }

    /// Robustness fix over the oracle: a hand-crafted cursor with a
    /// negative offset clamps to the first page instead of reaching
    /// SQLite's `OFFSET` clause.
    #[test]
    fn negative_offset_is_clamped_to_zero() {
        assert_eq!(
            decode_cursor(&encode_cursor(-5, 20)).unwrap(),
            Page::first(20)
        );
    }

    #[test]
    fn invalid_base64_is_rejected() {
        for cursor in ["not-base64!!!", "!!!", "a b c"] {
            assert_eq!(
                decode_cursor(cursor).unwrap_err(),
                CursorError::InvalidBase64,
                "{cursor}"
            );
        }
    }

    #[test]
    fn malformed_payload_is_rejected() {
        // Valid base64, but not the cursor JSON object.
        for payload in ["[1,2,3]", "\"offset\"", "null", "42"] {
            let cursor = STANDARD.encode(payload.as_bytes());
            assert_eq!(
                decode_cursor(&cursor).unwrap_err(),
                CursorError::MalformedPayload,
                "{payload}"
            );
        }
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // Oracle json.Unmarshal zero values: offset 0, limit 0 → default.
        let cursor = STANDARD.encode(r#"{}"#.as_bytes());
        assert_eq!(
            decode_cursor(&cursor).unwrap(),
            Page::first(DEFAULT_PAGE_SIZE)
        );
    }
}
