//! JSON chunker (oracle: `internal/ingestion/chunkers/json_chunker.go` and
//! its tests).
//!
//! Splits a JSON document at its structural unit: one chunk per object —
//! each top-level element of an array document, or the whole value of a
//! single-object document. Configuration arrives at construction from
//! [`config::preset::JsonChunkerConfig`] (`chunking.json.*`): `text_fields`
//! (an empty list falls back to the oracle's four defaults — the config
//! crate's `apply_defaults` normally already applied them),
//! `combine_fields` (one chunk per object vs. one chunk per text field) and
//! `max_objects` (0 = unlimited).
//!
//! **Deliberate deviations from the oracle** (functional copy, not code copy):
//!
//! * **Byte-offset invariant (crate contract).** The oracle synthesized the
//!   chunk text as `**field**: value` markdown joined with blank lines — not
//!   a slice of the file, so its `StartOffset`/`EndOffset` could never point
//!   at the text. Here the chunk text is the object's (or field value's) raw
//!   JSON: a pure slice of the content, `content[start..end] == text`. The
//!   field names the oracle embedded in the text live in the chunk metadata
//!   instead (`text_fields` in combined mode, `field_name` in per-field
//!   mode), and non-text fields stay inside the raw object rather than being
//!   dropped from the indexed content.
//! * **Scalar documents produce one chunk.** The oracle hard-errored on any
//!   value that is neither an array nor an object (`42`, `"text"`, `null`),
//!   while the parser (task 1.4) deliberately ingests valid scalars with
//!   structure `"unknown"`. Erroring later in the chunker would re-create the
//!   bug the parser deviation removed: a validated file aborting the pipeline
//!   at a stage that cannot name the cause. A scalar has no internal
//!   structure to split, so the whole content is the chunk.
//! * **`sequence_num`** is the chunk's position in the returned slice (the
//!   oracle numbered chunks by object index, which collided for per-field
//!   chunks of one object and left gaps at skipped objects). The object index
//!   stays in the metadata (`object_index`).
//! * **Deterministic field order.** The oracle's combined-mode fallback
//!   iterated Go map order (random per run); Rust iterates the BTreeMap-backed
//!   [`serde_json::Map`] in key order.
//!
//! The serde parse in [`Chunker::chunk`] also guards the chunker when it is
//! called with content that bypassed the parser: syntactically invalid JSON
//! is an [`IngestionError::Json`], matching the oracle's error for such input
//! (in the pipeline the parser rejects it up front instead).

use std::path::PathBuf;

use config::preset::JsonChunkerConfig;
use serde_json::{Map, Value};

use crate::error::IngestionError;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata};

/// JSON chunker.
///
/// See the module docs for the splitting semantics and the deviations from
/// the oracle. Stateless after construction: [`Chunker::chunk`] receives
/// only the content and the document metadata.
#[derive(Debug, Clone)]
pub struct JsonChunker {
    text_fields: Vec<String>,
    combine_fields: bool,
    max_objects: usize,
}

impl JsonChunker {
    /// Creates a chunker from the config crate's JSON chunking settings.
    ///
    /// An empty `text_fields` gains the oracle's four defaults (the config
    /// crate normally already applied them); a negative `max_objects` is
    /// clamped to 0 (unlimited).
    pub fn new(config: JsonChunkerConfig) -> Self {
        let text_fields = if config.text_fields.is_empty() {
            default_text_fields()
        } else {
            config.text_fields
        };
        Self {
            text_fields,
            combine_fields: config.combine_fields,
            max_objects: config.max_objects.max(0) as usize,
        }
    }

    /// Produces the chunks of one JSON object at byte span `span` (oracle
    /// `chunkObject`): a single combined chunk, or one chunk per text field.
    fn chunk_object(
        &self,
        map: &Map<String, Value>,
        index: usize,
        span: (usize, usize),
        content: &str,
        metadata: &DocumentMetadata,
        chunks: &mut Vec<DocumentChunk>,
    ) {
        if self.combine_fields {
            let fields = text_fields_of(map, &self.text_fields);
            if fields.is_empty() {
                return; // No text content in this object (oracle parity).
            }
            let meta = with_extras(metadata, index, None, Some(&fields));
            push_chunk(chunks, content, span.0, span.1, &meta);
        } else {
            for field in &self.text_fields {
                let is_text = map
                    .get(field)
                    .is_some_and(|value| value.as_str().is_some_and(|s| !s.is_empty()));
                if !is_text {
                    continue;
                }
                let Some(field_span) = field_value_span(content, span, field) else {
                    continue;
                };
                let meta = with_extras(metadata, index, Some(field), None);
                push_chunk(chunks, content, field_span.0, field_span.1, &meta);
            }
        }
    }
}

impl Chunker for JsonChunker {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: Value =
            serde_json::from_str(content).map_err(|source| IngestionError::Json {
                path: PathBuf::from(metadata.source_file.clone()),
                source,
            })?;

        let mut chunks = Vec::new();
        match &value {
            Value::Array(items) => {
                let spans = array_element_spans(content, first_value_start(content));
                let count = if self.max_objects > 0 {
                    items.len().min(self.max_objects)
                } else {
                    items.len()
                };
                for (i, item) in items.iter().enumerate().take(count) {
                    // Non-object elements carry no fields to index (oracle:
                    // unparseable objects are skipped).
                    if let (Value::Object(map), Some(span)) = (item, spans.get(i)) {
                        self.chunk_object(map, i, *span, content, metadata, &mut chunks);
                    }
                }
            }
            Value::Object(map) => {
                let start = first_value_start(content);
                let end = skip_value(content, start).unwrap_or(content.len());
                self.chunk_object(map, 0, (start, end), content, metadata, &mut chunks);
            }
            // Scalar (string / number / bool / null): the whole content is
            // the chunk (see the module docs).
            _ => push_chunk(&mut chunks, content, 0, content.len(), metadata),
        }
        Ok(chunks)
    }
}

/// The oracle's default text fields (also the config crate's default).
fn default_text_fields() -> Vec<String> {
    ["description", "title", "wikitext", "html"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The non-empty string fields of `map` used in combined mode: the
/// configured `text_fields` present in the object (in configured order), or
/// — when none match — every non-empty string field (oracle
/// `combineFields` fallback; key order is BTreeMap order).
fn text_fields_of(map: &Map<String, Value>, configured: &[String]) -> Vec<String> {
    let matched: Vec<String> = configured
        .iter()
        .filter(|field| map.get(field.as_str()).is_some_and(is_text_value))
        .map(String::clone)
        .collect();
    if !matched.is_empty() {
        return matched;
    }
    map.iter()
        .filter(|(_, value)| is_text_value(value))
        .map(|(key, _)| key.clone())
        .collect()
}

/// True for a non-empty JSON string value (oracle `s != ""` check).
fn is_text_value(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

/// The document metadata plus the chunk-specific extras: `object_index`
/// always, `field_name` in per-field mode, `text_fields` in combined mode.
fn with_extras(
    metadata: &DocumentMetadata,
    object_index: usize,
    field_name: Option<&str>,
    text_fields: Option<&[String]>,
) -> DocumentMetadata {
    let mut meta = metadata.clone();
    meta.extra
        .insert("object_index".to_owned(), Value::from(object_index as u64));
    if let Some(field_name) = field_name {
        meta.extra.insert(
            "field_name".to_owned(),
            Value::String(field_name.to_owned()),
        );
    }
    if let Some(fields) = text_fields {
        meta.extra.insert(
            "text_fields".to_owned(),
            Value::Array(fields.iter().map(|f| Value::String(f.clone())).collect()),
        );
    }
    meta
}

/// Byte offset of the first non-whitespace byte of `content` (the start of
/// the top-level JSON value).
fn first_value_start(content: &str) -> usize {
    content
        .as_bytes()
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(content.len())
}

/// Byte spans `(start, end)` of every top-level element of the array
/// starting at `array_start` (a `[` byte), in element order.
fn array_element_spans(content: &str, array_start: usize) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut spans = Vec::new();
    let mut i = array_start + 1;
    loop {
        i = skip_ws(bytes, i);
        let Some(&b) = bytes.get(i) else {
            return spans;
        };
        if b == b']' {
            return spans;
        }
        if b == b',' {
            i += 1;
            continue;
        }
        let Some(end) = skip_value(content, i) else {
            return spans;
        };
        if end == i {
            return spans; // Zero progress: malformed input, stop defensively.
        }
        spans.push((i, end));
        i = end;
    }
}

/// `(key, value_start, value_end)` for every top-level field of the object
/// starting at `object_start` (a `{` byte). Values of nested objects/arrays
/// are skipped whole, never descended — only the top level is enumerated.
fn object_field_spans(content: &str, object_start: usize) -> Vec<(String, usize, usize)> {
    let bytes = content.as_bytes();
    let mut fields = Vec::new();
    let mut i = object_start + 1;
    loop {
        i = skip_ws(bytes, i);
        let Some(&b) = bytes.get(i) else {
            return fields;
        };
        if b == b',' {
            i += 1;
            continue;
        }
        if b == b'}' {
            return fields;
        }
        // The key is a JSON string (the content is validated upstream).
        let Some(key_end) = skip_value(content, i) else {
            return fields;
        };
        let quoted = &content[i..key_end];
        // Escape-decode the key; fall back to the raw inner text if it is
        // not a well-formed JSON string (defensive — validated upstream).
        let key = serde_json::from_str::<String>(quoted).unwrap_or_else(|_| {
            quoted
                .get(1..quoted.len().saturating_sub(1))
                .unwrap_or_default()
                .to_owned()
        });
        i = skip_ws(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return fields;
        }
        i = skip_ws(bytes, i + 1);
        let Some(value_end) = skip_value(content, i) else {
            return fields;
        };
        if value_end == i {
            return fields; // Zero progress: malformed input, stop defensively.
        }
        fields.push((key, i, value_end));
        i = value_end;
    }
}

/// Byte span of the top-level `field` value inside the object at `span`.
fn field_value_span(content: &str, span: (usize, usize), field: &str) -> Option<(usize, usize)> {
    object_field_spans(content, span.0)
        .into_iter()
        .find(|(key, _, _)| key == field)
        .map(|(_, start, end)| (start, end))
}

/// Byte offset one past the end of the JSON value starting at `start`.
///
/// The content is known-valid JSON (the parser validates it; the serde pass
/// in [`Chunker::chunk`] confirms it), so the scanner only has to find the
/// matching bracket while respecting string literals and escapes.
fn skip_value(content: &str, start: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    match *bytes.get(start)? {
        b'"' => end_of_string(bytes, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            while let Some(&b) = bytes.get(i) {
                match b {
                    b'"' => i = end_of_string(bytes, i)? - 1,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        // number / true / false / null: up to the first delimiter.
        _ => {
            let mut i = start;
            while let Some(&b) = bytes.get(i) {
                if matches!(b, b',' | b' ' | b'\t' | b'\n' | b'\r' | b'}' | b']') {
                    break;
                }
                i += 1;
            }
            Some(i)
        }
    }
}

/// Byte offset one past the closing quote of the string starting at `start`
/// (a `"` byte); a backslash escapes the following byte.
fn end_of_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while let Some(&c) = bytes.get(i) {
        match c {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skips JSON whitespace (ASCII space, tab, newline, carriage return).
fn skip_ws(bytes: &[u8], i: usize) -> usize {
    bytes[i..]
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .map_or(bytes.len(), |p| i + p)
}

/// Appends one chunk for `content[start..end]`; `sequence_num` is the chunk's
/// position in the returned slice (see the module docs).
fn push_chunk(
    chunks: &mut Vec<DocumentChunk>,
    content: &str,
    start: usize,
    end: usize,
    metadata: &DocumentMetadata,
) {
    chunks.push(DocumentChunk {
        doc_id: None,
        text: content[start..end].to_owned(),
        sequence_num: chunks.len(),
        start_offset: start,
        end_offset: end,
        metadata: metadata.clone(),
    });
}

#[cfg(test)]
mod tests {
    //! Differential tests against the Go oracle: inputs and chunk-count
    //! expectations are taken from
    //! `../synopsis/internal/ingestion/chunkers/json_chunker_test.go`, which
    //! passes there (`go test ./internal/ingestion/chunkers/`). Where an
    //! oracle expectation conflicts with this crate's byte-offset invariant
    //! (synthesized `**field**: value` text; scalar documents erroring), the
    //! raw-slice text and the metadata are asserted instead — see the module
    //! docs.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::JsonChunkerConfig;

    use super::*;

    fn config(fields: &[&str], combine: bool, max_objects: i32) -> JsonChunkerConfig {
        JsonChunkerConfig {
            text_fields: fields.iter().map(|s| s.to_string()).collect(),
            combine_fields: combine,
            max_objects,
        }
    }

    fn combined(fields: &[&str]) -> JsonChunker {
        JsonChunker::new(config(fields, true, 0))
    }

    /// Crate invariant: every chunk is a pure byte-offset slice of
    /// `content`, numbered consecutively, with no document id yet.
    fn assert_invariant(content: &str, chunks: &[DocumentChunk]) {
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(
                &content[c.start_offset..c.end_offset],
                c.text,
                "chunk {i} must be a pure slice"
            );
            assert_eq!(c.sequence_num, i, "chunk {i} sequence");
            assert_eq!(c.doc_id, None, "chunk {i} doc_id");
        }
    }

    fn extra<'a>(chunk: &'a DocumentChunk, key: &str) -> Option<&'a Value> {
        chunk.metadata.extra.get(key)
    }

    fn extra_str(chunk: &DocumentChunk, key: &str) -> Option<String> {
        extra(chunk, key).and_then(Value::as_str).map(str::to_owned)
    }

    fn extra_u64(chunk: &DocumentChunk, key: &str) -> Option<u64> {
        extra(chunk, key).and_then(Value::as_u64)
    }

    fn extra_strings(chunk: &DocumentChunk, key: &str) -> Vec<String> {
        extra(chunk, key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn array_of_objects_combined_one_chunk_per_object() {
        // Oracle TestJSONChunker_ChunkArray "array of objects combined".
        let content = "[\n\
                       {\"title\": \"First\", \"description\": \"Desc 1\"},\n\
                       {\"title\": \"Second\", \"description\": \"Desc 2\"}\n\
                       ]";
        let chunks = combined(&["title", "description"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        // The text is the raw object slice (oracle synthesized markdown).
        assert_eq!(
            chunks[0].text,
            "{\"title\": \"First\", \"description\": \"Desc 1\"}"
        );
        assert_eq!(
            chunks[1].text,
            "{\"title\": \"Second\", \"description\": \"Desc 2\"}"
        );
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(0));
        assert_eq!(extra_u64(&chunks[1], "object_index"), Some(1));
        // Field names live in the metadata, in configured order.
        assert_eq!(
            extra_strings(&chunks[0], "text_fields"),
            vec!["title", "description"]
        );
        assert!(extra(&chunks[0], "field_name").is_none());
    }

    #[test]
    fn array_of_objects_per_field_one_chunk_per_text_field() {
        // Oracle TestJSONChunker_ChunkArray "array of objects per field":
        // title + description = 2 chunks for 1 object.
        let content = "[{\"title\": \"First\", \"description\": \"Desc 1\"}]";
        let chunker = JsonChunker::new(config(&["title", "description"], false, 0));
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        // Per-field chunks are the raw string values (quotes included).
        assert_eq!(chunks[0].text, "\"First\"");
        assert_eq!(chunks[1].text, "\"Desc 1\"");
        assert_eq!(extra_str(&chunks[0], "field_name"), Some("title".into()));
        assert_eq!(
            extra_str(&chunks[1], "field_name"),
            Some("description".into())
        );
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(0));
    }

    #[test]
    fn empty_inputs_yield_no_chunks() {
        // Oracle TestJSONChunker_ChunkArray "empty array"; blank content is
        // the markdown chunker's convention.
        for content in ["[]", "", "   \n  "] {
            let chunks = combined(&["title"])
                .chunk(content, &DocumentMetadata::default())
                .unwrap();
            assert!(chunks.is_empty(), "input: {content:?}");
        }
    }

    #[test]
    fn single_object_combined_and_per_field() {
        // Oracle TestJSONChunker_ChunkObject (both cases).
        let content = "{\"title\": \"Page Title\", \"description\": \"Full description here.\"}";
        let meta = DocumentMetadata::default();

        let chunks = combined(&["title", "description"])
            .chunk(content, &meta)
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, content);
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(0));

        let chunker = JsonChunker::new(config(&["title", "description"], false, 0));
        let chunks = chunker.chunk(content, &meta).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "\"Page Title\"");
        assert_eq!(chunks[1].text, "\"Full description here.\"");
    }

    #[test]
    fn invalid_json_is_an_error() {
        // Oracle TestJSONChunker_InvalidJSON. In the pipeline the parser
        // rejects broken JSON up front; this guards direct chunker use.
        let err = combined(&["title"])
            .chunk("{invalid json}", &DocumentMetadata::default())
            .unwrap_err();
        assert!(matches!(err, IngestionError::Json { .. }), "got {err:?}");
    }

    #[test]
    fn max_objects_limits_processed_elements() {
        // Oracle TestJSONChunker_MaxObjects.
        let content = "[{\"title\": \"A\"}, {\"title\": \"B\"}, {\"title\": \"C\"}]";
        let chunker = JsonChunker::new(config(&["title"], true, 2));
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(0));
        assert_eq!(extra_u64(&chunks[1], "object_index"), Some(1));
        // max_objects = 0 (the default) is unlimited.
        let chunks = combined(&["title"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn scalar_document_is_one_chunk_of_the_whole_content() {
        // Deviation from the oracle (module docs): valid scalars are ingested
        // by the parser with structure "unknown", so the chunker must not
        // hard-fail on them.
        for scalar in ["42", "\"hello\"", "true", "null"] {
            let chunks = combined(&["title"])
                .chunk(scalar, &DocumentMetadata::default())
                .unwrap();
            assert_eq!(chunks.len(), 1, "scalar {scalar}");
            assert_invariant(scalar, &chunks);
            assert_eq!(chunks[0].text, scalar);
            assert!(extra(&chunks[0], "object_index").is_none());
        }
    }

    #[test]
    fn byte_offsets_hold_on_cyrillic_utf8() {
        // 'я' is 2 bytes in UTF-8, so byte and character offsets diverge; the
        // invariant must hold in bytes and every span must land on a
        // character boundary (slicing would panic otherwise).
        let content = format!("[{{\"description\": \"{}\"}}]", "я".repeat(30));
        let chunker = JsonChunker::new(config(&["description"], false, 0));
        let chunks = chunker
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(&content, &chunks);
        // The value span is the quoted string: 2 quotes + 30 × 2 bytes.
        assert_eq!(chunks[0].text, format!("\"{}\"", "я".repeat(30)));
        assert_eq!(chunks[0].text.len(), 62);

        // A Cyrillic key before the field shifts byte and character offsets
        // apart: the value starts at byte 45 but character 35.
        let content = "[{\"заголовок\": \"я\", \"description\": \"текст\"}]";
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].start_offset, 45);
        assert_ne!(
            chunks[0].start_offset, 35,
            "byte offset, not character offset"
        );
        assert_eq!(chunks[0].text, "\"текст\"");
    }

    #[test]
    fn objects_without_text_content_are_skipped() {
        // Oracle combineFields: no parts -> no chunk; perFieldChunks: a
        // missing field simply yields no chunk.
        let content = "[{\"id\": 1}, {\"title\": \"Has text\"}]";
        let chunks = combined(&["title"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(1));

        let chunker = JsonChunker::new(config(&["title"], false, 0));
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "\"Has text\"");
    }

    #[test]
    fn combine_falls_back_to_all_string_fields() {
        // Oracle combineFields fallback: no configured field present -> use
        // all non-empty string fields (here: "name").
        let content = "{\"name\": \"bob\", \"age\": 30}";
        let chunks = combined(&["title", "description"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, content);
        assert_eq!(extra_strings(&chunks[0], "text_fields"), vec!["name"]);

        // An object with no string fields at all still yields no chunk.
        let chunks = combined(&["title"])
            .chunk("{\"id\": 1, \"count\": 2}", &DocumentMetadata::default())
            .unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn non_object_array_elements_are_skipped() {
        // Oracle chunkArray: elements that do not unmarshal to an object are
        // skipped; their indices stay in `object_index`.
        let content = "[{\"title\": \"A\"}, 42, null, \"str\", [1]]";
        let chunks = combined(&["title"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "{\"title\": \"A\"}");
        assert_eq!(extra_u64(&chunks[0], "object_index"), Some(0));
    }

    #[test]
    fn strings_with_braces_and_escapes_keep_their_spans() {
        // The bracket matcher must respect string literals: braces, brackets
        // and escaped quotes inside values do not end the object early.
        let content = "[{\"description\": \"a } { \\\"quoted\\\" [1]\"}]";
        let chunks = combined(&["description"])
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(
            chunks[0].text,
            "{\"description\": \"a } { \\\"quoted\\\" [1]\"}"
        );

        let chunker = JsonChunker::new(config(&["description"], false, 0));
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "\"a } { \\\"quoted\\\" [1]\"");
    }

    #[test]
    fn nested_objects_do_not_leak_fields() {
        // Only the top level of the object is enumerated: the nested
        // "title" must not be matched.
        let content = "{\"title\": \"Top\", \"meta\": {\"title\": \"Nested\"}}";
        let chunker = JsonChunker::new(config(&["title"], false, 0));
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "\"Top\"");
    }

    #[test]
    fn default_text_fields_are_applied_when_empty() {
        // JsonChunkerConfig::default() has an empty field list and
        // combine_fields = false: the oracle's four defaults kick in.
        let chunker = JsonChunker::new(JsonChunkerConfig::default());
        let content = "{\"wikitext\": \"== T ==\", \"other\": 1}";
        let chunks = chunker
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "\"== T ==\"");
        assert_eq!(extra_str(&chunks[0], "field_name"), Some("wikitext".into()));
    }

    #[test]
    fn document_metadata_is_copied_into_chunks() {
        // The originating document metadata (including the parser's
        // "structure" extra) rides along into every chunk.
        let mut meta = DocumentMetadata {
            source_type: "json".to_owned(),
            source_file: "data/items.json".to_owned(),
            ..Default::default()
        };
        meta.extra
            .insert("structure".to_owned(), Value::String("array".to_owned()));
        let content = "[{\"title\": \"A\"}]";
        let chunks = combined(&["title"]).chunk(content, &meta).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].metadata.source_file, "data/items.json");
        assert_eq!(chunks[0].metadata.source_type, "json");
        assert_eq!(extra_str(&chunks[0], "structure"), Some("array".into()));
    }
}
