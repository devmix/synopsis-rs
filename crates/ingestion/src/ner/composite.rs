//! Composite NER stage: ordered providers + auto-publish threshold filter
//! (ingestion-ner design D7, task 2.6).
//!
//! [`CompositeNer`] implements [`NerProvider`] (name `"composite"`) so the
//! pipeline treats the whole stage as one provider. Extraction runs the
//! providers sequentially in declared order, short-circuits on the first
//! provider error (no partial results — design D10), enriches every
//! entity/fact metadata bag with the chunk's source metadata plus a
//! `"provider"` name tag (provider-set `domain` fields preserved), then
//! applies the per-domain `auto_publish_threshold` filter: entities below
//! their domain's threshold are dropped, and a fact whose subject OR object
//! is not among the surviving entities is dropped (cascade). Entities from
//! domains absent from the threshold map pass through unfiltered.
//!
//! # Design: pre-built providers + a stage factory
//!
//! [`CompositeNer::new`] takes **pre-built** `Vec<Box<dyn NerProvider>>`; the
//! stage-to-provider wiring lives in the factory
//! [`CompositeNer::build_from_stages`]. Each provider has different
//! construction needs — [`RegexNer`] only the domain configs, [`LlmNer`] the
//! LLM config + prompts + cache pool — so pushing all of that into the
//! composite constructor would couple orchestration to every provider's
//! dependencies. The factory keeps one match arm per stage; the core stays
//! provider-agnostic and testable with stubs.
//!
//! # Design decisions
//!
//! - **No "unknown stage" arm.** The config crate's strict [`NerMethod`]
//!   enum already rejects unknown words at parse time with the same
//!   "want one of" message (config design D7), so the factory matches the
//!   three variants exhaustively and the unknown-stage error has no Rust
//!   analogue.
//! - **`prose` is a construction error.** Prose NER (a statistical provider
//!   with no Rust equivalent) is deferred by human decision 2026-08-23 — a
//!   second ONNX stack was rejected. The config parser still accepts the
//!   `"prose"` word (the strict enum keeps the word); the failure surfaces at
//!   provider construction as [`IngestionError::ProseNerDeferred`].
//! - **Empty merge is `Ok(None)`.** The trait contract (design D2) says
//!   "nothing found" is `Ok(None)` — an empty merge after filtering becomes
//!   `Ok(None)`, like the individual providers (task 2.5 pattern).
//! - **Threshold map keyed by the normalized domain name, built once at
//!   construction.** Both providers tag entities with the *normalized* name,
//!   so the map is keyed by the normalized name (a config name with uppercase
//!   or extra whitespace would otherwise silently disable filtering), and is
//!   built once here from the same domain configs the providers use rather
//!   than rebuilt on every extraction call.
//! - **Fact endpoint keys are `(name, type)` tuples**, not `name|type`
//!   strings (avoids a `|`-collision, the same fix as the `regex.rs` dedup).
//! - **Duplicate stages are allowed:** a repeated stage runs twice and its
//!   entities are duplicated — the composite does not dedup across providers.

use std::collections::{HashMap, HashSet};
use std::fmt;

use config::DomainConfig;
use config::ontology::NerMethod;
use config::preset::LlmConfig;
use db::Db;
use serde_json::{Map, Value};

use super::regex::normalize;
use super::{LlmNer, NerPrompts, NerProvider, NerResult, RegexNer};
use crate::error::IngestionError;

/// Metadata key carrying the name of the provider that produced an
/// entity/fact.
const PROVIDER_KEY: &str = "provider";

/// Composite NER stage: sequential providers + per-domain auto-publish
/// threshold filter (design D7).
///
/// `Send + Sync` — shareable behind a trait object (design D2).
pub struct CompositeNer {
    /// Providers in declared stage order.
    providers: Vec<Box<dyn NerProvider>>,
    /// `auto_publish_threshold` per normalized domain name; entities from
    /// domains absent here pass through unfiltered.
    thresholds: HashMap<String, f64>,
}

impl fmt::Debug for CompositeNer {
    /// Prints the stage order and the threshold map (the providers are
    /// opaque trait objects, so only their names are echoed).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeNer")
            .field(
                "providers",
                &self.providers.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .field("thresholds", &self.thresholds)
            .finish()
    }
}

impl CompositeNer {
    /// Builds the composite from pre-built providers and the domain configs
    /// (no logger — logging is a pipeline concern, not part of the
    /// extraction contract).
    ///
    /// The per-domain `auto_publish_threshold` map is built once here from
    /// the same domain configs the providers were built from
    /// ([`DomainConfig::effective_confidence`]), keyed by the normalized
    /// domain name (see the module docs).
    pub fn new(providers: Vec<Box<dyn NerProvider>>, domain_configs: &[DomainConfig]) -> Self {
        let thresholds = domain_configs
            .iter()
            .map(|config| {
                (
                    normalize(&config.name),
                    config.effective_confidence().auto_publish,
                )
            })
            .collect();
        Self {
            providers,
            thresholds,
        }
    }

    /// Builds the composite from the configured NER stages.
    ///
    /// `methods` is `GlobalNerConfig.methods` in declared order: `regex`
    /// builds a [`RegexNer`] over all domain configs, `llm` builds an
    /// [`LlmNer`] from the LLM config + prompts + cache pool (the `llm`
    /// parameters are only consumed when an `llm` stage is present).
    /// [`NerMethod::Prose`] fails with [`IngestionError::ProseNerDeferred`]
    /// (prose NER is deferred — see the module docs). Unknown stage words
    /// never reach this function: the config crate's strict enum rejects
    /// them at parse time (config design D7).
    ///
    /// # Errors
    ///
    /// - [`IngestionError::ProseNerDeferred`] when a stage is `prose`;
    /// - [`IngestionError::LlmNerNoDomains`] or [`IngestionError::Llm`] when
    ///   an `llm` stage's client configuration is invalid.
    pub fn build_from_stages(
        methods: &[NerMethod],
        domain_configs: &[DomainConfig],
        llm_config: &LlmConfig,
        prompts: NerPrompts,
        cache: Option<Db>,
    ) -> Result<Self, IngestionError> {
        let mut providers: Vec<Box<dyn NerProvider>> = Vec::with_capacity(methods.len());
        for method in methods {
            match method {
                NerMethod::Regex => providers.push(Box::new(RegexNer::new(domain_configs))),
                NerMethod::Llm => {
                    let ner =
                        LlmNer::new(llm_config, domain_configs, prompts.clone(), cache.clone())?;
                    providers.push(Box::new(ner));
                }
                NerMethod::Prose => return Err(IngestionError::ProseNerDeferred),
            }
        }
        Ok(Self::new(providers, domain_configs))
    }

    /// The provider names in stage order (test seam for wiring assertions).
    #[cfg(test)]
    fn provider_names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.name())
            .collect()
    }

    /// Per-domain `auto_publish_threshold` filter + fact cascade. Entities
    /// below their domain's threshold are dropped; entities from domains
    /// absent from the map pass through unfiltered. A fact survives only when
    /// BOTH its subject and its object are among the surviving entities (a
    /// fact referencing a never-extracted entity is dangling and dropped too).
    fn filter_by_auto_publish(&self, result: NerResult) -> NerResult {
        if self.thresholds.is_empty() {
            return result;
        }

        let entities = result
            .entities
            .into_iter()
            .filter(|entity| {
                self.thresholds
                    .get(&entity.domain)
                    .is_none_or(|threshold| entity.confidence >= *threshold)
            })
            .collect::<Vec<_>>();

        let kept = entities
            .iter()
            .map(|entity| (entity.name.as_str(), entity.entity_type.as_str()))
            .collect::<HashSet<(&str, &str)>>();

        let facts = result
            .facts
            .into_iter()
            .filter(|fact| {
                kept.contains(&(fact.subject_name.as_str(), fact.subject_type.as_str()))
                    && kept.contains(&(fact.object_name.as_str(), fact.object_type.as_str()))
            })
            .collect();

        NerResult { entities, facts }
    }
}

impl NerProvider for CompositeNer {
    fn name(&self) -> &'static str {
        "composite"
    }

    fn extract_entities(
        &self,
        content: &str,
        metadata: &Map<String, Value>,
    ) -> Result<Option<NerResult>, IngestionError> {
        // An empty stage list finds nothing.
        if self.providers.is_empty() {
            return Ok(None);
        }

        let mut merged = NerResult::default();
        for provider in &self.providers {
            // Design D10: a provider error aborts the whole call — no
            // partial results (design D2).
            let Some(mut result) = provider.extract_entities(content, metadata)? else {
                continue;
            };
            enrich_metadata(provider.name(), &mut result, metadata);
            merged.entities.extend(result.entities);
            merged.facts.extend(result.facts);
        }

        let filtered = self.filter_by_auto_publish(merged);
        if filtered.entities.is_empty() && filtered.facts.is_empty() {
            // Design D2: "nothing found" is Ok(None), never an empty Some.
            Ok(None)
        } else {
            Ok(Some(filtered))
        }
    }
}

/// Copies the chunk's source metadata into every entity/fact of `result`,
/// then stamps the producing provider's name under `"provider"` when absent.
/// Source metadata wins on key collision (the source bag is merged in
/// first); the provider-set `domain` field is never touched.
fn enrich_metadata(provider_name: &str, result: &mut NerResult, source: &Map<String, Value>) {
    for entity in &mut result.entities {
        entity.metadata.extend(source.clone());
        entity
            .metadata
            .entry(PROVIDER_KEY.to_owned())
            .or_insert_with(|| Value::String(provider_name.to_owned()));
    }
    for fact in &mut result.facts {
        fact.metadata.extend(source.clone());
        fact.metadata
            .entry(PROVIDER_KEY.to_owned())
            .or_insert_with(|| Value::String(provider_name.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use config::load_domain_config;
    use config::preset::{LlmConfig, ResponseFormat};

    use super::*;
    use crate::ner::{NerEntity, NerFact, load_ner_prompts};

    /// A stub provider with canned output and a call counter. `Ok(None)`
    /// models "nothing found" (design D2).
    struct MockProvider {
        name: &'static str,
        output: Option<NerResult>,
        fail: bool,
        calls: Arc<AtomicU32>,
    }

    impl MockProvider {
        /// A successful stub; the returned counter observes the call count.
        fn new(name: &'static str, output: Option<NerResult>) -> (Self, Arc<AtomicU32>) {
            let calls = Arc::new(AtomicU32::new(0));
            (
                Self {
                    name,
                    output,
                    fail: false,
                    calls: calls.clone(),
                },
                calls,
            )
        }

        /// A stub that always fails (any `IngestionError` works — the
        /// composite propagates it unchanged).
        fn failing(name: &'static str) -> Self {
            Self {
                name,
                output: None,
                fail: true,
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl NerProvider for MockProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn extract_entities(
            &self,
            _content: &str,
            _metadata: &Map<String, Value>,
        ) -> Result<Option<NerResult>, IngestionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(IngestionError::LlmNerNoDomains);
            }
            Ok(self.output.clone())
        }
    }

    fn entity(name: &str, entity_type: &str, domain: &str, confidence: f64) -> NerEntity {
        NerEntity {
            name: name.to_owned(),
            entity_type: entity_type.to_owned(),
            description: String::new(),
            confidence,
            domain: domain.to_owned(),
            metadata: Map::new(),
        }
    }

    fn fact(subject: (&str, &str), predicate: &str, object: (&str, &str), domain: &str) -> NerFact {
        NerFact {
            subject_type: subject.1.to_owned(),
            subject_name: subject.0.to_owned(),
            predicate: predicate.to_owned(),
            object_type: object.1.to_owned(),
            object_name: object.0.to_owned(),
            domain: domain.to_owned(),
            metadata: Map::new(),
        }
    }

    /// A string-valued metadata bag.
    fn metadata(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut map = Map::new();
        for &(key, value) in pairs {
            map.insert(key.to_owned(), Value::String(value.to_owned()));
        }
        map
    }

    /// Unique directory sequence (parallel tests must not share a dir).
    static DOMAIN_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// One-domain config with an explicit auto-publish threshold and
    /// optional regex rules `(id, entity, pattern, confidence)` — loaded
    /// through the real config loader (the only public way to get compiled
    /// patterns, config design D5).
    fn domain_config(
        name: &str,
        auto_publish: f64,
        rules: &[(&str, &str, &str, f64)],
    ) -> DomainConfig {
        let seq = DOMAIN_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "synopsis-ner-composite-{}-{seq}-{}",
            std::process::id(),
            name.trim().to_lowercase().replace(' ', "_")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rules_xml = rules
            .iter()
            .map(|(id, entity, pattern, confidence)| {
                format!(
                    r#"<regex id="{id}" entity="{entity}" pattern="{pattern}" confidence="{confidence}"/>"#
                )
            })
            .collect::<String>();
        let xml = format!(
            r#"<domain name="{name}" version="1.0"><extraction><regex-rules>{rules_xml}</regex-rules></extraction><confidence auto_publish_threshold="{auto_publish}"/></domain>"#
        );
        let path = dir.join("domain.xml");
        std::fs::write(&path, xml).unwrap();
        let config = load_domain_config(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        config
    }

    /// A valid LLM config (no network at construction; the llm crate
    /// validates fail-fast, llm crate D2).
    fn llm_config() -> LlmConfig {
        LlmConfig {
            api_base_url: "http://127.0.0.1:1".to_owned(),
            api_key: String::new(),
            model_name: "test-model".to_owned(),
            temperature: 0.0,
            max_tokens: 1024,
            seed: 0,
            response_format: ResponseFormat::JsonObject,
            timeout_ms: 5000,
            max_retries: 0,
            reasoning_effort: String::new(),
        }
    }

    /// The embedded-default prompts (a missing path is the normal case).
    fn prompts() -> NerPrompts {
        load_ner_prompts("/nonexistent-ner-prompts").unwrap()
    }

    /// The provider-set domain field survives enrichment untouched (single
    /// and multi-provider).
    #[test]
    fn provider_set_domain_is_preserved() {
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![entity("CEO", "role", "hr", 0.95)],
                facts: vec![],
            }),
        );
        let (p2, _) = MockProvider::new(
            "p2",
            Some(NerResult {
                entities: vec![entity("§10", "section", "legal", 0.95)],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1), Box::new(p2)], &[]);

        let result = composite
            .extract_entities("The CEO referenced §10.", &metadata(&[("doc_id", "1")]))
            .unwrap()
            .expect("entities found");

        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.entities[0].domain, "hr");
        assert_eq!(result.entities[1].domain, "legal");
    }

    /// Task 2.6: stages run in declared order; each entity is tagged with
    /// the provider that produced it.
    #[test]
    fn stages_run_in_declared_order_and_are_tagged() {
        let (a, _) = MockProvider::new(
            "first",
            Some(NerResult {
                entities: vec![entity("A", "role", "hr", 0.9)],
                facts: vec![],
            }),
        );
        let (b, _) = MockProvider::new(
            "second",
            Some(NerResult {
                entities: vec![entity("B", "role", "hr", 0.9)],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(a), Box::new(b)], &[]);

        let result = composite
            .extract_entities("text", &Map::new())
            .unwrap()
            .expect("found");
        assert_eq!(
            result
                .entities
                .iter()
                .map(|e| (e.name.as_str(), e.metadata.get("provider")))
                .collect::<Vec<_>>(),
            [
                ("A", Some(&Value::String("first".to_owned()))),
                ("B", Some(&Value::String("second".to_owned()))),
            ]
        );
    }

    /// Task 2.6: source metadata is extended into every entity and fact
    /// (source wins on key collision; provider metadata survives
    /// non-colliding keys); the `"provider"` tag is set when absent and
    /// preserved when already present.
    #[test]
    fn source_metadata_is_extended_and_provider_tag_preserved() {
        let mut e = entity("Alice", "person", "hr", 0.9);
        e.metadata
            .insert("rule_id".to_owned(), Value::String("r1".to_owned()));
        e.metadata.insert(
            "doc_id".to_owned(),
            Value::String("provider-value".to_owned()),
        );
        let mut custom = entity("Carol", "person", "hr", 0.9);
        custom
            .metadata
            .insert("provider".to_owned(), Value::String("custom".to_owned()));
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![e, entity("Bob", "person", "hr", 0.9), custom],
                facts: vec![fact(("Alice", "person"), "works_at", ("Acme", "org"), "hr")],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1)], &[]);

        let result = composite
            .extract_entities("text", &metadata(&[("doc_id", "chunk-1")]))
            .unwrap()
            .expect("found");

        let alice = &result.entities[0];
        assert_eq!(
            alice.metadata.get("rule_id"),
            Some(&Value::String("r1".to_owned()))
        );
        assert_eq!(
            alice.metadata.get("doc_id"),
            Some(&Value::String("chunk-1".to_owned()))
        );
        assert_eq!(
            alice.metadata.get("provider"),
            Some(&Value::String("p1".to_owned()))
        );
        // No pre-existing "provider" key → tagged with the provider name.
        assert_eq!(
            result.entities[1].metadata.get("provider"),
            Some(&Value::String("p1".to_owned()))
        );
        // A pre-existing "provider" key is preserved (set only if absent).
        assert_eq!(
            result.entities[2].metadata.get("provider"),
            Some(&Value::String("custom".to_owned()))
        );
        let fact = &result.facts[0];
        assert_eq!(
            fact.metadata.get("doc_id"),
            Some(&Value::String("chunk-1".to_owned()))
        );
        assert_eq!(
            fact.metadata.get("provider"),
            Some(&Value::String("p1".to_owned()))
        );
    }

    /// Task 2.6: entities below their domain's threshold are dropped;
    /// exactly at the threshold is kept (`>=`).
    #[test]
    fn entities_below_domain_threshold_are_dropped() {
        let hr = domain_config("hr", 0.5, &[]);
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![
                    entity("Low", "person", "hr", 0.4),
                    entity("Edge", "person", "hr", 0.5),
                    entity("High", "person", "hr", 0.9),
                ],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1)], &[hr]);

        let result = composite
            .extract_entities("text", &Map::new())
            .unwrap()
            .expect("found");
        assert_eq!(
            result
                .entities
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Edge", "High"]
        );
    }

    /// Task 2.6: fact cascade — a fact whose subject OR object was filtered
    /// out is dropped, as is a fact with a never-extracted (dangling)
    /// endpoint; a fact with both endpoints surviving is kept.
    #[test]
    fn fact_cascade_drops_when_an_endpoint_is_missing() {
        let hr = domain_config("hr", 0.5, &[]);
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![entity("Kept", "person", "hr", 0.9)],
                facts: vec![
                    fact(("Kept", "person"), "works_at", ("Kept2", "org"), "hr"),
                    fact(("Dropped", "person"), "knows", ("Kept", "person"), "hr"),
                    fact(("Kept", "person"), "manages", ("Dropped", "person"), "hr"),
                    fact(("Kept", "person"), "owns", ("Ghost", "org"), "hr"),
                ],
            }),
        );
        // "Kept2" (org) is extracted by a second provider above the
        // threshold; "Dropped" is below it.
        let (p2, _) = MockProvider::new(
            "p2",
            Some(NerResult {
                entities: vec![
                    entity("Kept2", "org", "hr", 0.9),
                    entity("Dropped", "person", "hr", 0.1),
                ],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1), Box::new(p2)], &[hr]);

        let result = composite
            .extract_entities("text", &Map::new())
            .unwrap()
            .expect("found");
        assert_eq!(result.facts.len(), 1);
        assert_eq!(result.facts[0].predicate, "works_at");
    }

    /// Task 2.6: entities from domains absent from the threshold map pass
    /// through unfiltered (even at confidence 0.0).
    #[test]
    fn unknown_domains_pass_through_unfiltered() {
        let hr = domain_config("hr", 0.9, &[]);
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![
                    entity("Known", "person", "hr", 0.5),
                    entity("Unknown", "person", "unseen", 0.0),
                ],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1)], &[hr]);

        let result = composite
            .extract_entities("text", &Map::new())
            .unwrap()
            .expect("found");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "Unknown");
    }

    /// No domain configs → no filtering at all (dangling facts included).
    #[test]
    fn no_domain_configs_disables_filtering() {
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![entity("Low", "person", "hr", 0.1)],
                facts: vec![fact(("Low", "person"), "owns", ("Ghost", "org"), "hr")],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1)], &[]);

        let result = composite
            .extract_entities("text", &Map::new())
            .unwrap()
            .expect("found");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.facts.len(), 1);
    }

    /// Design D2: "nothing found" is `Ok(None)` (empty stage list or every
    /// provider empty); the composite itself is a named, object-safe
    /// provider.
    #[test]
    fn nothing_found_yields_none_and_the_composite_is_named() {
        let empty = CompositeNer::new(Vec::new(), &[]);
        assert_eq!(empty.extract_entities("text", &Map::new()).unwrap(), None);
        assert_eq!(NerProvider::name(&empty), "composite");
        let boxed: Box<dyn NerProvider> = Box::new(empty);
        assert_eq!(boxed.name(), "composite");

        let (p1, _) = MockProvider::new("p1", None);
        let (p2, _) = MockProvider::new("p2", None);
        let all_empty = CompositeNer::new(vec![Box::new(p1), Box::new(p2)], &[]);
        assert_eq!(
            all_empty.extract_entities("text", &Map::new()).unwrap(),
            None
        );
    }

    /// Task 2.6: a provider error short-circuits — the error propagates and
    /// later stages are not run (design D10).
    #[test]
    fn provider_error_short_circuits_later_stages() {
        let (p2, p2_calls) = MockProvider::new(
            "p2",
            Some(NerResult {
                entities: vec![entity("B", "role", "hr", 0.9)],
                facts: vec![],
            }),
        );
        let p1 = MockProvider::failing("p1");
        let composite = CompositeNer::new(vec![Box::new(p1), Box::new(p2)], &[]);

        let err = composite.extract_entities("text", &Map::new()).unwrap_err();
        assert!(matches!(err, IngestionError::LlmNerNoDomains));
        assert_eq!(p2_calls.load(Ordering::Relaxed), 0);
    }

    /// The threshold map is keyed by the normalized domain name (the
    /// providers tag normalized names), so a config name with
    /// uppercase/whitespace still filters its entities.
    #[test]
    fn threshold_map_is_keyed_by_normalized_domain_name() {
        let hr = domain_config("  HR ", 0.5, &[]);
        let (p1, _) = MockProvider::new(
            "p1",
            Some(NerResult {
                entities: vec![entity("Low", "person", "hr", 0.4)],
                facts: vec![],
            }),
        );
        let composite = CompositeNer::new(vec![Box::new(p1)], &[hr]);

        // 0.4 < 0.5 and the domain is known (normalized) → dropped → empty.
        assert_eq!(
            composite.extract_entities("text", &Map::new()).unwrap(),
            None
        );
    }

    /// Task 2.6: the `prose` stage is rejected with the deferral error
    /// (human decision 2026-08-23).
    #[test]
    fn build_from_stages_rejects_prose() {
        let hr = domain_config("hr", 0.85, &[]);
        let err = CompositeNer::build_from_stages(
            &[NerMethod::Prose],
            &[hr],
            &llm_config(),
            prompts(),
            None,
        )
        .unwrap_err();

        assert!(matches!(err, IngestionError::ProseNerDeferred));
        let message = err.to_string();
        assert!(message.contains("prose"), "{message}");
        assert!(message.contains("2026-08-23"), "{message}");
    }

    /// Task 2.6: stages map to providers in declared order (construction).
    #[test]
    fn build_from_stages_builds_providers_in_declared_order() {
        let hr = domain_config("hr", 0.85, &[]);
        let composite = CompositeNer::build_from_stages(
            &[NerMethod::Llm, NerMethod::Regex],
            &[hr],
            &llm_config(),
            prompts(),
            None,
        )
        .unwrap();

        assert_eq!(composite.provider_names(), ["llm", "regex"]);
    }

    /// Task 2.6: a regex-only stage extracts through the composite — the
    /// built provider's entities are enriched and threshold-filtered.
    #[test]
    fn build_from_stages_regex_stage_extracts() {
        let hr = domain_config("hr", 0.5, &[("r1", "role", "CEO", 0.9)]);
        let composite = CompositeNer::build_from_stages(
            &[NerMethod::Regex],
            &[hr],
            &llm_config(),
            prompts(),
            None,
        )
        .unwrap();

        let result = composite
            .extract_entities("The CEO announced changes.", &metadata(&[("doc_id", "9")]))
            .unwrap()
            .expect("found");

        assert_eq!(result.entities.len(), 1);
        let entity = &result.entities[0];
        assert_eq!(entity.name, "CEO");
        assert_eq!(entity.domain, "hr");
        assert_eq!(
            entity.metadata.get("rule_id"),
            Some(&Value::String("r1".to_owned()))
        );
        assert_eq!(
            entity.metadata.get("provider"),
            Some(&Value::String("regex".to_owned()))
        );
        assert_eq!(
            entity.metadata.get("doc_id"),
            Some(&Value::String("9".to_owned()))
        );
    }
}
