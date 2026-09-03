//! Pure helpers of the per-document ingestion pipeline.
//!
//! All functions are pure and rune-aware: multi-byte scripts (Cyrillic, CJK,
//! emoji) are indexed and windowed by `char`, so they behave identically to
//! single-byte text (task 3.3, design D7).
//!
//! Design: case-insensitive matching uses Rust's `char::to_lowercase` (full
//! Unicode case mapping); for the rare special-casing runes (e.g. `İ`) this
//! differs from a simple fold, and the Unicode-standard mapping is the more
//! correct one.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Rune window kept on each side of a matched entity name inside a quote.
const QUOTE_WINDOW: usize = 60;
/// Fallback quote length in runes when neither entity name is found.
const QUOTE_FALLBACK_RUNES: usize = 120;

/// Hex-encoded SHA-256 of the given content.
///
/// The hash is the document's dedup key: unchanged content yields an
/// unchanged hash, so a re-ingestion skips the document (task 3.4).
pub fn compute_content_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Extracts a text window around the first occurrence of `subject_name` or
/// `object_name` (case-insensitive, earliest rune index wins) from
/// `chunk_text`. If neither name is found, returns the first ~120 runes as
/// fallback. Empty input yields `""`. When the quote is truncated it is
/// trimmed to a line boundary and `...` is appended.
///
/// All indices are rune (char) indices (design D7).
pub fn extract_quote_from_chunk(chunk_text: &str, subject_name: &str, object_name: &str) -> String {
    if chunk_text.is_empty() {
        return String::new();
    }

    let chunk: Vec<char> = chunk_text.chars().collect();
    let text_len = chunk.len();

    // (rune index of first occurrence, rune length of the name)
    let mut best: Option<(usize, usize)> = None;
    for name in [subject_name, object_name] {
        let Some(idx) = first_occurrence(chunk_text, name) else {
            continue;
        };
        if best.is_none_or(|(best_idx, _)| idx < best_idx) {
            best = Some((idx, name.chars().count()));
        }
    }

    let (quote, truncated) = match best {
        Some((idx, name_len)) => {
            let start = idx.saturating_sub(QUOTE_WINDOW);
            let end = (idx + name_len + QUOTE_WINDOW).min(text_len);
            let quote: String = chunk[start..end].iter().collect();
            let truncated = start > 0 || end < text_len;
            (quote, truncated)
        }
        None => {
            // Fallback: first ~120 runes; chunks that short are returned whole.
            if text_len <= QUOTE_FALLBACK_RUNES {
                return chunk_text.to_owned();
            }
            let quote: String = chunk[..QUOTE_FALLBACK_RUNES].iter().collect();
            (quote, true)
        }
    };

    let quote = if truncated {
        trim_to_line_boundary(&quote)
    } else {
        quote
    };
    if truncated && !quote.ends_with("...") {
        format!("{quote}...")
    } else {
        quote
    }
}

/// Resolves the `source_type` field of a document metadata map.
///
/// A missing, empty or non-string value yields `"unknown"`.
pub fn source_type_from_metadata(metadata: &Map<String, Value>) -> String {
    metadata
        .get("source_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| "unknown".to_owned(), str::to_owned)
}

/// Case-insensitive search of `name` in `text`; returns the rune index of
/// the first occurrence. Empty names never match.
fn first_occurrence(text: &str, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let (lower_text, rune_map) = lowered_with_rune_map(text);
    let lower_name = name.to_lowercase();
    let byte_idx = lower_text.find(lower_name.as_str())?;
    let lower_rune_idx = lower_text[..byte_idx].chars().count();
    Some(rune_map[lower_rune_idx])
}

/// Lowercases `text` while recording, for every produced rune, the index of
/// the original rune it came from. Expanding case mappings (e.g. `İ` →
/// `i` + combining dot) keep the mapping monotone, so a leftmost match in
/// the lowered text maps back to the leftmost match in the original.
fn lowered_with_rune_map(text: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(text.len());
    let mut rune_map = Vec::new();
    for (orig_idx, ch) in text.chars().enumerate() {
        for mapped in ch.to_lowercase() {
            lower.push(mapped);
            rune_map.push(orig_idx);
        }
    }
    (lower, rune_map)
}

/// Trims the text at the last newline (`\n` or `\r`) boundary so quotes are
/// never cut mid-line, then trims the resulting whitespace. If no newline
/// is present, returns the text unchanged.
fn trim_to_line_boundary(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    // `rposition` yields the from-the-front index of the last newline.
    let Some(idx) = chars.iter().rposition(|ch| *ch == '\n' || *ch == '\r') else {
        return text.to_owned();
    };
    let cut: String = chars[..idx].iter().collect();
    cut.trim().to_owned()
}

#[cfg(test)]
mod tests {
    //! Quote-extraction cases plus hash determinism, `source_type`
    //! resolution and rune-window cases.

    use super::*;

    // ── Entity found in chunk text ────────────────────────────────────

    #[test]
    fn entity_found_in_chunk_text() {
        let cases = [
            (
                "subject_found_earlier_than_object",
                "Alice works at Acme Corp as a developer.",
                "Alice",
                "Acme Corp",
                "Alice",
            ),
            (
                "object_found_earlier_than_subject",
                "The Acme Corp hired Alice for the position.",
                "Alice",
                "Acme Corp",
                "Acme Corp",
            ),
            (
                "case_insensitive_match",
                "alice works at acme corp.",
                "Alice",
                "Acme Corp",
                "alice",
            ),
            (
                "quote_is_not_predicate",
                "Alice works at Acme Corp.",
                "Alice",
                "Acme Corp",
                "Alice",
            ),
        ];
        for (name, chunk_text, subject, object, want_contains) in cases {
            let quote = extract_quote_from_chunk(chunk_text, subject, object);
            assert!(
                quote.contains(want_contains),
                "{name}: quote {quote:?} must contain {want_contains:?}"
            );
            // The predicate must never leak in as the quote.
            assert_ne!(quote, "works_at", "{name}: quote must not be the predicate");
        }
    }

    // ── Neither entity found ──────────────────────────────────────────

    #[test]
    fn neither_entity_found_falls_back_to_prefix() {
        let chunk_text =
            "This document discusses general topics without mentioning specific people.";
        let quote = extract_quote_from_chunk(chunk_text, "Alice", "Acme Corp");
        let expected: String = chunk_text.chars().take(QUOTE_FALLBACK_RUNES).collect();
        assert_eq!(quote, expected);
    }

    #[test]
    fn empty_chunk_returns_empty() {
        assert_eq!(extract_quote_from_chunk("", "Alice", "Acme Corp"), "");
    }

    // ── Window bounds ─────────────────────────────────────────────────

    #[test]
    fn entity_at_start_no_negative_offset() {
        let quote = extract_quote_from_chunk("Alice is the CEO.", "Alice", "");
        assert!(quote.starts_with("Alice"), "quote: {quote:?}");
    }

    #[test]
    fn entity_at_end_no_overflow() {
        let chunk_text = "The CEO is Alice.";
        let quote = extract_quote_from_chunk(chunk_text, "Alice", "");
        assert!(quote.contains("Alice"), "quote: {quote:?}");
        assert!(
            quote.chars().count() <= chunk_text.chars().count(),
            "quote longer than chunk text"
        );
    }

    // ── Line-boundary truncation ──────────────────────────────────────

    #[test]
    fn line_boundary_truncation() {
        // Constructed inputs are hoisted so the cases table stays `&str`-based.
        let fallback_text = "word ".repeat(100);
        let multiline_text = format!(
            "Line one.\nLine two with Alice in it. {}{}",
            "extra words ".repeat(30),
            "\nLine three continues after the match window with more text that pushes beyond the quote extraction limit.",
        );
        let cases = [
            (
                "truncated_quote_ends_with_ellipsis",
                "This is a very long paragraph that contains Alice somewhere in the middle of the text and continues for many more words beyond the window size limit.",
                "Alice",
                "",
                true,
            ),
            (
                "fallback_truncated_ends_with_ellipsis",
                fallback_text.as_str(),
                "Nobody",
                "",
                true,
            ),
            (
                "short_text_no_truncation",
                "Alice works here.",
                "Alice",
                "",
                false,
            ),
            (
                "multiline_trimmed_to_line_boundary",
                multiline_text.as_str(),
                "Alice",
                "",
                true,
            ),
        ];
        for (name, chunk_text, subject, object, want_suffix) in cases {
            let quote = extract_quote_from_chunk(chunk_text, subject, object);
            if want_suffix {
                assert!(quote.ends_with("..."), "{name}: {quote:?}");
            } else {
                assert!(!quote.ends_with("..."), "{name}: {quote:?}");
            }
        }
    }

    // ── Mid-line trimmed ──────────────────────────────────────────────

    #[test]
    fn mid_line_trimmed() {
        let chunk_text = "Alice works at Acme Corp.\n\nThis is the second paragraph with more details about the company.";
        let quote = extract_quote_from_chunk(chunk_text, "Alice", "");
        assert!(quote.contains("Alice"), "quote: {quote:?}");
        // Truncated: the last line before the ellipsis must be a complete
        // line (asserted here).
        let clean = quote.strip_suffix("...").unwrap_or(&quote);
        let last_line = clean.split('\n').next_back().unwrap_or("");
        assert_eq!(last_line, "Alice works at Acme Corp.");
    }

    // ── Rune-awareness (multi-byte cases) ─────────────────────────────

    #[test]
    fn cyrillic_rune_window() {
        // Multi-byte runes: the window must count runes, not bytes — a
        // byte-based window would land mid-rune and cut a different span.
        let prefix = "я".repeat(50); // 50 runes, 100 bytes
        let chunk_text = format!("{prefix}Иван работает над проектом.\n{}", "б".repeat(200));
        let quote = extract_quote_from_chunk(&chunk_text, "Иван", "");
        // Match at rune 50, name length 4: window end = 114 > line end 77,
        // so the quote is truncated and trimmed to the first line boundary.
        assert_eq!(quote, format!("{prefix}Иван работает над проектом...."));
    }

    #[test]
    fn cyrillic_fallback_first_120_runes() {
        let chunk_text = format!("{}\n{}", "с".repeat(150), "д".repeat(50));
        let quote = extract_quote_from_chunk(&chunk_text, "Никто", "");
        // Fallback: first 120 runes; the only newline sits past rune 150,
        // so no line-boundary trim applies and the ellipsis is appended.
        assert_eq!(quote, "с".repeat(120) + "...");
    }

    #[test]
    fn cyrillic_case_insensitive_match() {
        let quote = extract_quote_from_chunk("иванов работает в векторе.", "Иванов", "");
        assert!(quote.contains("иванов"), "quote: {quote:?}");
    }

    // ── compute_content_hash ────────────────────────────────────────────

    #[test]
    fn content_hash_is_sha256_hex() {
        assert_eq!(
            compute_content_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            compute_content_hash("hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn content_hash_is_deterministic() {
        let content = "Привет, мир!\nsecond line";
        let first = compute_content_hash(content);
        let second = compute_content_hash(content);
        assert_eq!(first, second);
        assert_eq!(first.len(), 64, "hex sha256 is 64 chars");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex: {first:?}"
        );
        assert_ne!(first, compute_content_hash("Привет, мир!\nsecond line "));
    }

    // ── source_type_from_metadata ───────────────────────────────────────

    #[test]
    fn source_type_resolution() {
        let mut metadata = Map::new();
        assert_eq!(source_type_from_metadata(&metadata), "unknown");

        metadata.insert(
            "source_type".to_owned(),
            Value::String("markdown".to_owned()),
        );
        assert_eq!(source_type_from_metadata(&metadata), "markdown");

        metadata.insert("source_type".to_owned(), Value::String(String::new()));
        assert_eq!(source_type_from_metadata(&metadata), "unknown");

        metadata.insert("source_type".to_owned(), Value::Number(42.into()));
        assert_eq!(source_type_from_metadata(&metadata), "unknown");

        // Whitespace is not empty, so it is kept.
        metadata.insert("source_type".to_owned(), Value::String(" ".to_owned()));
        assert_eq!(source_type_from_metadata(&metadata), " ");
    }
}
