//! Batch clustering, canonical prototypes and metadata scoping (design D8).
//!
//! Oracle mapping: the pure parts of
//! `../synopsis/internal/ingestion/entities/resolver.go` (`clusterBatch`,
//! `canonicalProto`, `scopeEntityMetadata`, `unionFind`). The persistent
//! blocking-index resolver (task 2.8) builds on these primitives; nothing
//! here touches the database.
//!
//! # Deliberate deviations
//!
//! - Block keys are `(domain, type, bigram)` tuples instead of the oracle's
//!   concatenated `"domain:type:bigram"` strings — the same partitioning,
//!   with no separator-collision risk.
//! - [`canonical_proto`] ranks names by rune count, not the oracle's UTF-8
//!   byte length (byte length is an encoding artifact that misorders
//!   mixed-script names of equal rune count; ASCII parity is unaffected).
//!
//! Domain keys are normalized with the same rule the NER providers tag
//! with (`crate::ner::normalize`, oracle `utils.Normalize`), so `"HR"` and
//! `" hr "` block together.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::similarity::{bigrams, jaro_winkler};
use crate::ner::{NerEntity, normalize};

/// Groups entities into clusters of similar names (oracle `clusterBatch`).
///
/// Bigram blocking: two entities are compared only when they share a
/// `(normalized domain, type, bigram)` block — cross-domain or cross-type
/// pairs are never merged. Shared pairs with Jaro-Winkler similarity at or
/// above `threshold` are unioned, so chains of transitively similar names
/// land in one cluster. Clusters keep first-seen order; members keep input
/// order within a cluster.
pub fn cluster_batch(entities: &[NerEntity], threshold: f64) -> Vec<Vec<NerEntity>> {
    let n = entities.len();
    if n == 0 {
        return Vec::new();
    }

    let mut blocks: HashMap<(String, String, String), Vec<usize>> = HashMap::new();
    for (i, entity) in entities.iter().enumerate() {
        let domain = normalize(&entity.domain);
        for bigram in bigrams(&entity.name) {
            blocks
                .entry((domain.clone(), entity.entity_type.clone(), bigram))
                .or_default()
                .push(i);
        }
    }

    let mut uf = UnionFind::new(n);
    let mut checked: HashSet<(usize, usize)> = HashSet::new();
    for indices in blocks.values() {
        if indices.len() < 2 {
            continue;
        }
        for (pos, &a) in indices.iter().enumerate() {
            for &b in &indices[pos + 1..] {
                let (lo, hi) = (a.min(b), a.max(b));
                if lo == hi || !checked.insert((lo, hi)) {
                    continue;
                }
                if jaro_winkler(&entities[lo].name, &entities[hi].name) >= threshold {
                    uf.union(lo, hi);
                }
            }
        }
    }

    // Group by root, keeping first-seen cluster order.
    let mut groups: HashMap<usize, Vec<NerEntity>> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    for (i, entity) in entities.iter().enumerate() {
        let root = uf.find(i);
        let group = groups.entry(root).or_default();
        if group.is_empty() {
            order.push(root);
        }
        group.push(entity.clone());
    }

    let mut clusters = Vec::with_capacity(order.len());
    for root in order {
        if let Some(cluster) = groups.remove(&root) {
            clusters.push(cluster);
        }
    }
    clusters
}

/// The cluster member with the longest name; ties resolve to the
/// first-encountered member (oracle `canonicalProto`).
///
/// `cluster` must be non-empty (as produced by [`cluster_batch`]).
pub fn canonical_proto(cluster: &[NerEntity]) -> &NerEntity {
    let mut best = &cluster[0];
    for entity in &cluster[1..] {
        if entity.name.chars().count() > best.name.chars().count() {
            best = entity;
        }
    }
    best
}

/// Document-level metadata fields that must not leak into entity metadata
/// (oracle `docLevelFields`).
const DOCUMENT_LEVEL_FIELDS: &[&str] = &["url", "image_paths", "page_links", "categories"];

/// Filters entity metadata down to entity-scoped fields (oracle
/// `scopeEntityMetadata`).
///
/// Document-level fields ([`DOCUMENT_LEVEL_FIELDS`]) are dropped
/// case-insensitively; provenance fields (`source_file`, `source_type`,
/// `space`, …) are kept. If `raw` contains a string `title`, it is
/// rewritten to the entity name; a non-string `title` is kept as-is.
pub fn scope_entity_metadata(entity_name: &str, raw: &Map<String, Value>) -> Map<String, Value> {
    let mut scoped = Map::new();
    for (key, value) in raw {
        let doc_level = DOCUMENT_LEVEL_FIELDS
            .iter()
            .any(|field| key.eq_ignore_ascii_case(field));
        if !doc_level {
            scoped.insert(key.clone(), value.clone());
        }
    }
    if raw.get("title").is_some_and(Value::is_string) {
        scoped.insert("title".to_string(), Value::String(entity_name.to_string()));
    }
    scoped
}

/// Disjoint-set with path compression (oracle `unionFind`).
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
        }
    }

    fn find(&mut self, i: usize) -> usize {
        let mut root = i;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut current = i;
        while self.parent[current] != root {
            let next = self.parent[current];
            self.parent[current] = root;
            current = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are static).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    /// Test entity builder (oracle tests set only name/type/domain).
    fn entity(name: &str, entity_type: &str, domain: &str) -> NerEntity {
        NerEntity {
            name: name.to_string(),
            entity_type: entity_type.to_string(),
            description: String::new(),
            confidence: 1.0,
            domain: domain.to_string(),
            metadata: Map::new(),
        }
    }

    /// Oracle `TestAddEntitiesBatchDedup` "ascii synonyms merged" (pure
    /// clustering part — the DB assertions belong to task 2.8).
    #[test]
    fn cluster_batch_merges_ascii_synonyms() {
        let clusters = cluster_batch(
            &[
                entity("Apple Inc.", "ORGANIZATION", ""),
                entity("Apple", "ORGANIZATION", ""),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 1);
        assert_eq!(canonical_proto(&clusters[0]).name, "Apple Inc.");
    }

    /// Oracle `TestAddEntitiesBatchDedup` "cyrillic initials merged".
    #[test]
    fn cluster_batch_merges_cyrillic_initials() {
        let clusters = cluster_batch(
            &[
                entity("Стив Джобс", "PERSON", ""),
                entity("С. Джобс", "PERSON", ""),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 1);
        assert_eq!(canonical_proto(&clusters[0]).name, "Стив Джобс");
    }

    /// Oracle `TestAddEntitiesBatchDedup` "different types not merged".
    #[test]
    fn cluster_batch_keeps_different_types_apart() {
        let clusters = cluster_batch(
            &[
                entity("Apple", "ORGANIZATION", ""),
                entity("Стив Джобс", "PERSON", ""),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 2);
    }

    /// Oracle `TestAddEntitiesBatchDedup` "different names not merged".
    #[test]
    fn cluster_batch_keeps_different_names_apart() {
        let clusters = cluster_batch(
            &[
                entity("Иван Иванов", "PERSON", ""),
                entity("Петр Петров", "PERSON", ""),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 2);
    }

    /// Oracle `TestAddEntities_DomainIsolation`: identical (name, type) in
    /// different domains never merge.
    #[test]
    fn cluster_batch_isolates_domains() {
        let clusters = cluster_batch(
            &[
                entity("Архитектор", "ROLE", "construction"),
                entity("Архитектор", "ROLE", "it"),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 2);
    }

    /// Oracle `TestAddEntities_SameDomainDedup`.
    #[test]
    fn cluster_batch_dedups_same_domain() {
        let clusters = cluster_batch(
            &[
                entity("Архитектор", "ROLE", "construction"),
                entity("Архитектор", "ROLE", "construction"),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 1);
    }

    /// Oracle `TestAddEntities_DomainNormalization`: domains differing only
    /// in case/whitespace block together.
    #[test]
    fn cluster_batch_normalizes_domain_keys() {
        let clusters = cluster_batch(
            &[
                entity("Alice", "PERSON", "HR"),
                entity("Alice", "PERSON", " hr "),
            ],
            0.8,
        );
        assert_eq!(clusters.len(), 1);
    }

    /// Oracle `TestAddEntitiesEmpty`.
    #[test]
    fn cluster_batch_empty_input_yields_no_clusters() {
        assert!(cluster_batch(&[], 0.8).is_empty());
    }

    /// Transitivity: A~B and B~C merge into one cluster even though A~C is
    /// below the threshold (union-find over checked pairs).
    #[test]
    fn cluster_batch_merges_transitive_chains() {
        let clusters = cluster_batch(
            &[
                entity("Apple", "ORGANIZATION", ""),
                entity("Apple Inc.", "ORGANIZATION", ""),
                entity("Inc Apple", "ORGANIZATION", ""),
            ],
            0.6,
        );
        // apple~apple inc. = 0.9, apple inc.~inc apple = 0.617,
        // but apple~inc apple = 0.437 < 0.6.
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 3);
        assert_eq!(canonical_proto(&clusters[0]).name, "Apple Inc.");
    }

    /// Oracle `canonicalProto` semantics: longest name wins.
    #[test]
    fn canonical_proto_longest_name_wins() {
        let cluster = vec![
            entity("Apple", "ORGANIZATION", ""),
            entity("Apple Inc.", "ORGANIZATION", ""),
        ];
        assert_eq!(canonical_proto(&cluster).name, "Apple Inc.");
    }

    /// Ties resolve to the first-encountered member.
    #[test]
    fn canonical_proto_tie_goes_to_first() {
        let cluster = vec![entity("Вера", "PERSON", ""), entity("Варя", "PERSON", "")];
        assert_eq!(canonical_proto(&cluster).name, "Вера");
    }

    /// Oracle `TestScopeEntityMetadata` — full parity port.
    #[test]
    fn scope_entity_metadata_matches_oracle_cases() {
        // (entity name, raw pairs, expected keys, forbidden keys, expected title)
        type ScopeCase<'a> = (
            &'a str,
            Vec<(&'a str, Value)>,
            Vec<&'a str>,
            Vec<&'a str>,
            Option<&'a str>,
        );
        let cases: [ScopeCase; 7] = [
            ("Alice", vec![], vec![], vec![], None),
            (
                "Alice",
                vec![
                    ("provider", json!("internal")),
                    ("confidence", json!(0.95)),
                    ("title", json!("Software Engineer")),
                ],
                vec!["provider", "confidence"],
                vec![],
                Some("Alice"),
            ),
            (
                "Alice",
                vec![
                    ("url", json!("https://example.com/doc.md")),
                    ("image_paths", json!(["img.png"])),
                    ("page_links", json!(["#section-1"])),
                    ("categories", json!(["engineering"])),
                    ("source_file", json!("/path/to/file.md")),
                    ("provider", json!("internal")),
                ],
                vec!["provider", "source_file"],
                vec!["url", "image_paths", "page_links", "categories"],
                None,
            ),
            (
                "Alice Smith",
                vec![
                    ("title", json!("Software Engineer")),
                    ("provider", json!("internal")),
                ],
                vec!["title", "provider"],
                vec![],
                Some("Alice Smith"),
            ),
            (
                "Bob",
                vec![
                    ("URL", json!("https://example.com")),
                    ("source_file", json!("/path/to/file.md")),
                    ("provider", json!("internal")),
                ],
                vec!["source_file", "provider"],
                vec!["URL"],
                None,
            ),
            (
                "Charlie",
                vec![
                    ("url", json!("https://example.com")),
                    ("image_paths", json!(["img.png"])),
                    ("source_file", json!("/path/to/file.md")),
                ],
                vec!["source_file"],
                vec![],
                None,
            ),
            // Non-string title: kept as-is, NOT rewritten to the entity name.
            (
                "Dave",
                vec![("title", json!(42)), ("provider", json!("internal"))],
                vec!["title", "provider"],
                vec![],
                None,
            ),
        ];

        for (entity_name, raw_pairs, want_keys, not_want, want_title) in cases {
            let raw: Map<String, Value> = raw_pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            let scoped = scope_entity_metadata(entity_name, &raw);

            if want_keys.is_empty() {
                assert!(
                    scoped.is_empty(),
                    "{entity_name}: expected empty, got {scoped:?}"
                );
                continue;
            }
            for key in want_keys {
                assert!(
                    scoped.contains_key(key),
                    "{entity_name}: missing key {key:?} in {scoped:?}"
                );
            }
            if let Some(title) = want_title {
                assert_eq!(
                    scoped.get("title"),
                    Some(&Value::String(title.to_string())),
                    "{entity_name}: title not rewritten to entity name: {scoped:?}"
                );
            }
            for key in not_want {
                assert!(
                    !scoped.contains_key(key),
                    "{entity_name}: forbidden key {key:?} in {scoped:?}"
                );
            }
        }
    }
}
