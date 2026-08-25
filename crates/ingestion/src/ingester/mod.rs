//! Per-document ingestion pipeline (series change 3, design D2/D3).
//!
//! Task 3.3 lands the pipeline's pure helpers — content hashing, quote
//! extraction (design D7) and source-type resolution — in [`helpers`].
//! The `Ingester` struct, the document loop and the fact/backup stages
//! arrive in tasks 3.4–3.6.

mod helpers;

pub use helpers::{compute_content_hash, extract_quote_from_chunk, source_type_from_metadata};
