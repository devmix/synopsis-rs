//! DB-backed entity resolver: persistent deduplication of NER entities
//! (design D9).
//!
//! The persistent resolver and its helpers (`hydrate`/`rehydrate`/`index`,
//! `resolve_one`, `find_best_candidate`, `lookup`, `lookup_or_create`,
//! `lookup_or_create_with_stats`, `add_entities`). The pure primitives
//! (batch clustering, canonical prototype, name similarity, metadata
//! scoping) live in [`super::cluster`] and [`super::similarity`] (task 2.7)
//! and are reused as-is.
//!
//! # Locking strategy
//!
//! Every operation holds the resolver's single [`Mutex`] for the whole
//! operation — hydrate, resolution and DB writes included — behind one
//! global lock. This is safe with the db crate's DAO shape: the DAOs are
//! bound to a [`ConnectionOrTx`] handle that is independent of the
//! resolver's lock, each DAO call acquires and releases the connection's
//! own internal lock, and no DAO callback ever re-enters the resolver —
//! the two lock domains never nest in a cycle, so holding the index lock
//! across DB calls cannot deadlock. A plain [`Mutex`] (rather than an
//! `RwLock`) is the simplest correct strategy here, because every operation
//! can mutate the index (even [`Resolver::lookup`] hydrates it on first use)
//! and the critical section is dominated by DB I/O anyway.
//!
//! # Design decisions
//!
//! - [`Resolver::lookup`] returns `Vec<Option<i64>>` aligned with the input
//!   (Rust idiom) rather than a dense `Vec<i64>` with `0` = "not found".
//! - [`Resolver::add_entities`] returns [`ResolvedEntity`] rows (id + the
//!   extracted fields) rather than full entity stubs: the row-only columns
//!   (`created_at`, `metadata_json`) are unknown for freshly created
//!   entities, and a stub carrying an empty `created_at` would be a lie.
//! - Canonical-name promotion compares RUNE counts (task 2.7 semantics)
//!   rather than UTF-8 byte length.
//! - The GC-missing-candidate recovery (design D9) is bounded to ONE
//!   rehydrate + retry: after a full rehydrate the candidate id comes
//!   from the database listing itself, so a second miss is the defensive
//!   [`IngestionError::EntityCandidateGone`] error, not another retry.
//!
//! Block keys are `(normalized domain, entity type, bigram)` tuples and name
//! keys are `"domain:normalized_name"` strings, so cross-domain and
//! cross-type entities never merge.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use config::preset::ResolverConfig;
use db::{ConnectionOrTx, EntityDao, EntitySourceDao};

use super::cluster::{canonical_proto, cluster_batch, scope_entity_metadata};
use super::similarity::{bigrams, jaro_winkler, normalize_name};
use crate::error::IngestionError;
use crate::ner::{NerEntity, normalize};

/// Deduplicates NER entities by name similarity against both the current
/// batch and previously persisted entities, then records document
/// provenance (design D9).
///
/// The resolver is stateless with respect to the database: persistence
/// goes through the [`ConnectionOrTx`] handle passed to each operation, so
/// the same instance works over a pooled connection or inside an in-flight
/// pipeline transaction. The in-memory blocking index is the only
/// long-lived state (see the module docs for the locking strategy).
pub struct Resolver {
    threshold: f64,
    state: Mutex<BlockingIndex>,
}

impl Resolver {
    /// Creates a resolver with the given Jaro-Winkler merge threshold
    /// (the config preset loader applies the 0.8 default; see
    /// [`ResolverConfig`]).
    #[must_use]
    pub fn new(similarity_threshold: f64) -> Self {
        Self {
            threshold: similarity_threshold,
            state: Mutex::new(BlockingIndex::new()),
        }
    }

    /// Creates a resolver from the config's resolver settings.
    #[must_use]
    pub fn from_config(config: &ResolverConfig) -> Self {
        Self::new(config.similarity_threshold)
    }

    /// Resolves each entity against the hydrated index WITHOUT creating
    /// anything. Returns ids aligned with the input; `None` means no
    /// candidate at or above the threshold.
    pub fn lookup(
        &self,
        exec: ConnectionOrTx<'_>,
        entities: &[NerEntity],
    ) -> Result<Vec<Option<i64>>, IngestionError> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        let dao = EntityDao::new(exec);
        let mut state = lock(&self.state);
        state.hydrate(&dao)?;
        Ok(entities
            .iter()
            .map(|entity| {
                state
                    .find_best_candidate(entity)
                    .filter(|candidate| candidate.score >= self.threshold)
                    .map(|candidate| candidate.id)
            })
            .collect())
    }

    /// Resolves each entity against the hydrated index, creating missing
    /// ones, and returns the resolved ids aligned with the input. Created
    /// entities are linked to `doc_id`.
    pub fn lookup_or_create(
        &self,
        exec: ConnectionOrTx<'_>,
        doc_id: i64,
        entities: &[NerEntity],
    ) -> Result<Vec<i64>, IngestionError> {
        let (ids, _) = self.lookup_or_create_with_stats(exec, doc_id, entities)?;
        Ok(ids)
    }

    /// Resolves each entity against the hydrated index, creating missing
    /// ones (per-entity, no batch clustering). Returns the resolved ids
    /// aligned with the input plus the number of newly created entities;
    /// created ids are linked to `doc_id` via `entity_sources`.
    pub fn lookup_or_create_with_stats(
        &self,
        exec: ConnectionOrTx<'_>,
        doc_id: i64,
        entities: &[NerEntity],
    ) -> Result<(Vec<i64>, usize), IngestionError> {
        if entities.is_empty() {
            return Ok((Vec::new(), 0));
        }
        if doc_id <= 0 {
            return Err(IngestionError::InvalidDocumentId(doc_id));
        }

        let dao = EntityDao::new(exec);
        let sources = EntitySourceDao::new(exec);
        let mut state = lock(&self.state);
        state.hydrate(&dao)?;

        let mut ids = Vec::with_capacity(entities.len());
        let mut created = Vec::new();
        for entity in entities {
            match state.find_best_candidate(entity) {
                Some(candidate) if candidate.score >= self.threshold => {
                    ids.push(candidate.id);
                }
                _ => {
                    let resolved = self.resolve_one(&mut state, &dao, entity)?;
                    created.push(resolved.id);
                    ids.push(resolved.id);
                }
            }
        }
        sources.link_batch(doc_id, &created)?;
        Ok((ids, created.len()))
    }

    /// Normalizes, deduplicates and persists the batch: clusters similar
    /// names first (one canonical per cluster), resolves each canonical
    /// against the database, and links every resolved entity to `doc_id`
    /// for provenance. Returns the resolved entities in cluster order,
    /// deduplicated by id.
    pub fn add_entities(
        &self,
        exec: ConnectionOrTx<'_>,
        doc_id: i64,
        entities: &[NerEntity],
    ) -> Result<Vec<ResolvedEntity>, IngestionError> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        if doc_id <= 0 {
            return Err(IngestionError::InvalidDocumentId(doc_id));
        }

        let dao = EntityDao::new(exec);
        let sources = EntitySourceDao::new(exec);
        let mut state = lock(&self.state);
        state.hydrate(&dao)?;

        let mut resolved = Vec::new();
        let mut ids = Vec::new();
        for cluster in cluster_batch(entities, self.threshold) {
            let canonical = canonical_proto(&cluster);
            let entity = self.resolve_one(&mut state, &dao, canonical)?;
            if !ids.contains(&entity.id) {
                ids.push(entity.id);
                resolved.push(entity);
            }
        }
        sources.link_batch(doc_id, &ids)?;
        Ok(resolved)
    }

    /// Merges `entity` into an existing canonical entity when a similar
    /// candidate is found, otherwise creates a new one. Called with the
    /// index lock already held.
    fn resolve_one(
        &self,
        state: &mut BlockingIndex,
        dao: &EntityDao<'_>,
        entity: &NerEntity,
    ) -> Result<ResolvedEntity, IngestionError> {
        let mut rehydrated = false;
        loop {
            let Some(candidate) = state
                .find_best_candidate(entity)
                .filter(|candidate| candidate.score >= self.threshold)
            else {
                return self.create_entity(state, dao, entity);
            };

            let candidate_id = candidate.id;
            match dao.get_by_id(candidate_id)? {
                Some(mut existing) => {
                    // Promote the longer canonical name (task 2.7 rune
                    // semantics) and keep BOTH names resolving to the id.
                    // `existing.name` is the index's canonical name in the
                    // database (hydration and promotion keep them in sync),
                    // and using the DB copy also ends `candidate`'s borrow
                    // of the index before the mutation below.
                    if entity.name.chars().count() > existing.name.chars().count() {
                        dao.update_name(candidate_id, &entity.name)?;
                        let domain = state.domains[&candidate_id].clone();
                        state
                            .names
                            .insert(name_key(&domain, &existing.name), candidate_id);
                        state
                            .names
                            .insert(name_key(&domain, &entity.name), candidate_id);
                        state.canonical.insert(candidate_id, entity.name.clone());
                        for bigram in bigrams(&entity.name) {
                            state
                                .blocks
                                .entry((domain.clone(), entity.entity_type.clone(), bigram))
                                .or_default()
                                .push(candidate_id);
                        }
                        existing.name = entity.name.clone();
                    }
                    return Ok(ResolvedEntity {
                        id: existing.id,
                        entity_type: existing.entity_type,
                        name: existing.name,
                        domain: existing.domain,
                        description: existing.description,
                        confidence: existing.confidence,
                    });
                }
                // The candidate was deleted mid-run (GC): rebuild the
                // index from the database and retry ONCE (design D9).
                None => {
                    if rehydrated {
                        return Err(IngestionError::EntityCandidateGone(candidate.id));
                    }
                    rehydrated = true;
                    state.rehydrate(dao)?;
                }
            }
        }
    }

    /// Persists a new entity: scoped metadata JSON + description through the
    /// atomic `get_or_create`, indexed in-memory immediately.
    fn create_entity(
        &self,
        state: &mut BlockingIndex,
        dao: &EntityDao<'_>,
        entity: &NerEntity,
    ) -> Result<ResolvedEntity, IngestionError> {
        let domain = normalize(&entity.domain);
        let metadata_json = if entity.metadata.is_empty() {
            None
        } else {
            let scoped = scope_entity_metadata(&entity.name, &entity.metadata);
            (!scoped.is_empty())
                .then(|| serde_json::to_string(&scoped))
                .transpose()
                .map_err(|source| IngestionError::EntityMetadataJson { source })?
        };

        let id = dao.get_or_create(
            &entity.entity_type,
            &entity.name,
            &domain,
            Some(&entity.description),
            Some(entity.confidence),
            metadata_json.as_deref(),
        )?;
        state.index_entity(id, &entity.entity_type, &entity.name, &domain);
        Ok(ResolvedEntity {
            id,
            entity_type: entity.entity_type.clone(),
            name: entity.name.clone(),
            domain,
            description: Some(entity.description.clone()),
            confidence: Some(entity.confidence),
        })
    }
}

/// A resolved (deduplicated) entity: the canonical row's identity plus the
/// fields the caller can rely on (the entity row, minus the row-only columns
/// `created_at`/`metadata_json`, which are unknown for freshly created
/// entities).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEntity {
    /// The canonical entity's database id.
    pub id: i64,
    /// Entity type (as extracted).
    pub entity_type: String,
    /// Canonical name (the promoted one when a longer name was merged).
    pub name: String,
    /// Normalized domain.
    pub domain: String,
    /// Description (stored value for merged entities, input value for
    /// created ones).
    pub description: Option<String>,
    /// Confidence (stored value for merged entities, input value for
    /// created ones).
    pub confidence: Option<f64>,
}

/// The in-memory blocking index (design D9 state): name keys, canonical
/// names, bigram blocks and per-id domains, plus the hydrated flag.
struct BlockingIndex {
    /// `"domain:normalized_name"` → entity id.
    names: HashMap<String, i64>,
    /// entity id → canonical name.
    canonical: HashMap<i64, String>,
    /// `(normalized domain, entity type, bigram)` → entity ids.
    blocks: HashMap<(String, String, String), Vec<i64>>,
    /// entity id → normalized domain.
    domains: HashMap<i64, String>,
    /// Whether the index has been loaded from the database.
    hydrated: bool,
}

impl BlockingIndex {
    fn new() -> Self {
        Self {
            names: HashMap::new(),
            canonical: HashMap::new(),
            blocks: HashMap::new(),
            domains: HashMap::new(),
            hydrated: false,
        }
    }

    /// Registers one entity in the index.
    fn index_entity(&mut self, id: i64, entity_type: &str, name: &str, domain: &str) {
        let domain = normalize(domain);
        self.names.insert(name_key(&domain, name), id);
        self.canonical.insert(id, name.to_string());
        self.domains.insert(id, domain.clone());
        for bigram in bigrams(name) {
            self.blocks
                .entry((domain.clone(), entity_type.to_string(), bigram))
                .or_default()
                .push(id);
        }
    }

    /// Lazily loads the whole `entities` table into the index on the first
    /// call; later calls are a no-op — updates arrive incrementally through
    /// [`Self::index_entity`] and name promotion.
    fn hydrate(&mut self, dao: &EntityDao<'_>) -> Result<(), IngestionError> {
        if self.hydrated {
            return Ok(());
        }
        for entity in dao.list()? {
            self.index_entity(entity.id, &entity.entity_type, &entity.name, &entity.domain);
        }
        self.hydrated = true;
        Ok(())
    }

    /// Discards the index and loads it from the database again: recovery
    /// from entities deleted mid-run by GC.
    fn rehydrate(&mut self, dao: &EntityDao<'_>) -> Result<(), IngestionError> {
        self.names.clear();
        self.canonical.clear();
        self.blocks.clear();
        self.domains.clear();
        self.hydrated = false;
        self.hydrate(dao)
    }

    /// The most similar persisted entity of the same type AND domain: an
    /// exact normalized-name hit scores 1.0, otherwise the best
    /// Jaro-Winkler over the shared bigram blocks.
    /// The candidate's canonical name is NOT carried: the merge path reads
    /// it from the database row (hydration and promotion keep the index's
    /// canonical names and the `entities` table in sync).
    fn find_best_candidate(&self, entity: &NerEntity) -> Option<Candidate> {
        let domain = normalize(&entity.domain);
        if let Some(&id) = self.names.get(&name_key(&domain, &entity.name)) {
            return Some(Candidate { id, score: 1.0 });
        }

        let mut best: Option<(i64, f64)> = None;
        for bigram in bigrams(&entity.name) {
            let Some(ids) = self
                .blocks
                .get(&(domain.clone(), entity.entity_type.clone(), bigram))
            else {
                continue;
            };
            for &id in ids {
                let Some(name) = self.canonical.get(&id) else {
                    continue;
                };
                let score = jaro_winkler(&entity.name, name);
                if best.is_none_or(|(_, current)| score > current) {
                    best = Some((id, score));
                }
            }
        }

        best.map(|(id, score)| Candidate { id, score })
    }
}

/// The best persisted candidate for one entity: id and Jaro-Winkler score
/// (1.0 on an exact normalized-name hit).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    id: i64,
    score: f64,
}

/// `"domain:normalized_name"` — the exact-match key.
fn name_key(domain: &str, name: &str) -> String {
    format!("{domain}:{}", normalize_name(name))
}

/// Locks the index mutex. On poisoning (a previous holder panicked) the
/// guard is recovered and the index discarded: the state is fully
/// rebuildable, so the next operation simply rehydrates from the database.
fn lock(index: &Mutex<BlockingIndex>) -> MutexGuard<'_, BlockingIndex> {
    match index.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.hydrated = false;
            guard
        }
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are static).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db, DocumentDao, EntityDao, EntityFilter, EntitySourceDao};
    use serde_json::{Map, Value};

    use super::*;

    /// Test entity builder (sets only name/type/domain).
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

    struct Fixture {
        db: Db,
        resolver: Resolver,
        doc_id: i64,
    }

    fn fixture() -> Fixture {
        let db = in_memory_db();
        let doc_id = db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                    "markdown",
                    "/test/doc.md",
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        Fixture {
            db,
            resolver: Resolver::new(0.8),
            doc_id,
        }
    }

    /// Runs `op` on a pooled connection, unwrapping the checkout and the
    /// operation result.
    fn run_op<T>(
        fixture: &Fixture,
        op: impl FnOnce(ConnectionOrTx<'_>, &Resolver) -> Result<T, IngestionError>,
    ) -> T {
        fixture
            .db
            .with_conn(|conn| op(ConnectionOrTx::Connection(conn), &fixture.resolver))
            .unwrap()
            .unwrap()
    }

    fn entity_count(fixture: &Fixture) -> i64 {
        fixture
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).count(&EntityFilter::default())
            })
            .unwrap()
            .unwrap()
    }

    fn source_doc_ids(fixture: &Fixture, entity_id: i64) -> Vec<i64> {
        fixture
            .db
            .with_conn(|conn| {
                EntitySourceDao::new(ConnectionOrTx::Connection(conn))
                    .get_documents_by_entity_id(entity_id)
            })
            .unwrap()
            .unwrap()
    }

    fn seed_entity(fixture: &Fixture, entity_type: &str, name: &str) -> i64 {
        fixture
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn)).create(
                    entity_type,
                    name,
                    "",
                    None,
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap()
    }

    fn stored_entity(fixture: &Fixture, id: i64) -> db::Entity {
        fixture
            .db
            .with_conn(|conn| {
                EntityDao::new(ConnectionOrTx::Connection(conn))
                    .get_by_id(id)
                    .map(|row| row.unwrap())
            })
            .unwrap()
            .unwrap()
    }

    // ── add_entities ─────────────────────────────────────────────────────

    /// Batch deduplication cases (merge / no-merge).
    #[test]
    fn add_entities_batch_dedup() {
        let cases: [(&str, Vec<NerEntity>, usize, Option<&str>); 4] = [
            (
                "ascii synonyms merged",
                vec![
                    entity("Apple Inc.", "ORGANIZATION", ""),
                    entity("Apple", "ORGANIZATION", ""),
                ],
                1,
                Some("Apple Inc."),
            ),
            (
                "cyrillic initials merged",
                vec![
                    entity("Стив Джобс", "PERSON", ""),
                    entity("С. Джобс", "PERSON", ""),
                ],
                1,
                Some("Стив Джобс"),
            ),
            (
                "different types not merged",
                vec![
                    entity("Apple", "ORGANIZATION", ""),
                    entity("Стив Джобс", "PERSON", ""),
                ],
                2,
                None,
            ),
            (
                "different names not merged",
                vec![
                    entity("Иван Иванов", "PERSON", ""),
                    entity("Петр Петров", "PERSON", ""),
                ],
                2,
                None,
            ),
        ];
        for (name, entities, want_resolved, want_name) in cases {
            let f = fixture();
            let resolved = run_op(&f, |exec, r| r.add_entities(exec, f.doc_id, &entities));
            assert_eq!(resolved.len(), want_resolved, "{name}");
            assert_eq!(entity_count(&f), want_resolved as i64, "{name}");
            for resolved_entity in &resolved {
                assert!(resolved_entity.id > 0, "{name}");
                assert_eq!(
                    source_doc_ids(&f, resolved_entity.id),
                    vec![f.doc_id],
                    "{name}: linked to the source document"
                );
            }
            if let Some(want) = want_name {
                assert_eq!(resolved[0].name, want, "{name}");
            }
        }
    }

    /// A shorter synonym in a second call reuses the existing entity.
    #[test]
    fn add_entities_incremental_reuses_existing() {
        let f = fixture();
        let first = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple Inc.", "ORGANIZATION", "")])
        });
        let second = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple", "ORGANIZATION", "")])
        });
        assert_eq!(entity_count(&f), 1);
        assert_eq!(
            second[0].id, first[0].id,
            "the shorter synonym must reuse the existing entity"
        );
        assert_eq!(second[0].name, "Apple Inc.", "canonical name preserved");
    }

    /// Case-insensitive exact match reuses the existing entity.
    #[test]
    fn add_entities_case_insensitive_exact_match() {
        let f = fixture();
        let existing = seed_entity(&f, "ORGANIZATION", "Apple");
        let resolved = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("apple", "ORGANIZATION", "")])
        });
        assert_eq!(entity_count(&f), 1);
        assert_eq!(resolved[0].id, existing);
    }

    /// The longer name wins, and the old name still resolves to the same
    /// entity.
    #[test]
    fn add_entities_promotes_canonical_name() {
        let f = fixture();
        let existing = seed_entity(&f, "ORGANIZATION", "Apple");
        let resolved = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple Inc.", "ORGANIZATION", "")])
        });
        assert_eq!(resolved[0].id, existing);
        assert_eq!(stored_entity(&f, existing).name, "Apple Inc.");

        let again = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple", "ORGANIZATION", "")])
        });
        assert_eq!(again[0].id, existing, "the old name must still resolve");
        assert_eq!(entity_count(&f), 1);
    }

    /// Provenance links accumulate across documents.
    #[test]
    fn add_entities_links_provenance_across_documents() {
        let f = fixture();
        let doc2 =
            f.db.with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).create(
                    "markdown",
                    "/test/doc2.md",
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();

        let first = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple", "ORGANIZATION", "")])
        });
        let second = run_op(&f, |exec, r| {
            r.add_entities(exec, doc2, &[entity("Apple", "ORGANIZATION", "")])
        });
        assert_eq!(first[0].id, second[0].id);
        assert_eq!(source_doc_ids(&f, first[0].id), vec![f.doc_id, doc2]);
    }

    /// Empty input yields nothing; an invalid doc id is an error.
    #[test]
    fn add_entities_empty_and_invalid_doc_id() {
        let f = fixture();
        assert!(run_op(&f, |exec, r| r.add_entities(exec, f.doc_id, &[])).is_empty());

        let err =
            f.db.with_conn(|conn| {
                f.resolver.add_entities(
                    ConnectionOrTx::Connection(conn),
                    0,
                    &[entity("Apple", "ORGANIZATION", "")],
                )
            })
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, IngestionError::InvalidDocumentId(0)), "{err}");
    }

    /// The resolver persists through the same transaction that holds the
    /// write lock (a pool connection would fail with "database is locked").
    #[test]
    fn add_entities_within_transaction() {
        let f = fixture();
        let resolved =
            f.db.exec_tx(|tx| {
                let exec = ConnectionOrTx::Transaction(&*tx);
                DocumentDao::new(exec).create("markdown", "/test/txdoc.md", None, None)?;
                f.resolver
                    .add_entities(exec, f.doc_id, &[entity("Junior", "PERSON", "")])
            })
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(entity_count(&f), 1);
    }

    /// Switching the executor handle (pool connection vs transaction) keeps
    /// the in-memory index intact.
    #[test]
    fn index_survives_handle_change() {
        let f = fixture();
        let first = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple", "ORGANIZATION", "")])
        });
        let second =
            f.db.exec_tx(|tx| {
                f.resolver.add_entities(
                    ConnectionOrTx::Transaction(&*tx),
                    f.doc_id,
                    &[entity("Apple Inc.", "ORGANIZATION", "")],
                )
            })
            .unwrap();
        assert_eq!(
            second[0].id, first[0].id,
            "the index must survive the handle change"
        );
    }

    /// Domain isolation, same-domain dedup, and domain normalization.
    #[test]
    fn add_entities_domain_isolation_and_normalization() {
        // Identical (name, type) in DIFFERENT domains: distinct entities.
        let f = fixture();
        let resolved = run_op(&f, |exec, r| {
            r.add_entities(
                exec,
                f.doc_id,
                &[
                    entity("Архитектор", "ROLE", "construction"),
                    entity("Архитектор", "ROLE", "it"),
                ],
            )
        });
        assert_eq!(resolved.len(), 2, "domain isolation");
        assert_ne!(resolved[0].id, resolved[1].id);
        assert_eq!(entity_count(&f), 2);

        // Domains differing only in case/whitespace: one entity, and the
        // stored domain is normalized.
        let f = fixture();
        let resolved = run_op(&f, |exec, r| {
            r.add_entities(
                exec,
                f.doc_id,
                &[
                    entity("Alice", "PERSON", "HR"),
                    entity("Alice", "PERSON", " hr "),
                ],
            )
        });
        assert_eq!(resolved.len(), 1, "normalized domain dedup");
        assert_eq!(entity_count(&f), 1);
        assert_eq!(stored_entity(&f, resolved[0].id).domain, "hr");
    }

    /// Confidence and scoped metadata are persisted.
    #[test]
    fn add_entities_persists_confidence_and_metadata() {
        let f = fixture();
        let mut ent = entity("Alice Smith", "PERSON", "");
        ent.description = "CEO of Acme Corp".to_string();
        ent.confidence = 0.92;
        ent.metadata
            .insert("source".to_string(), Value::String("llm".to_string()));
        ent.metadata
            .insert("model".to_string(), Value::String("gpt-4".to_string()));

        let resolved = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, std::slice::from_ref(&ent))
        });
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].confidence, Some(0.92));

        let stored = stored_entity(&f, resolved[0].id);
        assert_eq!(stored.confidence, Some(0.92));
        assert!(
            stored.metadata_json.is_some(),
            "scoped metadata must be persisted"
        );
        let metadata: Map<String, Value> =
            serde_json::from_str(stored.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            metadata.get("source"),
            Some(&Value::String("llm".to_string()))
        );
        assert_eq!(
            metadata.get("model"),
            Some(&Value::String("gpt-4".to_string()))
        );
    }

    /// Lookup cases: exact hit, similarity merge (0.9), no candidate,
    /// input-order alignment, hydration from the DB, and no creation.
    #[test]
    fn lookup_resolves_without_creating() {
        let f = fixture();
        let apple = seed_entity(&f, "ORGANIZATION", "Apple Inc.");
        let alice = seed_entity(&f, "PERSON", "Alice");
        let acme = seed_entity(&f, "ORGANIZATION", "Acme Corp");
        let before = entity_count(&f);

        let ids = run_op(&f, |exec, r| {
            r.lookup(
                exec,
                &[
                    entity("Apple Inc.", "ORGANIZATION", ""), // exact hit
                    entity("Apple", "ORGANIZATION", ""),      // similar (JW 0.9)
                    entity("Google LLC", "ORGANIZATION", ""), // no candidate >= 0.8
                    entity("Alice", "PERSON", ""),            // exact hit
                    entity("Unknown Person", "PERSON", ""),   // no candidate
                    entity("Acme Corp", "ORGANIZATION", ""),  // exact hit
                ],
            )
        });
        assert_eq!(
            ids,
            vec![
                Some(apple),
                Some(apple),
                None,
                Some(alice),
                None,
                Some(acme)
            ]
        );
        assert_eq!(entity_count(&f), before, "lookup must not create");
    }

    /// Empty input yields no ids.
    #[test]
    fn lookup_empty_input() {
        let f = fixture();
        assert!(run_op(&f, |exec, r| r.lookup(exec, &[])).is_empty());
    }

    /// Existing entity returns its id; a missing entity is created; repeated
    /// calls deduplicate.
    #[test]
    fn lookup_or_create_existing_and_created() {
        let f = fixture();
        let apple = seed_entity(&f, "ORGANIZATION", "Apple Inc.");

        // Existing entity: same id, no new row.
        let ids = run_op(&f, |exec, r| {
            r.lookup_or_create(exec, f.doc_id, &[entity("Apple Inc.", "ORGANIZATION", "")])
        });
        assert_eq!(ids, vec![apple]);
        assert_eq!(entity_count(&f), 1);

        // Missing entity: created and linked to the document.
        let first = run_op(&f, |exec, r| {
            r.lookup_or_create(exec, f.doc_id, &[entity("NewCorp", "ORGANIZATION", "")])
        });
        assert!(first[0] > 0);
        assert_eq!(entity_count(&f), 2);
        assert_eq!(source_doc_ids(&f, first[0]), vec![f.doc_id]);

        // Repeated call: deduplicated, one row.
        let second = run_op(&f, |exec, r| {
            r.lookup_or_create(exec, f.doc_id, &[entity("NewCorp", "ORGANIZATION", "")])
        });
        assert_eq!(second, first, "repeated calls must deduplicate");
        assert_eq!(entity_count(&f), 2);
    }

    /// Empty input yields no ids; an invalid doc id is an error.
    #[test]
    fn lookup_or_create_empty_and_invalid_doc_id() {
        let f = fixture();
        assert!(run_op(&f, |exec, r| r.lookup_or_create(exec, f.doc_id, &[])).is_empty());

        for doc_id in [0, -1] {
            let err =
                f.db.with_conn(|conn| {
                    f.resolver.lookup_or_create(
                        ConnectionOrTx::Connection(conn),
                        doc_id,
                        &[entity("Corp", "ORGANIZATION", "")],
                    )
                })
                .unwrap()
                .unwrap_err();
            assert!(matches!(err, IngestionError::InvalidDocumentId(_)), "{err}");
        }
    }

    /// A mix of existing and new entities resolves correctly.
    #[test]
    fn lookup_or_create_mixed_existing_and_new() {
        let f = fixture();
        let alice = seed_entity(&f, "PERSON", "Alice");

        let (ids, created) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(
                exec,
                f.doc_id,
                &[entity("Alice", "PERSON", ""), entity("Bob", "PERSON", "")],
            )
        });
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], alice, "the existing entity keeps its id");
        assert!(ids[1] > 0 && ids[1] != alice);
        assert_eq!(created, 1, "only Bob is new");
        assert_eq!(source_doc_ids(&f, ids[1]), vec![f.doc_id]);
        assert_eq!(entity_count(&f), 2);
    }

    /// A later `add_entities` call merges into the synthetic entity.
    #[test]
    fn later_add_entities_merges_into_synthetic() {
        let f = fixture();
        let synthetic = run_op(&f, |exec, r| {
            r.lookup_or_create(exec, f.doc_id, &[entity("SynthCorp", "ORGANIZATION", "")])
        });
        let resolved = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("SynthCorp", "ORGANIZATION", "")])
        });
        assert_eq!(resolved[0].id, synthetic[0]);
        assert_eq!(entity_count(&f), 1, "no duplicate row");
    }

    /// The created count: existing → 0, new → 1, duplicate ingestion → 0,
    /// empty input.
    #[test]
    fn lookup_or_create_with_stats_counts_created() {
        let f = fixture();
        let apple = seed_entity(&f, "ORGANIZATION", "Apple Inc.");

        let (ids, created) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(
                exec,
                f.doc_id,
                &[entity("Apple Inc.", "ORGANIZATION", "")],
            )
        });
        assert_eq!(ids, vec![apple]);
        assert_eq!(created, 0, "the entity already exists");
        assert_eq!(entity_count(&f), 1);

        let (ids, created) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(exec, f.doc_id, &[entity("NewCorp", "ORGANIZATION", "")])
        });
        assert!(ids[0] > 0);
        assert_eq!(created, 1);
        assert_eq!(entity_count(&f), 2);

        // Duplicate ingestion: everything already exists.
        let batch = [
            entity("ProjectAlpha", "PROJECT", ""),
            entity("TeamBeta", "TEAM", ""),
        ];
        let (_, created1) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(exec, f.doc_id, &batch)
        });
        assert_eq!(created1, 2);
        let (_, created2) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(exec, f.doc_id, &batch)
        });
        assert_eq!(created2, 0, "duplicate ingestion must create nothing");
        assert_eq!(entity_count(&f), 4);

        let (ids, created) = run_op(&f, |exec, r| {
            r.lookup_or_create_with_stats(exec, f.doc_id, &[])
        });
        assert!(ids.is_empty());
        assert_eq!(created, 0);
    }

    /// Design D9: the candidate was deleted mid-run (GC) — the stale index
    /// entry leads to a DB miss, which triggers a full rehydrate + ONE
    /// retry; the entity is then created fresh.
    #[test]
    fn gc_deleted_candidate_triggers_rehydrate_and_retry() {
        let f = fixture();
        let first = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple Inc.", "ORGANIZATION", "")])
        });
        let old_id = first[0].id;

        // Simulate GC: delete the row out-of-band; the resolver's index
        // still holds the stale id.
        f.db.with_conn(|conn| EntityDao::new(ConnectionOrTx::Connection(conn)).delete(old_id))
            .unwrap()
            .unwrap();

        // "Apple" (JW 0.9) resolves through the stale index to the deleted
        // id → DB miss → rehydrate + retry → fresh creation.
        let second = run_op(&f, |exec, r| {
            r.add_entities(exec, f.doc_id, &[entity("Apple", "ORGANIZATION", "")])
        });
        assert_eq!(second.len(), 1);
        assert_ne!(
            second[0].id, old_id,
            "the recreated entity must get a new id"
        );
        assert_eq!(entity_count(&f), 1, "exactly one row after rehydration");

        // "Apple Inc." (JW 0.9) now resolves to the RECREATED entity, not
        // the stale deleted id: the rehydration dropped the stale entry.
        let ids = run_op(&f, |exec, r| {
            r.lookup(exec, &[entity("Apple Inc.", "ORGANIZATION", "")])
        });
        assert_eq!(
            ids,
            vec![Some(second[0].id)],
            "the stale index entry must be gone; the recreated entity resolves"
        );
    }

    /// `from_config` wires the threshold through: "Andrew" vs "Anners"
    /// scores Jaro-Winkler ≈ 0.756 (shared "an" bigram) — below the 0.8
    /// default, above 0.5.
    #[test]
    fn from_config_threshold_controls_merging() {
        let f = fixture();
        seed_entity(&f, "PERSON", "Andrew");

        let strict = Resolver::new(0.8);
        let strict_ids =
            f.db.with_conn(|conn| {
                strict.lookup(
                    ConnectionOrTx::Connection(conn),
                    &[entity("Anners", "PERSON", "")],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(strict_ids, vec![None], "0.756 < 0.8: no merge");

        let lenient = Resolver::from_config(&ResolverConfig {
            similarity_threshold: 0.5,
        });
        let lenient_ids =
            f.db.with_conn(|conn| {
                lenient.lookup(
                    ConnectionOrTx::Connection(conn),
                    &[entity("Anners", "PERSON", "")],
                )
            })
            .unwrap()
            .unwrap();
        assert!(lenient_ids[0].is_some(), "0.756 >= 0.5: merge");
    }
}
