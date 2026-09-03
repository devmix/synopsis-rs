//! Structure-aware Markdown chunker (oracle:
//! `internal/ingestion/chunkers/markdown_chunker.go` and its tests).
//!
//! Splits a Markdown document at ATX heading boundaries (levels 1-6) and caps
//! sections that exceed `max_chunk_size` with fixed-size splitting + overlap.
//! Configuration is injected at construction time from
//! [`config::preset::MarkdownChunkerConfig`] (`chunking.markdown.*`), which the
//! config crate has already normalized: defaults applied, and a configured
//! `overlap_size: 0` preserved (the chunker never substitutes defaults).
//!
//! **Deliberate deviations from the oracle** (functional copy, not code copy):
//!
//! * **Byte-offset invariant (crate contract).** Chunk text is a pure slice of
//!   the source: `content[start_offset..end_offset] == text`. The oracle
//!   trimmed sections and prefixed breadcrumbs/file names into `Text` while
//!   keeping offsets at the original span; here the breadcrumb, section title
//!   and image paths live in the chunk [`metadata`](DocumentChunk::metadata)
//!   bag (`section_title`, `breadcrumb`, `image_paths`), never in `text`.
//! * **`search_text` (search-text-embedding design D1).** The chunk also
//!   carries `search_text` — the only synthetic field — built from the
//!   breadcrumb already computed for the metadata plus the body:
//!   `breadcrumb + "\n\n" + text` for sectioned chunks, or `text` when there
//!   is no breadcrumb (preamble / headingless). This is the exact
//!   breadcrumb-prefixed text the oracle fed both search legs; here it lives
//!   in a dedicated field so `text` keeps the byte-offset invariant.
//! * **Strategy collapse.** The oracle's `"headers"` strategy had no size cap
//!   (unbounded chunks overflow the embedding context); Rust `"headers"` and
//!   `"hybrid"` both produce structure-aware chunks with oversized sections
//!   split internally (the oracle's hybrid behavior). `"fixed"` is unchanged:
//!   plain fixed-size splitting of the whole content.
//! * **Character-based fixed splitting.** The oracle split at raw byte offsets
//!   (Go `len`), which can land mid-rune on non-ASCII text and emit invalid
//!   UTF-8 chunks. Sizes are in characters (as the config documents) and every
//!   span lands on a character boundary.
//! * **`min_section_size` is not used.** The oracle only ever checked it `> 0`
//!   (the value was never compared), and the config crate normalizes it to a
//!   positive default, so the gate was dead; Rust always splits sections that
//!   exceed `max_chunk_size`.
//! * **`sequence_num`** is the chunk's position in the returned slice (the
//!   oracle numbered heading chunks by heading index, which collided with the
//!   preamble chunk and left gaps at skipped header-only sections).

use config::preset::{ChunkingStrategy, MarkdownChunkerConfig};
use serde_json::{Map, Value};

use crate::error::IngestionError;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata};

/// Structure-aware Markdown chunker.
///
/// See the module docs for the strategy semantics and the deviations from the
/// oracle. Stateless after construction: [`Chunker::chunk`] receives only the
/// content and the document metadata.
#[derive(Debug, Clone)]
pub struct MarkdownChunker {
    strategy: ChunkingStrategy,
    max_chunk_size: usize,
    overlap_size: usize,
}

impl MarkdownChunker {
    /// Creates a chunker from the config crate's markdown chunking settings.
    ///
    /// The config crate's `apply_defaults` normally guarantees
    /// `max_chunk_size > 0` and `overlap_size >= 0`; a directly constructed
    /// config is clamped defensively (`max` to 1, `overlap` to 0) instead of
    /// failing.
    pub fn new(config: MarkdownChunkerConfig) -> Self {
        Self {
            strategy: config.strategy,
            max_chunk_size: config.max_chunk_size.max(1) as usize,
            overlap_size: config.overlap_size.max(0) as usize,
        }
    }

    /// Structure-aware chunking: an optional preamble plus one chunk (or a run
    /// of overlapping sub-chunks) per section that has a body.
    fn chunk_by_headers(&self, content: &str, metadata: &DocumentMetadata) -> Vec<DocumentChunk> {
        let headings = find_headings(content);
        let mut chunks = Vec::new();
        if headings.is_empty() {
            // No headings: the whole document is one preamble chunk.
            let mut meta = metadata.extra.clone();
            insert_image_paths(&mut meta, content);
            push_chunk(&mut chunks, content, 0, content.len(), &meta);
            return chunks;
        }

        // Preamble: text before the first heading (oracle keeps it only when
        // non-blank; the file name stays in the document metadata, not in
        // the text).
        let first = headings[0].pos;
        if first > 0 && !content[..first].trim().is_empty() {
            let mut meta = metadata.extra.clone();
            insert_image_paths(&mut meta, &content[..first]);
            push_chunk(&mut chunks, content, 0, first, &meta);
        }

        for (idx, heading) in headings.iter().enumerate() {
            let end = headings.get(idx + 1).map_or(content.len(), |next| next.pos);
            let section = &content[heading.pos..end];
            // A section without a body carries no content to index (oracle
            // `isHeaderOnly`).
            if is_header_only(section) {
                continue;
            }

            let mut meta = metadata.extra.clone();
            meta.insert(
                "section_title".to_owned(),
                Value::String(heading.text.clone()),
            );
            meta.insert("heading_level".to_owned(), Value::from(heading.level));
            let breadcrumb = build_breadcrumbs(&headings, idx);
            if !breadcrumb.is_empty() {
                meta.insert("breadcrumb".to_owned(), Value::String(breadcrumb));
            }
            // Images are extracted from the body, never from the heading line
            // (oracle `sectionBody` + `imageRe`).
            if let Some(body) = section_body(section) {
                insert_image_paths(&mut meta, body);
            }

            // `fixed_spans` yields a single (0, len) span for sections that
            // fit, so this one loop covers both the plain and the split case.
            for (start, end) in fixed_spans(section, self.max_chunk_size, self.overlap_size) {
                push_chunk(
                    &mut chunks,
                    content,
                    heading.pos + start,
                    heading.pos + end,
                    &meta,
                );
            }
        }
        chunks
    }

    /// Plain fixed-size chunking of the whole content (oracle `"fixed"`
    /// strategy).
    fn chunk_fixed(&self, content: &str, metadata: &DocumentMetadata) -> Vec<DocumentChunk> {
        let mut chunks = Vec::new();
        for (start, end) in fixed_spans(content, self.max_chunk_size, self.overlap_size) {
            push_chunk(&mut chunks, content, start, end, &metadata.extra);
        }
        chunks
    }
}

impl Chunker for MarkdownChunker {
    fn chunk(
        &self,
        content: &str,
        metadata: &DocumentMetadata,
    ) -> Result<Vec<DocumentChunk>, IngestionError> {
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(match &self.strategy {
            ChunkingStrategy::Headers | ChunkingStrategy::Hybrid => {
                self.chunk_by_headers(content, metadata)
            }
            ChunkingStrategy::Fixed => self.chunk_fixed(content, metadata),
            ChunkingStrategy::Unknown(word) => {
                return Err(IngestionError::UnknownStrategy(word.clone()));
            }
        })
    }
}

/// One ATX heading found in the content.
struct Heading {
    /// Heading level (1-6, the number of leading `#`).
    level: u8,
    /// Heading text, trimmed (inline markers like `**` kept; `clean_heading`
    /// strips them for breadcrumbs only).
    text: String,
    /// Byte offset of the heading line's first byte in the content.
    pos: usize,
}

/// Extracts all ATX headings with their byte positions (oracle
/// `findHeadings` + `headingRe = ^#{1,6}\s+(.+)$` applied per line).
fn find_headings(content: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut offset = 0;
    for line in content.split('\n') {
        if let Some((level, text)) = parse_atx_heading(line) {
            headings.push(Heading {
                level,
                text: text.to_owned(),
                pos: offset,
            });
        }
        offset += line.len() + 1; // +1 for the newline
    }
    headings
}

/// Parses one line as an ATX heading: 1-6 `#`, at least one whitespace
/// character, then non-empty text.
fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !matches!(level, 1..=6) {
        return None;
    }
    let rest = line.get(level..)?;
    // Go's `\s` class, minus the newline the line split already removed.
    if !matches!(rest.get(..1)?, " " | "\t" | "\r" | "\u{0b}" | "\u{0c}") {
        return None;
    }
    let text = rest[1..].trim();
    if text.is_empty() {
        return None;
    }
    Some((level as u8, text))
}

/// Strips inline markdown markers from a heading for breadcrumb display
/// (oracle `cleanHeading`: removes `**`, `*`, backticks, trims).
fn clean_heading(text: &str) -> String {
    text.replace("**", "")
        .replace(['*', '`'], "")
        .trim()
        .to_owned()
}

/// Multi-line breadcrumb of the heading path (oracle `buildBreadcrumbs`):
/// one `> Title` line per ancestor, indented by depth, most recent last.
fn build_breadcrumbs(headings: &[Heading], current_idx: usize) -> String {
    let mut path = Vec::new();
    let mut level = headings[current_idx].level;
    for j in (0..current_idx).rev() {
        let ancestor = &headings[j];
        if ancestor.level < level {
            path.insert(0, clean_heading(&ancestor.text));
            level = ancestor.level;
            if ancestor.level == 1 {
                break;
            }
        }
    }
    path.push(clean_heading(&headings[current_idx].text));
    path.iter()
        .enumerate()
        .map(|(depth, title)| format!("{}> {title}", " ".repeat(depth)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// True if the section has no body: every line after the heading line is
/// blank (oracle `isHeaderOnly`).
fn is_header_only(section: &str) -> bool {
    section.lines().skip(1).all(|line| line.trim().is_empty())
}

/// The section text after the heading line, trimmed (oracle `sectionBody`).
fn section_body(section: &str) -> Option<&str> {
    section.split_once('\n').map(|(_, rest)| rest.trim())
}

/// Stores the `![alt](path)` image paths of `text` in the chunk's metadata
/// bag (oracle `imageRe` + `extractImageMetadata`). The alt text is ignored;
/// an empty path is not an image.
fn insert_image_paths(meta: &mut Map<String, Value>, text: &str) {
    let mut paths = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("![") {
        let start = from + rel;
        let after = &text[start + 2..];
        let Some(close) = after.find(']') else {
            from = start + 2;
            continue;
        };
        let Some(inner) = after[close + 1..].strip_prefix('(') else {
            from = start + 2;
            continue;
        };
        let Some(paren) = inner.find(')') else {
            from = start + 2;
            continue;
        };
        let path = &inner[..paren];
        if !path.is_empty() {
            paths.push(path.to_owned());
        }
        from = start + 2 + close + 1 + 1 + paren + 1;
    }
    if !paths.is_empty() {
        meta.insert(
            "image_paths".to_owned(),
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }
}

/// Fixed-size spans `(start, end)` in byte offsets over `content`, split at
/// character boundaries: `max`/`overlap` are character counts (the config's
/// documented units). Oracle `chunkFixed`, re-based from raw byte offsets
/// (which could land mid-rune on non-ASCII text) onto character boundaries.
fn fixed_spans(content: &str, max: usize, overlap: usize) -> Vec<(usize, usize)> {
    let max = max.max(1);
    // Byte offset just after each character: `char_ends[k]` is where the
    // k-th character (0-based) ends; `char_ends[0] = 0`, last = content.len().
    let mut char_ends = Vec::with_capacity(content.len() + 1);
    char_ends.push(0);
    let mut byte = 0;
    for ch in content.chars() {
        byte += ch.len_utf8();
        char_ends.push(byte);
    }
    let total = char_ends.len() - 1;
    if total <= max {
        return vec![(0, content.len())];
    }
    let step = if overlap >= max { max } else { max - overlap };
    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < total {
        let end = (start + max).min(total);
        spans.push((char_ends[start], char_ends[end]));
        if end >= total {
            break;
        }
        start += step;
    }
    spans
}

/// Appends one chunk for `content[start..end]`; `sequence_num` is the chunk's
/// position in the returned slice (see the module docs).
///
/// `search_text` (search-text-embedding design D1) is the breadcrumb context
/// the FTS5 index and the embedding leg operate on: `breadcrumb + "\n\n" +
/// text` when the chunk carries a section breadcrumb (already in the
/// metadata bag), or `text` otherwise (preamble / headingless). The `text`
/// itself stays a pure source slice — the byte-offset invariant is untouched.
fn push_chunk(
    chunks: &mut Vec<DocumentChunk>,
    content: &str,
    start: usize,
    end: usize,
    metadata: &Map<String, Value>,
) {
    let text = content[start..end].to_owned();
    let search_text = match metadata.get("breadcrumb").and_then(Value::as_str) {
        Some(breadcrumb) => format!("{breadcrumb}\n\n{text}"),
        None => text.clone(),
    };
    chunks.push(DocumentChunk {
        doc_id: None,
        text,
        search_text,
        sequence_num: chunks.len(),
        start_offset: start,
        end_offset: end,
        metadata: metadata.clone(),
    });
}

#[cfg(test)]
mod tests {
    //! Differential tests against the Go oracle.
    //!
    //! Inputs and expectations (chunk counts, breadcrumbs, section titles,
    //! skip rules) are taken from
    //! `../synopsis/internal/ingestion/chunkers/markdown_chunker_test.go`,
    //! which passes there (`go test ./internal/ingestion/chunkers/`). Where an
    //! oracle expectation conflicts with this crate's byte-offset invariant
    //! (text with breadcrumb/file-name prefixes; hybrid sub-chunk offsets
    //! re-based onto a re-prefixed text), the span and the metadata are
    //! asserted instead of the prefixed text — see the module docs.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::ChunkingStrategy;

    use super::*;
    use crate::error::IngestionError;

    fn md_config(strategy: ChunkingStrategy, max: i32, overlap: i32) -> MarkdownChunkerConfig {
        MarkdownChunkerConfig {
            strategy,
            max_chunk_size: max,
            overlap_size: overlap,
            // Deliberately 0: the value must not gate splitting (module docs).
            min_section_size: 0,
        }
    }

    fn chunker(max: i32, overlap: i32) -> MarkdownChunker {
        MarkdownChunker::new(md_config(ChunkingStrategy::Headers, max, overlap))
    }

    fn titles(chunks: &[DocumentChunk]) -> Vec<String> {
        chunks
            .iter()
            .map(|c| {
                c.metadata
                    .get("section_title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    fn breadcrumb(chunk: &DocumentChunk) -> Option<&str> {
        chunk.metadata.get("breadcrumb").and_then(Value::as_str)
    }

    /// Crate invariant: every chunk is a pure byte-offset slice of `content`,
    /// numbered consecutively, with no document id yet.
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

    #[test]
    fn header_only_sections_are_skipped() {
        // Oracle TestMarkdownChunker_HeaderOnlySkip.
        let cases = [
            ("# A\n\n## A.1\ntext under a1\n\n## A.2\ntext under a2", 2),
            (
                "# Title\nsome intro text\n\n## Section 1\nbody one\n\n## Section 2\nbody two",
                3,
            ),
            ("# A\n\n## B\n\n### C", 0),
            (
                "# Title\nintro text\n\n## Section 1\nbody here\n\n## Empty Section",
                2,
            ),
        ];
        for (content, want) in cases {
            let chunks = chunker(1000, 100)
                .chunk(content, &DocumentMetadata::default())
                .unwrap();
            assert_eq!(chunks.len(), want, "input: {content:?}");
            assert_invariant(content, &chunks);
        }
    }

    #[test]
    fn breadcrumbs_follow_the_heading_hierarchy() {
        // Oracle TestMarkdownChunker_Breadcrumbs (two-level case) and
        // TestMarkdownChunker_AcceptanceCriteria.
        let content = "# A\n\n## A.1\ntext under a1\n\n## A.2\ntext under a2";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(
            chunks.iter().map(breadcrumb).collect::<Vec<_>>(),
            vec![Some("> A\n > A.1"), Some("> A\n > A.2")]
        );
        assert_eq!(titles(&chunks), vec!["A.1", "A.2"]);
        // The chunk's metadata bag carries the section keys under a heading
        // hierarchy (task 1.1, criterion 2).
        assert_eq!(
            chunks[0].metadata["breadcrumb"],
            Value::String("> A\n > A.1".to_owned())
        );
        assert_eq!(
            chunks[0].metadata["section_title"],
            Value::String("A.1".to_owned())
        );
        assert_eq!(chunks[0].metadata["heading_level"], Value::from(2));
        // The text stays a pure slice: it starts at the heading line, and the
        // breadcrumb lives in the bag instead of prefixing the text.
        assert_eq!(chunks[0].text, "## A.1\ntext under a1\n\n");
        assert_eq!(chunks[1].text, "## A.2\ntext under a2");
    }

    // (search-text-embedding task 2.1, criterion 1) sectioned chunks carry
    // `search_text = breadcrumb + "\n\n" + text`; the pure-slice `text` and
    // the byte-offset invariant are untouched.
    #[test]
    fn search_text_is_breadcrumb_plus_text_for_sectioned_chunks() {
        let content = "# A\n\n## A.1\ntext under a1\n\n## A.2\ntext under a2";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        // Both chunks are under a heading hierarchy: search_text is the
        // breadcrumb context prefixed to the (pure-slice) text.
        for chunk in &chunks {
            let breadcrumb = breadcrumb(chunk).expect("sectioned chunk has a breadcrumb");
            assert_eq!(
                chunk.search_text,
                format!("{breadcrumb}\n\n{}", chunk.text),
                "search_text = breadcrumb + \"\\n\\n\" + text"
            );
        }
        assert_eq!(
            chunks[0].search_text,
            "> A\n > A.1\n\n## A.1\ntext under a1\n\n"
        );
        assert_eq!(
            chunks[1].search_text,
            "> A\n > A.2\n\n## A.2\ntext under a2"
        );
    }

    // (search-text-embedding task 2.1, criterion 1) a chunk with no
    // breadcrumb (preamble / headingless) has `search_text == text`.
    #[test]
    fn search_text_equals_text_without_a_breadcrumb() {
        let content = "This is intro text before any heading.\n\n## Section\nbody";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        // The preamble has no breadcrumb: search_text == text.
        assert!(chunks[0].metadata.get("breadcrumb").is_none());
        assert_eq!(chunks[0].search_text, chunks[0].text);
        // The sectioned chunk carries the breadcrumb context.
        assert_eq!(chunks[1].search_text, "> Section\n\n## Section\nbody");

        // A headingless document: the single chunk has no breadcrumb.
        let plain = "Just plain text without headers.";
        let chunks = chunker(1000, 100)
            .chunk(plain, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].search_text, chunks[0].text);
    }

    #[test]
    fn three_level_and_mixed_breadcrumbs() {
        // Oracle TestMarkdownChunker_Breadcrumbs (remaining cases).
        let content = "# Chapter\n\n## Section 1\n\n### Subsection 1.1\ndeep content here";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(content, &chunks);
        assert_eq!(
            breadcrumb(&chunks[0]),
            Some("> Chapter\n > Section 1\n  > Subsection 1.1")
        );
        assert_eq!(
            chunks[0]
                .metadata
                .get("heading_level")
                .and_then(Value::as_u64),
            Some(3)
        );

        let content = "# Root\n\n## Child A\ncontent a\n\n## Child B\n\n### Grandchild B.1\ncontent b1\n\n\
             ### Grandchild B.2\ncontent b2";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 3);
        assert_invariant(content, &chunks);
        assert_eq!(
            chunks.iter().map(breadcrumb).collect::<Vec<_>>(),
            vec![
                Some("> Root\n > Child A"),
                Some("> Root\n > Child B\n  > Grandchild B.1"),
                Some("> Root\n > Child B\n  > Grandchild B.2"),
            ]
        );
        assert_eq!(
            titles(&chunks),
            vec!["Child A", "Grandchild B.1", "Grandchild B.2"]
        );
    }

    #[test]
    fn clean_heading_strips_inline_markers() {
        // Oracle TestMarkdownChunker_CleanHeading.
        let cases = [
            ("**Bold Heading**", "Bold Heading"),
            ("*Italic Heading*", "Italic Heading"),
            ("`code` heading", "code heading"),
            ("**1.1 Section**", "1.1 Section"),
            (
                "*`code`* and **bold** in *heading*",
                "code and bold in heading",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(clean_heading(input), want);
        }
    }

    #[test]
    fn preamble_and_headingless_documents() {
        // Oracle TestMarkdownChunker_PreambleWithFileName +
        // TestMarkdownChunker_TextWithoutHeaders. The file name stays in the
        // document metadata (source_file) instead of prefixing the text.
        let content = "This is intro text before any heading.\n\n## Section\nbody";
        let metadata = DocumentMetadata {
            source_file: "docs/guide.md".to_owned(),
            ..Default::default()
        };
        let chunks = chunker(1000, 100).chunk(content, &metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "This is intro text before any heading.\n\n");
        // The preamble chunk's bag carries no section keys, and the document
        // has no `extra` keys of its own (the typed fields stay on the
        // document, not in the chunk's bag).
        assert!(
            chunks[0].metadata.is_empty(),
            "preamble bag: {:?}",
            chunks[0].metadata
        );
        assert_eq!(titles(&chunks[1..]), vec!["Section"]);

        let plain = "Just plain text without headers.";
        let chunks = chunker(1000, 100)
            .chunk(plain, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant(plain, &chunks);
        assert_eq!(chunks[0].text, plain);

        for empty in ["", "   \n\n  "] {
            let chunks = chunker(1000, 100)
                .chunk(empty, &DocumentMetadata::default())
                .unwrap();
            assert!(chunks.is_empty(), "input: {empty:?}");
        }
    }

    #[test]
    fn long_section_is_split_internally_with_overlap() {
        let content = format!("# T\n\n## Sec\n{}", "a".repeat(300));
        let chunks = chunker(100, 20)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // "# T" is header-only (skipped); the 307-char section is split at
        // step 80: 100 + 100 + 100 + 7.
        assert_eq!(chunks.len(), 4);
        assert_invariant(&content, &chunks);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 100));
        assert_eq!(titles(&chunks), vec!["Sec"; 4]);
        // Consecutive sub-chunks overlap by exactly 20 characters.
        for (prev, next) in chunks.iter().zip(chunks.iter().skip(1)) {
            let tail: String = prev.text.chars().skip(80).collect();
            let head: String = next.text.chars().take(20).collect();
            assert_eq!(tail, head, "20-char overlap");
        }
        // The first sub-chunk starts at the heading line; the last reaches
        // the section end.
        assert!(chunks[0].text.starts_with("## Sec\n"));
        assert_eq!(chunks[3].end_offset, content.len());
    }

    #[test]
    fn zero_overlap_is_preserved_and_min_section_size_does_not_gate() {
        // The config crate keeps a configured overlap of 0; the chunker must
        // not substitute a default. min_section_size: 0 must not disable
        // splitting either (module docs).
        let content = format!("## S\n{}", "y".repeat(25));
        let chunks = chunker(10, 0)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // The 30-char section splits at step 10: 10 + 10 + 10, back to back.
        assert_eq!(chunks.len(), 3);
        assert_invariant(&content, &chunks);
        assert!(chunks.iter().all(|c| c.text.chars().count() == 10));
        for (prev, next) in chunks.iter().zip(chunks.iter().skip(1)) {
            assert_eq!(
                prev.end_offset, next.start_offset,
                "no overlap between consecutive chunks"
            );
        }
    }

    #[test]
    fn byte_offsets_hold_on_cyrillic_utf8() {
        // 'я' is 2 bytes in UTF-8, so byte and character offsets diverge; the
        // invariant must hold in bytes and every span must land on a
        // character boundary (slicing would panic otherwise).
        let content = format!("## Раздел\n{}", "я".repeat(150));
        let chunks = chunker(50, 10)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // 160 chars, step 40: 50 + 50 + 50 + 40.
        assert_eq!(chunks.len(), 4);
        assert_invariant(&content, &chunks);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 50));
        // Header "## Раздел\n" is 10 chars / 16 bytes: character 40 is byte 76.
        assert_eq!(chunks[1].start_offset, 76);
        assert_ne!(
            chunks[1].start_offset, 40,
            "byte offset, not character offset"
        );
        // Consecutive sub-chunks overlap by exactly 10 characters.
        for (prev, next) in chunks.iter().zip(chunks.iter().skip(1)) {
            let tail: String = prev.text.chars().skip(40).collect();
            let head: String = next.text.chars().take(10).collect();
            assert_eq!(tail, head, "10-char overlap");
        }
    }

    #[test]
    fn fixed_strategy_splits_the_whole_document() {
        // Oracle TestMarkdownChunker_FixedStrategy.
        let metadata = DocumentMetadata::default();
        let fixed = MarkdownChunker::new(md_config(ChunkingStrategy::Fixed, 100, 0));
        let chunks = fixed.chunk("Short text.", &metadata).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_invariant("Short text.", &chunks);

        let long = "A".repeat(300);
        let chunks = fixed.chunk(&long, &metadata).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_invariant(&long, &chunks);
        assert!(chunks.iter().all(|c| c.text == "A".repeat(100)));
    }

    #[test]
    fn hybrid_strategy_matches_headers() {
        // Oracle TestMarkdownChunker_HybridStrategy, plus the strategy
        // collapse: hybrid and headers produce identical output.
        let content = "# Root\n\n## Section 1\nbody text here";
        let metadata = DocumentMetadata::default();
        let headers = chunker(1000, 100).chunk(content, &metadata).unwrap();
        let hybrid = MarkdownChunker::new(md_config(ChunkingStrategy::Hybrid, 1000, 100))
            .chunk(content, &metadata)
            .unwrap();
        assert_eq!(headers, hybrid);
        assert_eq!(headers.len(), 1);
        assert_eq!(breadcrumb(&headers[0]), Some("> Root\n > Section 1"));

        // Long sections are split internally under hybrid as well.
        let long = format!("## Sec\n{}", "a".repeat(300));
        let hybrid = MarkdownChunker::new(md_config(ChunkingStrategy::Hybrid, 100, 20))
            .chunk(&long, &metadata)
            .unwrap();
        assert_eq!(hybrid.len(), 4);
        assert_invariant(&long, &hybrid);
    }

    #[test]
    fn unknown_strategy_is_an_explicit_error() {
        let chunker = MarkdownChunker::new(md_config(
            ChunkingStrategy::Unknown("rolling".into()),
            100,
            0,
        ));
        let err = chunker
            .chunk("text", &DocumentMetadata::default())
            .unwrap_err();
        assert!(
            matches!(err, IngestionError::UnknownStrategy(ref word) if word == "rolling"),
            "got {err:?}"
        );
    }

    #[test]
    fn image_paths_go_to_metadata_not_text() {
        let content = "intro\n\n## S\n\n![alt](img/a.png) and ![b](c/d.jpg)\n";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert!(chunks[0].metadata.get("image_paths").is_none());
        let images = chunks[1]
            .metadata
            .get("image_paths")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            images.as_slice(),
            [
                Value::String("img/a.png".to_owned()),
                Value::String("c/d.jpg".to_owned()),
            ]
        );
        // The image syntax stays in the text untouched.
        assert!(chunks[1].text.contains("![alt](img/a.png)"));
    }
}
