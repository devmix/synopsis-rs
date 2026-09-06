//! Structure-aware Mediawiki (wikitext) chunker.
//!
//! **Design:** the markdown chunker's ATX heading matcher does not match the
//! wikitext heading syntax `== Title ==`, so a heading-rich page fed to it
//! would collapse into one unsplit chunk. This chunker therefore applies the
//! markdown chunker's section algorithm to the wikitext syntax (levels 1-6,
//! balanced leading/trailing `=`), with a hand-written matcher — the same
//! no-regex policy as the markdown chunker.
//!
//! Strategy semantics mirror the markdown chunker:
//!
//! * `Headers`/`Hybrid`: an optional preamble plus one chunk (or a run of
//!   overlapping sub-chunks) per section that has a body; header-only
//!   sections are skipped; `section_title`, `heading_level` and
//!   `breadcrumb` extras.
//! * `Fixed`: plain fixed-size splitting of the whole content.
//! * `Unknown`: an explicit [`IngestionError::UnknownStrategy`].
//!
//! Configuration is the config crate's section-aware text knobs
//! ([`MarkdownChunkerConfig`], `chunking.markdown.*`): the config format has
//! no mediawiki section (frozen contract), so the mediawiki source reuses
//! the markdown chunker settings — same knobs, same meaning.
//!
//! The byte-offset invariant (crate contract) holds as in the markdown
//! chunker: `content[start_offset..end_offset] == text`. Image paths come
//! from the parser's `image_paths` extra (the page JSON's `images` field),
//! never from the chunker.
//!
//! The section helpers below mirror the markdown chunker's (same algorithm,
//! same shape); they stay private here rather than shared until a
//! consolidation task unifies the two chunkers.

use config::preset::{ChunkingStrategy, MarkdownChunkerConfig};
use serde_json::{Map, Value};

use crate::error::IngestionError;
use crate::types::{Chunker, DocumentChunk, DocumentMetadata};

/// Structure-aware Mediawiki (wikitext) chunker.
///
/// See the module docs for the strategy semantics and the design rationale.
/// Stateless after construction: [`Chunker::chunk`] receives only the
/// content and the document metadata.
#[derive(Debug, Clone)]
pub struct MediawikiChunker {
    strategy: ChunkingStrategy,
    max_chunk_size: usize,
    overlap_size: usize,
}

impl MediawikiChunker {
    /// Creates a chunker from the config crate's section-aware text settings.
    ///
    /// The config crate's `apply_defaults` normally guarantees
    /// `max_chunk_size > 0` and `overlap_size >= 0`; a directly constructed
    /// config is clamped defensively (`max` to 1, `overlap` to 0) instead of
    /// failing, like the markdown chunker.
    pub fn new(config: MarkdownChunkerConfig) -> Self {
        Self {
            strategy: config.strategy,
            max_chunk_size: config.max_chunk_size.max(1) as usize,
            overlap_size: config.overlap_size.max(0) as usize,
        }
    }

    /// Structure-aware chunking: an optional preamble plus one chunk (or a
    /// run of overlapping sub-chunks) per section that has a body.
    fn chunk_by_headings(&self, content: &str, metadata: &DocumentMetadata) -> Vec<DocumentChunk> {
        let headings = find_wiki_headings(content);
        let mut chunks = Vec::new();
        if headings.is_empty() {
            // No headings: the whole document is one chunk (markdown
            // chunker convention).
            push_chunk(&mut chunks, content, 0, content.len(), &metadata.extra);
            return chunks;
        }

        // Preamble: text before the first heading (kept only when non-blank).
        let first = headings[0].pos;
        if first > 0 && !content[..first].trim().is_empty() {
            push_chunk(&mut chunks, content, 0, first, &metadata.extra);
        }

        for (idx, heading) in headings.iter().enumerate() {
            let end = headings.get(idx + 1).map_or(content.len(), |next| next.pos);
            let section = &content[heading.pos..end];
            // A section without a body carries no content to index.
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

    /// Plain fixed-size chunking of the whole content (`"fixed"` strategy).
    fn chunk_fixed(&self, content: &str, metadata: &DocumentMetadata) -> Vec<DocumentChunk> {
        let mut chunks = Vec::new();
        for (start, end) in fixed_spans(content, self.max_chunk_size, self.overlap_size) {
            push_chunk(&mut chunks, content, start, end, &metadata.extra);
        }
        chunks
    }
}

impl Chunker for MediawikiChunker {
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
                self.chunk_by_headings(content, metadata)
            }
            ChunkingStrategy::Fixed => self.chunk_fixed(content, metadata),
            ChunkingStrategy::Unknown(word) => {
                return Err(IngestionError::UnknownStrategy(word.clone()));
            }
        })
    }
}

/// One wikitext heading found in the content.
struct Heading {
    /// Heading level (1-6, the number of leading `=`).
    level: u8,
    /// Heading text, trimmed (wikitext markup like `[[...]]` kept;
    /// `clean_heading` strips it for breadcrumbs only).
    text: String,
    /// Byte offset of the heading line's first byte in the content.
    pos: usize,
}

/// Extracts all wikitext headings with their byte positions.
fn find_wiki_headings(content: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut offset = 0;
    for line in content.split('\n') {
        if let Some((level, text)) = parse_wiki_heading(line) {
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

/// Parses one line as a wikitext heading: 1-6 leading `=`, the same number
/// of trailing `=`, and non-empty text in between (spaces around the title
/// are optional, as in MediaWiki).
fn parse_wiki_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim();
    let leading = trimmed.bytes().take_while(|&b| b == b'=').count();
    if !matches!(leading, 1..=6) {
        return None;
    }
    let trailing = trimmed.bytes().rev().take_while(|&b| b == b'=').count();
    if leading != trailing {
        return None;
    }
    let inner = trimmed.get(leading..trimmed.len() - trailing)?;
    let text = inner.trim();
    if text.is_empty() {
        return None;
    }
    Some((leading as u8, text))
}

/// Strips wikitext markup from a heading for breadcrumb display:
/// `[[link|alt]]` -> `alt`, `[[link]]` -> `link`, then `'''`, `''` and
/// backticks.
fn clean_heading(text: &str) -> String {
    unwrap_links(text)
        .replace("'''", "")
        .replace("''", "")
        .replace('`', "")
        .trim()
        .to_owned()
}

/// `[[link|alt]]` -> `alt`, `[[link]]` -> `link`; unbalanced markup is kept
/// as-is.
fn unwrap_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(close) = after.find("]]") else {
            out.push_str(&rest[start..]); // Unbalanced: keep `[[` and rest.
            rest = "";
            break;
        };
        let inner = &after[..close];
        // `[[link|alt]]` displays the alt text: the first pipe separates
        // target from display text, and the display text may itself contain
        // pipes (MediaWiki semantics).
        out.push_str(inner.split_once('|').map_or(inner, |(_, alt)| alt));
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Multi-line breadcrumb of the heading path (same shape as the markdown
/// chunker's): one `> Title` line per ancestor, indented by depth, most
/// recent last.
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
/// blank.
fn is_header_only(section: &str) -> bool {
    section.lines().skip(1).all(|line| line.trim().is_empty())
}

/// Fixed-size spans `(start, end)` in byte offsets over `content`, split at
/// character boundaries: `max`/`overlap` are character counts (the config's
/// documented units). Same algorithm as the markdown chunker's.
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
/// `search_text` (search-text-embedding design D1) defaults to `text`: this
/// task scopes the breadcrumb-context `search_text` to the Markdown chunker,
/// so the mediawiki chunker sets the pure-slice text as its search text.
fn push_chunk(
    chunks: &mut Vec<DocumentChunk>,
    content: &str,
    start: usize,
    end: usize,
    metadata: &Map<String, Value>,
) {
    let text = content[start..end].to_owned();
    chunks.push(DocumentChunk {
        text: text.clone(),
        search_text: text,
        sequence_num: chunks.len(),
        start_offset: start,
        end_offset: end,
        metadata: metadata.clone(),
    });
}

#[cfg(test)]
mod tests {
    //! These tests pin this chunker's own contract: the wikitext heading
    //! matcher, the section algorithm, and the byte-offset invariant.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use config::preset::ChunkingStrategy;

    use super::*;

    fn wiki_config(strategy: ChunkingStrategy, max: i32, overlap: i32) -> MarkdownChunkerConfig {
        MarkdownChunkerConfig {
            strategy,
            max_chunk_size: max,
            overlap_size: overlap,
            // Deliberately 0: the value must not gate splitting.
            min_section_size: 0,
        }
    }

    fn chunker(max: i32, overlap: i32) -> MediawikiChunker {
        MediawikiChunker::new(wiki_config(ChunkingStrategy::Headers, max, overlap))
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
    /// numbered consecutively.
    fn assert_invariant(content: &str, chunks: &[DocumentChunk]) {
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(
                &content[c.start_offset..c.end_offset],
                c.text,
                "chunk {i} must be a pure slice"
            );
            assert_eq!(c.sequence_num, i, "chunk {i} sequence");
        }
    }

    #[test]
    fn sections_are_split_at_wiki_headings() {
        let content = "== API Gateway ==\nA service mesh component.\n\n== Database ==\nA data storage system.";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(titles(&chunks), vec!["API Gateway", "Database"]);
        assert_eq!(
            chunks.iter().map(breadcrumb).collect::<Vec<_>>(),
            vec![Some("> API Gateway"), Some("> Database")]
        );
        // The text stays a pure slice starting at the heading line.
        assert_eq!(
            chunks[0].text,
            "== API Gateway ==\nA service mesh component.\n\n"
        );
        assert_eq!(chunks[1].text, "== Database ==\nA data storage system.");
    }

    #[test]
    fn breadcrumbs_follow_the_heading_hierarchy() {
        let content = "== Chapter ==\nchapter intro\n\n=== Section 1 ===\ndeep content here";
        let chunks = chunker(1000, 100)
            .chunk(content, &DocumentMetadata::default())
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(titles(&chunks), vec!["Chapter", "Section 1"]);
        assert_eq!(breadcrumb(&chunks[0]), Some("> Chapter"));
        assert_eq!(breadcrumb(&chunks[1]), Some("> Chapter\n > Section 1"));
        assert_eq!(
            chunks[1]
                .metadata
                .get("heading_level")
                .and_then(Value::as_u64),
            Some(3)
        );
    }

    #[test]
    fn header_only_sections_are_skipped() {
        let cases = [
            (
                "== A ==\n\n=== A.1 ===\ntext under a1\n\n=== A.2 ===\ntext under a2",
                2,
            ),
            (
                "== Title ==\nintro text\n\n== Section 1 ==\nbody one\n\n== Section 2 ==\nbody two",
                3,
            ),
            ("== A ==\n\n=== B ===\n\n==== C ====", 0),
            (
                "== Title ==\nintro\n\n== Section 1 ==\nbody here\n\n== Empty ==",
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
    fn preamble_and_headingless_documents() {
        let content = "Intro text before any section.\n\n== Section ==\nbody";
        let metadata = DocumentMetadata {
            source_file: "space/page.json".to_owned(),
            ..Default::default()
        };
        let chunks = chunker(1000, 100).chunk(content, &metadata).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_invariant(content, &chunks);
        assert_eq!(chunks[0].text, "Intro text before any section.\n\n");
        // The preamble chunk's bag carries no section keys, and the document
        // has no `extra` keys of its own (the typed fields stay on the
        // document, not in the chunk's bag).
        assert!(
            chunks[0].metadata.is_empty(),
            "preamble bag: {:?}",
            chunks[0].metadata
        );
        assert_eq!(titles(&chunks[1..]), vec!["Section"]);

        let plain = "Just plain wikitext without headings.";
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
        let content = format!("== Sec ==\n{}", "a".repeat(300));
        let chunks = chunker(100, 20)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // The 310-char section ("== Sec ==\n" + 300) splits at step 80:
        // 100 + 100 + 100 + 70.
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
        assert!(chunks[0].text.starts_with("== Sec ==\n"));
        assert_eq!(chunks[3].end_offset, content.len());
    }

    #[test]
    fn zero_overlap_is_preserved() {
        let content = format!("== S ==\n{}", "y".repeat(25));
        let chunks = chunker(10, 0)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // The 34-char section splits at step 10: 10 + 10 + 10 + 4.
        assert_eq!(chunks.len(), 4);
        assert_invariant(&content, &chunks);
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
        let content = format!("== Раздел ==\n{}", "я".repeat(150));
        let chunks = chunker(50, 10)
            .chunk(&content, &DocumentMetadata::default())
            .unwrap();
        // The 163-char section ("== Раздел ==\n" + 150) splits at step 40:
        // 50 + 50 + 50 + 43.
        assert_eq!(chunks.len(), 4);
        assert_invariant(&content, &chunks);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 50));
        // Consecutive sub-chunks overlap by exactly 10 characters.
        for (prev, next) in chunks.iter().zip(chunks.iter().skip(1)) {
            let tail: String = prev.text.chars().skip(40).collect();
            let head: String = next.text.chars().take(10).collect();
            assert_eq!(tail, head, "10-char overlap");
        }
    }

    #[test]
    fn fixed_strategy_splits_the_whole_document() {
        let metadata = DocumentMetadata::default();
        let fixed = MediawikiChunker::new(wiki_config(ChunkingStrategy::Fixed, 100, 0));
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
        let content = "== Root ==\nintro\n\n== Section 1 ==\nbody text here";
        let metadata = DocumentMetadata::default();
        let headers = chunker(1000, 100).chunk(content, &metadata).unwrap();
        let hybrid = MediawikiChunker::new(wiki_config(ChunkingStrategy::Hybrid, 1000, 100))
            .chunk(content, &metadata)
            .unwrap();
        assert_eq!(headers, hybrid);
        assert_eq!(headers.len(), 2);
        // Sibling headings are not ancestors (breadcrumbs walk strictly
        // lower levels only).
        assert_eq!(breadcrumb(&headers[1]), Some("> Section 1"));
    }

    #[test]
    fn unknown_strategy_is_an_explicit_error() {
        let chunker = MediawikiChunker::new(wiki_config(
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
    fn heading_matcher_accepts_balanced_equals_only() {
        let cases = [
            ("== Title ==", Some((2u8, "Title"))),
            ("= One =", Some((1, "One"))),
            ("==Title==", Some((2, "Title"))),
            ("=== Sub ===", Some((3, "Sub"))),
            ("====== Six ======", Some((6, "Six"))),
            ("  == padded ==", Some((2, "padded"))),
            ("== link [[Page Name]] ==", Some((2, "link [[Page Name]]"))),
            ("== x", None),                  // no closing equals
            ("===x==", None),                // unbalanced
            ("====", None),                  // empty title
            ("== ==", None),                 // whitespace-only title
            ("======= seven =======", None), // 7 > 6
            ("plain text", None),
            ("", None),
        ];
        for (line, want) in cases {
            assert_eq!(parse_wiki_heading(line), want, "line: {line:?}");
        }
    }

    #[test]
    fn clean_heading_strips_wikitext_markup() {
        let cases = [
            ("[[Page Name]]", "Page Name"),
            ("[[Page Name|Display Name]]", "Display Name"),
            ("'''Bold''' title", "Bold title"),
            ("''Italic'' and `code`", "Italic and code"),
            ("[[a|b|c]]", "b|c"),
            ("unbalanced [[kept", "unbalanced [[kept"),
        ];
        for (input, want) in cases {
            assert_eq!(clean_heading(input), want);
        }
    }

    #[test]
    fn document_extra_keys_ride_along_in_the_chunk_bag() {
        // The document's `extra` keys (the parser's `title`/`image_paths`
        // here) ride along into every chunk's bag alongside the
        // chunk-specific keys (design D2); the document's typed fields stay
        // on the document.
        let mut meta = DocumentMetadata {
            source_type: "mediawiki".to_owned(),
            source_file: "space/page.json".to_owned(),
            ..Default::default()
        };
        meta.extra
            .insert("title".to_owned(), Value::String("Page".to_owned()));
        meta.extra.insert(
            "image_paths".to_owned(),
            Value::Array(vec![Value::String("a.png".to_owned())]),
        );

        let content = "== Sec ==\nbody";
        let chunks = chunker(1000, 100).chunk(content, &meta).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].metadata.get("title"),
            Some(&Value::String("Page".to_owned()))
        );
        assert!(chunks[0].metadata.get("image_paths").is_some());
        assert_eq!(
            chunks[0].metadata.get("section_title"),
            Some(&Value::String("Sec".to_owned()))
        );
    }
}
