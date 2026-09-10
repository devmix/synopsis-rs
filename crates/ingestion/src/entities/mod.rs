//! Entity resolution (change `ingestion-ner`, design D8 + D9).
//!
//! The similarity primitives (name normalization, the resolution tier keys,
//! bigrams, Jaro-Winkler) and the resolver (batch clustering, canonical
//! prototype, metadata scoping, and the persistent `Resolver`) are described
//! below. The pure primitives ([`normalize_name`], [`match_key`], [`stem_key`],
//! [`bigrams`], [`jaro_winkler`], [`cluster_batch`], [`canonical_proto`],
//! [`scope_entity_metadata`]) carry no I/O; the persistent blocking-index
//! resolver ([`Resolver`], task 2.8) is built on them and talks to the
//! database through the db crate's DAOs.
//!
//! Design decisions (see the module docs of the similarity, cluster and
//! resolver modules): sub-two-rune names map to their *normalized* form in
//! bigrams (raw untrimmed input would leak stray whitespace into block keys),
//! and the canonical prototype is ranked by rune count, not UTF-8 byte length.
//!
//! All public items are re-exported at the crate root (task 2.9), so
//! downstream crates reference `ingestion::Resolver`,
//! `ingestion::jaro_winkler`, … directly.

mod cluster;
mod resolver;
mod similarity;

pub use cluster::{canonical_proto, cluster_batch, scope_entity_metadata};
pub use resolver::{EntityChanges, ResolvedEntity, Resolver};
pub use similarity::{bigrams, jaro_winkler, match_key, normalize_name, stem_key};
