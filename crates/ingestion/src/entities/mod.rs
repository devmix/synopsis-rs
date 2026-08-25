//! Entity resolution primitives (change `ingestion-ner`, design D8).
//!
//! Oracle reference: `../synopsis/internal/ingestion/entities/` —
//! `similarity.go` (name normalization, bigrams, Jaro-Winkler) and the pure
//! parts of `resolver.go` (batch clustering, canonical prototype, metadata
//! scoping). Everything here is pure: no DB, no I/O — the persistent
//! blocking-index resolver (task 2.8) is built on these primitives.
//!
//! Deliberate deviations from the oracle (recorded per the no-1:1-copy
//! directive, see the module docs of [`similarity`] and [`cluster`]):
//! sub-two-rune names map to their *normalized* form in bigrams (the
//! oracle leaked raw untrimmed input into block keys), and the canonical
//! prototype is ranked by rune count, not UTF-8 byte length.

mod cluster;
mod similarity;

pub use cluster::{canonical_proto, cluster_batch, scope_entity_metadata};
pub use similarity::{bigrams, jaro_winkler, normalize_name};
