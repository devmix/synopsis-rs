//! Entity resolution (change `ingestion-ner`, design D8 + D9).
//!
//! Oracle reference: `../synopsis/internal/ingestion/entities/` —
//! `similarity.go` (name normalization, bigrams, Jaro-Winkler) and
//! `resolver.go` (batch clustering, canonical prototype, metadata scoping,
//! and the persistent `Resolver`). The pure primitives ([`normalize_name`],
//! [`bigrams`], [`jaro_winkler`], [`cluster_batch`], [`canonical_proto`],
//! [`scope_entity_metadata`]) carry no I/O; the persistent blocking-index
//! resolver ([`Resolver`], task 2.8) is built on them and talks to the
//! database through the db crate's DAOs.
//!
//! Deliberate deviations from the oracle (recorded per the no-1:1-copy
//! directive, see the module docs of the similarity, cluster and resolver
//! modules): sub-two-rune names map to their *normalized* form in
//! bigrams (the oracle leaked raw untrimmed input into block keys), and the
//! canonical prototype is ranked by rune count, not UTF-8 byte length.
//!
//! All public items are re-exported at the crate root (task 2.9), so
//! downstream crates reference `ingestion::Resolver`,
//! `ingestion::jaro_winkler`, … directly.

mod cluster;
mod resolver;
mod similarity;

pub use cluster::{canonical_proto, cluster_batch, scope_entity_metadata};
pub use resolver::{ResolvedEntity, Resolver};
pub use similarity::{bigrams, jaro_winkler, normalize_name};
