//! NER (named entity recognition) layer (change `ingestion-ner`, design D1).
//!
//! Oracle reference: `../synopsis/internal/ingestion/ner/` — `ner.go` (result
//! types + provider trait) and `regex_ner.go` (the rule-based provider, ported
//! in [`regex`](Self::regex)). Extraction results attach to chunks through the
//! pipeline's own structure — the chunk itself stays a pure chunking artifact
//! (ingestion-sources design D2).
//!
//! Core contracts (design D2):
//!
//! - [`NerProvider`] is object-safe (`Send + Sync`) so the composite stage
//!   (task 2.6) and the pipeline runner can store providers behind a trait
//!   object.
//! - `extract_entities` returns `Ok(None)` for "nothing found" (empty
//!   content, no rules, no matches) — the oracle's `nil, nil`; extraction
//!   failures are the `Err` arm, never `Ok(None)` (design D10).
//! - Entity/fact metadata bags are [`serde_json::Map`] — the same BTreeMap-
//!   backed type chunk metadata uses, so enrichment is a plain extend.
//!
//! The LLM provider's building blocks live alongside the trait: [`NerPrompts`]
//! renders the system/user prompts (task 2.2), [`generate_json_schema`]
//! builds the structured-output schema and [`parse_llm_response`] applies the
//! design D5 parse/validate rules (task 2.3); [`LlmNerCache`] +
//! [`build_cache_key`] persist LLM responses in the lazily-created
//! `llm_ner_cache` table (task 2.4, design D6); the provider itself
//! (task 2.5) composes them. [`CompositeNer`] (task 2.6, design D7) is the
//! stage orchestrator: sequential providers in declared order + the
//! per-domain auto-publish threshold filter.

use serde_json::{Map, Value};

use crate::error::IngestionError;

mod composite;
mod llm;
mod llm_cache;
mod llm_schema;
mod parse;
mod prompts;
mod regex;

pub use composite::CompositeNer;
pub use llm::LlmNer;
pub use llm_cache::{LlmNerCache, build_cache_key};
pub use llm_schema::generate_json_schema;
pub use parse::parse_llm_response;
pub use prompts::{NerPrompts, TemplateHashes, load_ner_prompts};
// Crate-private re-export: the entity-resolution primitives (task 2.7)
// normalize domain keys with the same rule the providers tag with (DRY).
pub use regex::RegexNer;
pub(crate) use regex::normalize;

/// An entity extracted from chunk content by a [`NerProvider`].
///
/// Oracle `ner.Entity`. The oracle's `Type` field is `entity_type` here —
/// `type` is a Rust keyword, so the field carries an explicit prefix
/// (deliberate deviation, recorded in the task 2.1 report).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NerEntity {
    /// Normalized entity name (trimmed).
    pub name: String,
    /// Entity type, lowercase (e.g. `"employee"`, `"product"`).
    pub entity_type: String,
    /// Description derived from surrounding context (regex rules produce an
    /// empty one; the LLM provider fills it).
    pub description: String,
    /// Extraction confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Domain this entity belongs to (normalized domain config name).
    pub domain: String,
    /// Additional provenance data (e.g. the matching rule's id).
    pub metadata: Map<String, Value>,
}

/// A fact (subject–predicate–object triple) extracted by a [`NerProvider`].
///
/// Oracle `ner.Fact`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NerFact {
    /// Subject entity type.
    pub subject_type: String,
    /// Subject entity name.
    pub subject_name: String,
    /// Relation predicate.
    pub predicate: String,
    /// Object entity type.
    pub object_type: String,
    /// Object entity name.
    pub object_name: String,
    /// Domain this fact belongs to (normalized domain config name).
    pub domain: String,
    /// Additional provenance data.
    pub metadata: Map<String, Value>,
}

/// The extraction result of one [`NerProvider::extract_entities`] call.
///
/// Oracle `ner.Result`. Serialized as the LLM cache payload (design D6,
/// task 2.4), so the serde derives are part of the contract here.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NerResult {
    /// Entities extracted, in provider order (deduplicated by the provider).
    pub entities: Vec<NerEntity>,
    /// Facts extracted, in provider order.
    pub facts: Vec<NerFact>,
}

/// A named entity recognition provider.
///
/// Object-safe (`Send + Sync`, design D2): the composite stage (task 2.6)
/// and the pipeline runner store providers as `Box<dyn NerProvider>`.
///
/// Oracle `ner.Provider`. The oracle's `context.Context` argument is dropped:
/// Rust cancellation is a runtime concern, not part of the data-flow contract.
pub trait NerProvider: Send + Sync {
    /// Stable provider name (`"regex"`, `"llm"`, …) used for metadata tagging.
    fn name(&self) -> &'static str;

    /// Extracts entities and facts from `content`.
    ///
    /// `metadata` is the chunk's metadata bag, a read-only input — enrichment
    /// of the results happens in the composite stage (design D7).
    ///
    /// `Ok(None)` means "nothing found" (empty content, no rules, no
    /// matches) — the oracle's `nil, nil`; it is never an error (design D10).
    fn extract_entities(
        &self,
        content: &str,
        metadata: &Map<String, Value>,
    ) -> Result<Option<NerResult>, IngestionError>;
}
