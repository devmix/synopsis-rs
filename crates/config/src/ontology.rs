//! Global ontology (`global.xml`) loading (tasks 3.1 + 3.1b).
//!
//! One parser for the whole file (design D6): in the Go oracle this document was parsed twice —
//! `config.LoadGlobalConfig` (sources / cross-domain links / NER) and `domain.LoadGlobalPool`
//! (entities / relations / extraction). Here a single [`load_global_config`] returns one
//! [`GlobalConfig`] with every block, fully normalized: after parsing it applies the oracle's
//! defaults (`apply_defaults`) and validates + compiles every regex rule in place
//! (`validate`, design D5), mirroring the oracle's combined startup flow. The public structs are
//! still deserialized directly from the file; defaults and validation run only inside the loader,
//! so a raw `quick_xml::de` parse (as in this module's unit tests) yields un-defaulted values.
//!
//! Document format (design D15, revision 4): every group of repeated elements sits inside a
//! plural wrapper element: `<entities><entity/>`, `<relations><relation/>`, `<sources><source/>`,
//! `<domains><domain/>`, `<expressions><expression/>`, `<attributes><attribute/>`,
//! `<synonyms><synonym/>`, `<methods><method/>` (inside `<cross-domain-links>` and `<ner>`) and
//! `<regex-rules><regex/>` (inside `<extraction>`). Sub-structure containers survive as in the
//! oracle: `<global>`, `<cross-domain-links>`, `<equals>`, `<ner>`, `<extraction>`. XML
//! attributes map through `#[serde(rename = "@name")]`; scalar element children are plain fields.
//!
//! The public API is **flat** — every list is a plain `Vec` field on its owning struct. The
//! wrapper elements are consumed by private [`serde::deserialize_with`] helpers, each of which
//! deserializes a small local wrapper struct (one item-element field) and unwraps it to the flat
//! list. No duplicate public parse-shape types exist (design D15).
//!
//! Value enums use standard serde only — no macros:
//! - strict ([`LinkMethod`], [`NerMethod`]): `#[derive(Deserialize)]` +
//!   `#[serde(rename_all = "lowercase")]` with no `other` variant, so an unknown word is a parse
//!   error (`ConfigError::Xml`). The oracle validates exactly these two sets in `Validate()`, so
//!   failing earlier keeps loading equally rejecting (design D15, risks). quick-xml resolves a
//!   derived C-like enum's *list items* from the element **tag name**, never its text value, so
//!   the `<methods>` helpers map raw words to the strict enums by exact match against each
//!   enum's word list (case-sensitive, like the oracle's `m == vm` loop and like derived
//!   matching of renamed identifiers); an unknown word fails with the oracle-style message.
//! - tolerant: [`AttributeType`] is a pure derive + `#[serde(other)]` unit `Unknown` (the value
//!   is never inspected beyond variant matching; an absent attribute yields
//!   [`Default::default()`] = `Unknown`). [`SourceType`] keeps a custom `Deserialize` impl that
//!   preserves the raw word in `Unknown(String)` because the loader's validation needs to tell
//!   "absent/empty" (`Unknown("")`) from a non-empty unrecognized word (the oracle checks
//!   emptiness only: `src.Type == ""`).

use std::collections::HashSet;
use std::ops::Deref;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// File name of the global ontology inside the ontology directory (oracle convention).
pub const GLOBAL_XML_FILE: &str = "global.xml";

// Oracle default values (global_config.go `Default*` constants), applied by `apply_defaults`.

/// Default minimum word count for the equals method when absent or non-positive.
const DEFAULT_EQUALS_MIN_WORDS: i32 = 2;
/// Default confidence threshold for LLM linking results when absent or non-positive.
const DEFAULT_LLM_CONFIDENCE_THRESHOLD: f64 = 0.7;
/// Default number of entity pairs per LLM call when absent or non-positive.
const DEFAULT_LLM_BATCH_SIZE: i32 = 5;
/// Default relation type for linking expressions that omit one.
const DEFAULT_RELATION_TYPE: &str = "same_entity";

/// Builds a [`ConfigError::Validation`] from a message (mirrors the preset module's helper).
fn validation(message: impl Into<String>) -> ConfigError {
    ConfigError::Validation {
        message: message.into(),
    }
}

// ── Value enums ────────────────────────────────────────────────────────────

/// Cross-domain linking method (`<method>` under `<cross-domain-links><methods>`): `"expression"`,
/// `"equals"` or `"llm"`. Strict enum (design D7/D15): no `other` variant — an unknown word is a
/// parse error, matching the oracle's `Validate()` membership check moved earlier (see module
/// docs for why the mapping lives in the `<methods>` helpers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMethod {
    /// CEL-based conditional matching.
    Expression,
    /// Automatic matching by normalized name similarity.
    Equals,
    /// LLM-based entity resolution.
    Llm,
}

impl LinkMethod {
    /// Words accepted by the oracle's `ValidMethods` list (single source for mapping + message).
    const WORDS: [&str; 3] = ["expression", "equals", "llm"];

    /// Maps a raw `<method>` word to a variant — exact, case-sensitive match like the oracle.
    fn parse_word(word: &str) -> Result<Self, String> {
        let err = || {
            format!(
                "invalid cross-domain link method \"{word}\", want one of [{}]",
                Self::WORDS.join(" ")
            )
        };
        match word {
            "expression" => Ok(Self::Expression),
            "equals" => Ok(Self::Equals),
            "llm" => Ok(Self::Llm),
            _ => Err(err()),
        }
    }
}

/// NER extraction stage (`<method>` under `<ner><methods>`): `"regex"`, `"prose"` or `"llm"`.
/// Strict enum — see [`LinkMethod`] for the shared semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NerMethod {
    /// Rule-based pattern matching against entity definitions.
    Regex,
    /// Statistical (spaCy-style) NER.
    Prose,
    /// LLM-based extraction.
    Llm,
}

impl NerMethod {
    /// Words accepted by the oracle's `ValidNERMethods` list (single source for mapping + message).
    const WORDS: [&str; 3] = ["regex", "prose", "llm"];

    /// Maps a raw `<method>` word to a variant — exact, case-sensitive match like the oracle.
    fn parse_word(word: &str) -> Result<Self, String> {
        let err = || {
            format!(
                "invalid NER method \"{word}\", want one of [{}]",
                Self::WORDS.join(" ")
            )
        };
        match word {
            "regex" => Ok(Self::Regex),
            "prose" => Ok(Self::Prose),
            "llm" => Ok(Self::Llm),
            _ => Err(err()),
        }
    }
}

/// Entity attribute value kind (`type` attribute of `<attribute>`): `"string"`, `"date"`,
/// `"number"`, `"ref"` or `"boolean"`. Tolerant enum (design D7/D15): the oracle performs no
/// word validation here, so any other word lands in `Unknown`; an absent attribute yields
/// [`Default::default()`] = `Unknown`. Standard serde only: derive + `#[serde(other)]`, no
/// macros and no custom impl — the value is never inspected beyond variant matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttributeType {
    /// Plain string value.
    String,
    /// Date value.
    Date,
    /// Numeric value.
    Number,
    /// Reference to another entity (requires a non-empty `target` — checked at load time).
    Ref,
    /// Boolean value.
    Boolean,
    /// A word the schema does not recognize (`#[serde(other)]`).
    #[default]
    #[serde(other)]
    Unknown,
}

/// Ingestion data-source format (`type` attribute of `<source>`): `"markdown"`, `"webpages"`,
/// `"mediawiki"` or `"unstructured"`. Tolerant enum with a **custom** `Deserialize` impl (design
/// D15 revision 4): the oracle never validates this word, only its emptiness (`src.Type == ""`)
/// — the loader turns an empty value into the "type is required" error. The raw word must
/// therefore survive:
/// known words map to named variants (case-insensitively, crate convention), everything else —
/// including an absent attribute via [`Default`] — stays in `Unknown(String)` verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceType {
    /// Markdown documents.
    Markdown,
    /// Crawled web pages.
    Webpages,
    /// MediaWiki space.
    Mediawiki,
    /// Unstructured dataset directory.
    Unstructured,
    /// A word the schema does not recognize (kept verbatim); `Unknown("")` = absent attribute.
    Unknown(String),
}

impl Default for SourceType {
    fn default() -> Self {
        // Absent `type` attribute: the oracle's empty-string zero value, rejected at load time.
        Self::Unknown(String::new())
    }
}

impl Serialize for SourceType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Markdown => serializer.serialize_str("markdown"),
            Self::Webpages => serializer.serialize_str("webpages"),
            Self::Mediawiki => serializer.serialize_str("mediawiki"),
            Self::Unstructured => serializer.serialize_str("unstructured"),
            Self::Unknown(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for SourceType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        // Case-insensitive match on known words (crate convention); unknown and empty values are
        // preserved verbatim for the loader's validation and diagnostics.
        Ok(match raw.to_ascii_lowercase().as_str() {
            "markdown" => Self::Markdown,
            "webpages" => Self::Webpages,
            "mediawiki" => Self::Mediawiki,
            "unstructured" => Self::Unstructured,
            other => Self::Unknown(other.to_string()),
        })
    }
}

// ── Public types (flat lists; wrappers handled by the helpers below) ───────

/// The loaded global ontology: every block of one `global.xml` file in a single pass (design D6),
/// fully normalized — after parsing the loader applies the oracle's defaults and validates +
/// compiles the regex rules before returning it. Every repeated block lives inside its plural
/// wrapper element in the D15 revision-4 format; each list field is flat (`Vec`) with the wrapper
/// consumed by a `deserialize_with` helper. Values produced by a raw deserialization (unit tests)
/// carry none of that normalization.
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    /// Ingestion data sources (`<sources><source/></sources>`); empty when absent. Every loaded
    /// source has at least one domain (`["default"]` fallback applied by the loader) and its
    /// relative `path` anchored to the ontology directory (see [`load_global_config`]).
    #[serde(default, deserialize_with = "de_sources")]
    pub sources: Vec<SourceConfig>,
    /// Cross-domain linking settings; `None` when the file has no `<cross-domain-links>`. When
    /// present in a loaded config its methods are non-empty and every zero/empty field carries an
    /// oracle default.
    #[serde(rename = "cross-domain-links")]
    pub cross_domain_links: Option<CrossDomainLinksConfig>,
    /// NER extraction methods; loaded configs always carry at least the fallback `["regex",
    /// "llm"]` (absent block and empty list both parse to an empty vec, which the loader fills).
    #[serde(default)]
    pub ner: GlobalNerConfig,
    /// Global entity pool (`<entities><entity/></entities>`); loaded configs have unique non-empty
    /// ids and validated attributes. Empty when absent.
    #[serde(default, deserialize_with = "de_entities")]
    pub entities: Vec<EntityDef>,
    /// Global relation pool (`<relations><relation/></relations>`); loaded configs have unique
    /// non-empty predicates whose source/target reference pooled entities. Empty when absent.
    #[serde(default, deserialize_with = "de_relations")]
    pub relations: Vec<RelationDef>,
    /// Extraction section; in a loaded config every rule's pattern has been validated and compiled
    /// (design D5).
    #[serde(default)]
    pub extraction: ExtractionDef,
}

/// One ingestion data source (`<source>` element): scalar data in attributes plus a
/// `<domains><domain/></domains>` wrapper of domain names.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SourceConfig {
    /// Directory to index (`path` attribute); required — enforced at load time. In a loaded
    /// config a relative value is anchored to the ontology directory (the directory holding
    /// `global.xml`), so it resolves against the file rather than the working directory.
    #[serde(default, rename = "@path")]
    pub path: String,
    /// Parser format (`type` attribute); required — see [`SourceType`] (absent → `Unknown("")`).
    #[serde(default, rename = "@type")]
    pub source_type: SourceType,
    /// Whether the source is skipped by ingestion (`disabled` attribute).
    #[serde(default, rename = "@disabled")]
    pub disabled: bool,
    /// MediaWiki space name (`space` attribute); empty for other formats.
    #[serde(default, rename = "@space")]
    pub space: String,
    /// Domain names this source belongs to; a loaded config always has at least one — sources
    /// without any `<domain>` item get `["default"]` (oracle fallback, applied by the loader).
    #[serde(default, deserialize_with = "de_domains")]
    pub domains: Vec<String>,
    /// Dataset id for unstructured sources (`dataset` attribute).
    #[serde(default, rename = "@dataset")]
    pub dataset: String,
}

/// Cross-domain linking settings (`<cross-domain-links>` element): `<methods>`, optional
/// `<equals>`, scalar thresholds and an `<expressions>` wrapper.
#[derive(Debug, Clone, Deserialize)]
pub struct CrossDomainLinksConfig {
    /// Linking methods in priority order; a loaded config always has at least one — an empty list
    /// is rejected by the loader (the oracle's `Validate`). Unknown values already fail the parse
    /// ([`LinkMethod`]).
    #[serde(default, deserialize_with = "de_link_methods")]
    pub methods: Vec<LinkMethod>,
    /// Equals-method settings; `None` when the `<equals>` element is absent. When present in a
    /// loaded config, `min_words` carries at least the oracle default (2).
    pub equals: Option<EqualsConfig>,
    /// Minimum confidence for LLM linking results; an absent element parses to `0.0`, which the
    /// loader replaces with the oracle default of 0.7.
    #[serde(default, rename = "llm-confidence-threshold")]
    pub llm_confidence_threshold: f64,
    /// Entity pairs per LLM call; an absent element parses to `0`, which the loader replaces with
    /// the oracle default of 5.
    #[serde(default, rename = "batch-size")]
    pub batch_size: i32,
    /// CEL expressions used by the `expression` method (`<expressions><expression/></expressions>`).
    #[serde(default, deserialize_with = "de_expressions")]
    pub expressions: Vec<LinkExpression>,
}

/// Equals-linking settings (`<equals>` element).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct EqualsConfig {
    /// Minimum word count of a name to be considered; an absent or ≤ 0 value is replaced by the
    /// oracle default (2) in a loaded config.
    #[serde(default, rename = "min-words")]
    pub min_words: i32,
}

/// A CEL-based linking expression (`<expression>` element inside `<expressions>`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LinkExpression {
    /// Expression identifier (`<name>` child); required in practice (no oracle validation).
    #[serde(default)]
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Evaluation priority among expressions; absent → 0 (oracle zero value).
    #[serde(default)]
    pub priority: i32,
    /// CEL condition evaluated against entity pairs `A` and `B` (from the `<where>` child).
    #[serde(default, rename = "where")]
    pub where_: String,
    /// Relation type created for matched pairs; an empty value is replaced by the oracle default
    /// (`"same_entity"`) in a loaded config.
    #[serde(default, rename = "relation-type")]
    pub relation_type: String,
}

/// NER extraction settings (`<ner>` element) with its `<methods><method/></methods>` wrapper.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct GlobalNerConfig {
    /// Pipeline stages in order; a loaded config always carries at least the oracle fallback
    /// `["regex", "llm"]` — absent block and empty list both parse to an empty vec, which the
    /// loader fills.
    #[serde(default, deserialize_with = "de_ner_methods")]
    pub methods: Vec<NerMethod>,
}

/// An entity type of the global pool (`<entity>` element inside `<entities>`): scalar data in
/// attributes plus `<attributes>` and `<synonyms>` wrappers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EntityDef {
    /// Unique entity id; required — enforced at load time.
    #[serde(default, rename = "@id")]
    pub id: String,
    /// Display name.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Description.
    #[serde(default, rename = "@description")]
    pub description: String,
    /// Attribute definitions; names must be unique per entity (checked at load time).
    #[serde(default, deserialize_with = "de_entity_attributes")]
    pub attributes: Vec<AttributeDef>,
    /// Alternative surface forms used by the linker (`<synonyms><synonym/></synonyms>`).
    #[serde(default, deserialize_with = "de_synonyms")]
    pub synonyms: Vec<String>,
}

/// An attribute of an entity (`<attribute>` element inside `<attributes>`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AttributeDef {
    /// Unique within the owning entity; required — enforced at load time.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Value kind (see [`AttributeType`]; absent `@type` → `Unknown`).
    #[serde(default, rename = "@type")]
    pub attr_type: AttributeType,
    /// Whether ingestion must find this attribute.
    #[serde(default, rename = "@required")]
    pub required: bool,
    /// Target entity id when `attr_type` is [`AttributeType::Ref`] (must be non-empty then —
    /// checked at load time).
    #[serde(default, rename = "@target")]
    pub target: String,
}

/// A relation type of the global pool (`<relation>` element inside `<relations>`): scalar data in
/// attributes plus an `<attributes>` wrapper.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RelationDef {
    /// Source entity id; must exist in the pooled entities (checked at load time).
    #[serde(default, rename = "@source")]
    pub source: String,
    /// Unique relation predicate; required — enforced at load time.
    #[serde(default, rename = "@predicate")]
    pub predicate: String,
    /// Target entity id; must exist in the pooled entities (checked at load time).
    #[serde(default, rename = "@target")]
    pub target: String,
    /// Description.
    #[serde(default, rename = "@description")]
    pub description: String,
    /// Relation attributes (`<attributes><attribute/></attributes>`).
    #[serde(default, deserialize_with = "de_relation_attributes")]
    pub attributes: Vec<RelAttrDef>,
}

/// An attribute of a relation (`<attribute>` element under `<relation><attributes>`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RelAttrDef {
    /// Attribute name.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Value kind verbatim from the XML (`"string"`, `"date"`, `"number"`, …); the oracle performs
    /// no validation here, so it stays a plain string.
    #[serde(default, rename = "@type")]
    pub attr_type: String,
}

/// Extraction section of `global.xml` (`<extraction>` element) with its
/// `<regex-rules><regex/></regex-rules>` wrapper.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExtractionDef {
    /// Regex rules; in a loaded config every pattern has been validated and compiled (design D5).
    /// An empty list means "no rules". The field is renamed because quick-xml maps it to the
    /// wrapper element `<regex-rules>` by name.
    #[serde(default, rename = "regex-rules", deserialize_with = "de_regex_rules")]
    pub regex_rules: Vec<RegexRuleDef>,
}

/// A compiled extraction pattern (design D5): wraps [`regex::Regex`] behind serde glue, because
/// the XML carries only the pattern *text* ([`RegexRuleDef::pattern`]) and `regex::Regex`
/// implements neither serde trait. Raw deserialization yields an uncompiled placeholder; the
/// loader replaces it with a real compilation for every rule before returning (see
/// [`RegexRuleDef::validate_and_compile`]), so values observed through
/// [`load_global_config`] always carry a compiled pattern. Deref to `regex::Regex` keeps call
/// sites (`is_match`, …) unchanged.
#[derive(Debug, Clone)]
pub struct CompiledPattern(Regex);

impl Default for CompiledPattern {
    /// The uncompiled placeholder (see the type docs).
    fn default() -> Self {
        Self(Self::uncompiled())
    }
}

impl Deref for CompiledPattern {
    type Target = Regex;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq for CompiledPattern {
    /// Equality on the pattern source text: the compiled form is a deterministic function of it,
    /// and `regex::Regex` itself provides no comparison traits (1.13.x).
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for CompiledPattern {}

impl Serialize for CompiledPattern {
    /// Serializes the compiled pattern's source text, so a serialized rule round-trips to an
    /// equivalent one.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for CompiledPattern {
    /// Ignores the input entirely and returns the uncompiled placeholder: no document in this
    /// schema carries a compiled form (the `#[serde(default)]` field covers absence, this impl
    /// covers the impossible presence). The empty pattern is valid by construction, making the
    /// fallback arm unreachable.
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(Self::uncompiled()))
    }
}

impl CompiledPattern {
    /// Compiles the empty pattern — always valid, used as the deserialization placeholder.
    fn uncompiled() -> Regex {
        match Regex::new("") {
            Ok(regex) => regex,
            // `""` compiles in every release of the `regex` crate; failing would be a library bug.
            Err(_) => panic!("the empty regex pattern must compile"),
        }
    }
}

/// A regex-based extraction rule (`<regex>` element inside `<extraction><regex-rules>`).
///
/// The pattern is stored as source text ([`pattern`](Self::pattern)) and compiled in place by the
/// loader into [`compiled`](Self::compiled) (design D5: patterns are validated at load time, not
/// first use — the oracle's `regexp.MustCompile` panic becomes a typed error). A raw
/// deserialization (unit tests) leaves the placeholder; only values returned by
/// [`load_global_config`] carry a compiled pattern.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegexRuleDef {
    /// Rule identifier; required — enforced at load time by
    /// [`RegexRuleDef::validate_and_compile`].
    #[serde(default, rename = "@id")]
    pub id: String,
    /// Entity id that matches are attributed to; must be non-empty (checked at load time).
    #[serde(default, rename = "@entity")]
    pub entity: String,
    /// Pattern source text exactly as written in the XML.
    #[serde(default, rename = "@pattern")]
    pub pattern: String,
    /// Confidence assigned to matches; must lie in `[0, 1]` (checked at load time).
    #[serde(default, rename = "@confidence")]
    pub confidence: f64,
    /// Compiled form of [`pattern`](Self::pattern); populated by the loader via
    /// [`RegexRuleDef::validate_and_compile`] — never observed uncompiled through
    /// [`load_global_config`].
    #[serde(default)]
    pub compiled: CompiledPattern,
}

impl RegexRuleDef {
    /// Validates this rule's fields and compiles [`pattern`](Self::pattern)` into
    /// [`compiled`](Self::compiled) — the oracle `ValidateAndCompile` (domain_config.go), with the
    /// check order preserved: id, pattern presence, confidence range, entity, then compilation.
    ///
    /// An invalid pattern is a [`ConfigError::Regex`] carrying `file` and this rule's id; the
    /// oracle panics here (`regexp.MustCompile`) — the typed error is a deliberate fix recorded in
    /// the config-module change report.
    pub(crate) fn validate_and_compile(&mut self, file: &str) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(validation("regex rule has empty ID"));
        }
        if self.pattern.is_empty() {
            return Err(validation(format!(
                "regex rule {:?} has empty pattern",
                self.id
            )));
        }
        // The oracle prints Go `%f` (6 decimal places); `{:.6}` keeps the message byte-parity.
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err(validation(format!(
                "regex rule {:?} confidence {:.6} must be in [0, 1]",
                self.id, self.confidence
            )));
        }
        if self.entity.is_empty() {
            return Err(validation(format!(
                "regex rule {:?} entity name is empty",
                self.id
            )));
        }
        let compiled = Regex::new(&self.pattern).map_err(|source| ConfigError::Regex {
            file: file.to_string(),
            rule: self.id.clone(),
            source,
        })?;
        self.compiled = CompiledPattern(compiled);
        Ok(())
    }
}

// ── Wrapper helpers ────────────────────────────────────────────────────────
//
// One private local struct + one `deserialize_with` function per wrapped list (design D15): the
// public API stays flat while quick-xml consumes each plural wrapper element through its own
// single-field struct. All are crate-private except [`de_entities`] and [`de_relations`], which
// the domain loader reuses for its own wrapper elements (task 3.2); in every case they are serde
// glue, not a second model of the file.

/// `<sources>` wrapper: one `<source>` item field.
#[derive(Deserialize)]
struct SourceList {
    /// Items inside `<sources>`.
    #[serde(default, rename = "source")]
    items: Vec<SourceConfig>,
}

/// Unwraps `<sources><source/></sources>` to the flat [`GlobalConfig::sources`] field.
fn de_sources<'de, D>(deserializer: D) -> Result<Vec<SourceConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(SourceList::deserialize(deserializer)?.items)
}

/// `<entities>` wrapper: one `<entity>` item field.
#[derive(Deserialize)]
struct EntityList {
    /// Items inside `<entities>`.
    #[serde(default, rename = "entity")]
    items: Vec<EntityDef>,
}

/// Unwraps `<entities><entity/></entities>` to the flat [`GlobalConfig::entities`] field.
/// `pub(crate)` because the domain loader (task 3.2) reuses it for its own wrapper element.
pub(crate) fn de_entities<'de, D>(deserializer: D) -> Result<Vec<EntityDef>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(EntityList::deserialize(deserializer)?.items)
}

/// `<relations>` wrapper: one `<relation>` item field.
#[derive(Deserialize)]
struct RelationList {
    /// Items inside `<relations>`.
    #[serde(default, rename = "relation")]
    items: Vec<RelationDef>,
}

/// Unwraps `<relations><relation/></relations>` to the flat [`GlobalConfig::relations`] field.
/// `pub(crate)` because the domain loader (task 3.2) reuses it for its own wrapper element.
pub(crate) fn de_relations<'de, D>(deserializer: D) -> Result<Vec<RelationDef>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(RelationList::deserialize(deserializer)?.items)
}

/// `<domains>` wrapper (inside a `<source>`): one `<domain>` item field.
#[derive(Deserialize)]
struct DomainList {
    /// Items inside `<domains>`.
    #[serde(default, rename = "domain")]
    items: Vec<String>,
}

/// Unwraps `<domains><domain/></domains>` to the flat [`SourceConfig::domains`] field.
///
/// Empty items are dropped (parity action, task 3.1b): Go's `encoding/xml` contributes nothing to
/// a `[]string` for an element without text, while quick-xml yields `""`. Without this filter an
/// empty `<domain/>` would survive deserialization and block the oracle's "no domains → default"
/// fallback from firing.
fn de_domains<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = DomainList::deserialize(deserializer)?.items;
    Ok(items.into_iter().filter(|word| !word.is_empty()).collect())
}

/// `<methods>` wrapper (inside `<cross-domain-links>`): raw words before strict mapping.
#[derive(Deserialize)]
struct LinkMethodWords {
    /// Raw `<method>` text values inside `<cross-domain-links><methods>`.
    #[serde(default, rename = "method")]
    items: Vec<String>,
}

/// Unwraps `<cross-domain-links><methods>` and maps each word to the strict [`LinkMethod`] enum.
///
/// quick-xml resolves derived C-like enums in list position from the element *tag name*, not its
/// text, so the words come back as `String` here and are matched exactly against
/// [`LinkMethod::WORDS`] — unknown words are a parse error (oracle-style message). Empty items are
/// skipped: Go's `encoding/xml` contributes nothing to a `[]string` for an element without text.
fn de_link_methods<'de, D>(deserializer: D) -> Result<Vec<LinkMethod>, D::Error>
where
    D: Deserializer<'de>,
{
    let words = LinkMethodWords::deserialize(deserializer)?.items;
    let mut methods = Vec::with_capacity(words.len());
    for word in &words {
        if word.is_empty() {
            continue;
        }
        methods.push(LinkMethod::parse_word(word).map_err(serde::de::Error::custom)?);
    }
    Ok(methods)
}

/// `<methods>` wrapper (inside `<ner>`): raw words before strict mapping.
#[derive(Deserialize)]
struct NerMethodWords {
    /// Raw `<method>` text values inside `<ner><methods>`.
    #[serde(default, rename = "method")]
    items: Vec<String>,
}

/// Unwraps `<ner><methods>` and maps each word to the strict [`NerMethod`] enum — see
/// [`de_link_methods`] for the shared semantics.
fn de_ner_methods<'de, D>(deserializer: D) -> Result<Vec<NerMethod>, D::Error>
where
    D: Deserializer<'de>,
{
    let words = NerMethodWords::deserialize(deserializer)?.items;
    let mut methods = Vec::with_capacity(words.len());
    for word in &words {
        if word.is_empty() {
            continue;
        }
        methods.push(NerMethod::parse_word(word).map_err(serde::de::Error::custom)?);
    }
    Ok(methods)
}

/// `<expressions>` wrapper (inside `<cross-domain-links>`): one `<expression>` item field.
#[derive(Deserialize)]
struct ExpressionList {
    /// Items inside `<expressions>`.
    #[serde(default, rename = "expression")]
    items: Vec<LinkExpression>,
}

/// Unwraps `<expressions><expression/></expressions>` to the flat
/// [`CrossDomainLinksConfig::expressions`] field.
fn de_expressions<'de, D>(deserializer: D) -> Result<Vec<LinkExpression>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(ExpressionList::deserialize(deserializer)?.items)
}

/// `<attributes>` wrapper (inside an `<entity>`): one `<attribute>` item field.
#[derive(Deserialize)]
struct EntityAttributeList {
    /// Items inside an entity's `<attributes>`.
    #[serde(default, rename = "attribute")]
    items: Vec<AttributeDef>,
}

/// Unwraps an entity's `<attributes><attribute/></attributes>` to the flat
/// [`EntityDef::attributes`] field.
fn de_entity_attributes<'de, D>(deserializer: D) -> Result<Vec<AttributeDef>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(EntityAttributeList::deserialize(deserializer)?.items)
}

/// `<synonyms>` wrapper (inside an `<entity>`): one `<synonym>` item field.
#[derive(Deserialize)]
struct SynonymList {
    /// Items inside `<synonyms>`.
    #[serde(default, rename = "synonym")]
    items: Vec<String>,
}

/// Unwraps `<synonyms><synonym/></synonyms>` to the flat [`EntityDef::synonyms`] field, dropping
/// empty items for the same parity reason as [`de_domains`].
fn de_synonyms<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = SynonymList::deserialize(deserializer)?.items;
    Ok(items.into_iter().filter(|word| !word.is_empty()).collect())
}

/// `<attributes>` wrapper (inside a `<relation>`): one `<attribute>` item field.
#[derive(Deserialize)]
struct RelationAttributeList {
    /// Items inside a relation's `<attributes>`.
    #[serde(default, rename = "attribute")]
    items: Vec<RelAttrDef>,
}

/// Unwraps a relation's `<attributes><attribute/></attributes>` to the flat
/// [`RelationDef::attributes`] field.
fn de_relation_attributes<'de, D>(deserializer: D) -> Result<Vec<RelAttrDef>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(RelationAttributeList::deserialize(deserializer)?.items)
}

/// `<regex-rules>` wrapper (inside `<extraction>`): one `<regex>` item field.
#[derive(Deserialize)]
struct RegexRuleList {
    /// Items inside `<regex-rules>`.
    #[serde(default, rename = "regex")]
    items: Vec<RegexRuleDef>,
}

/// Unwraps `<extraction><regex-rules><regex/></regex-rules>` to the flat
/// [`ExtractionDef::regex_rules`] field.
fn de_regex_rules<'de, D>(deserializer: D) -> Result<Vec<RegexRuleDef>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(RegexRuleList::deserialize(deserializer)?.items)
}

// ── Defaults + validation (oracle global_config.go / global_pool.go) ───────

impl GlobalConfig {
    /// Applies the oracle's defaults (`ApplyDefaults` in both Go config structs): equals min-words
    /// ≤ 0 → 2, LLM confidence threshold ≤ 0 → 0.7, batch size ≤ 0 → 5, empty expression relation
    /// types → `"same_entity"`, and each source without any domain gets `["default"]`. Absent
    /// cross-domain links stay absent (the oracle guards on a non-nil pointer); an absent or
    /// empty NER method list becomes the fallback `[regex, llm]` (both shapes parse to an empty
    /// vec here, so one check covers the oracle's two branches).
    fn apply_defaults(&mut self) {
        if let Some(cdl) = &mut self.cross_domain_links {
            // The oracle guards on a non-nil pointer before defaulting min-words.
            if let Some(equals) = cdl.equals.as_mut()
                && equals.min_words <= 0
            {
                equals.min_words = DEFAULT_EQUALS_MIN_WORDS;
            }
            if cdl.llm_confidence_threshold <= 0.0 {
                cdl.llm_confidence_threshold = DEFAULT_LLM_CONFIDENCE_THRESHOLD;
            }
            if cdl.batch_size <= 0 {
                cdl.batch_size = DEFAULT_LLM_BATCH_SIZE;
            }
            for expression in &mut cdl.expressions {
                if expression.relation_type.is_empty() {
                    expression.relation_type = DEFAULT_RELATION_TYPE.to_string();
                }
            }
        }

        if self.ner.methods.is_empty() {
            self.ner.methods = vec![NerMethod::Regex, NerMethod::Llm];
        }

        // Empty words are already filtered at parse time (parity with Go's encoding/xml), so an
        // empty list means "no <domain> items with text" — exactly the oracle's `!NonEmpty`.
        for source in &mut self.sources {
            if source.domains.is_empty() {
                source.domains.push("default".to_string());
            }
        }
    }

    /// Anchors every non-empty relative source path to the ontology directory (the directory
    /// holding `global.xml`) so consumers resolve sources against the file, not the process
    /// working directory. Absolute paths are left untouched.
    fn resolve_source_paths(&mut self, ontology_dir: &Path) {
        for source in &mut self.sources {
            if source.path.is_empty() {
                continue;
            }
            let path = Path::new(&source.path);
            if !path.is_absolute() {
                source.path = ontology_dir.join(path).to_string_lossy().into_owned();
            }
        }
    }

    /// Validates every section in the oracle's error precedence and compiles the regex rules in
    /// place (design D5): cross-domain link methods, NER methods, per-source path/type, then the
    /// entity pool (ids, attribute names, ref targets), relation pool (predicates, endpoint
    /// existence) and finally each extraction rule ([`RegexRuleDef::validate_and_compile`]).
    /// `file` names the ontology file in [`ConfigError::Regex`].
    fn validate(&mut self, file: &str) -> Result<(), ConfigError> {
        if let Some(cdl) = &self.cross_domain_links {
            // Method membership is guaranteed at parse time (strict enums); only emptiness —
            // the oracle's `len(m.Methods) == 0` check — remains.
            if cdl.methods.is_empty() {
                return Err(validation(
                    "cross-domain-links.methods must have at least one method",
                ));
            }
        }

        // Unreachable after apply_defaults (the loader always runs it first); kept for parity
        // with the oracle's Validate and as a guard against future call sites.
        if self.ner.methods.is_empty() {
            return Err(validation("ner.methods must have at least one method"));
        }

        for (index, source) in self.sources.iter().enumerate() {
            let position = index + 1; // the oracle numbers sources from 1
            if source.path.is_empty() {
                return Err(validation(format!("source {position}: path is required")));
            }
            if matches!(source.source_type, SourceType::Unknown(ref word) if word.is_empty()) {
                return Err(validation(format!("source {position}: type is required")));
            }
        }

        let mut entity_ids = HashSet::new();
        for entity in &self.entities {
            // Oracle message bug fixed (task-mandated): the Go code says "global entity
            // Predicate is required" — a copy-paste from the relation check below.
            if entity.id.is_empty() {
                return Err(validation("global entity id is required"));
            }
            // Same copy-paste bug class in the duplicate message, fixed consistently:
            // "duplicate global entity Predicate: %s" → "duplicate global entity id: ...".
            if !entity_ids.insert(entity.id.as_str()) {
                return Err(validation(format!(
                    "duplicate global entity id: {}",
                    entity.id
                )));
            }

            let mut attribute_names = HashSet::new();
            for attribute in &entity.attributes {
                if attribute.name.is_empty() {
                    return Err(validation(format!(
                        "attribute name is required for global entity {}",
                        entity.id
                    )));
                }
                if !attribute_names.insert(attribute.name.as_str()) {
                    return Err(validation(format!(
                        "duplicate attribute name {} in global entity {}",
                        attribute.name, entity.id
                    )));
                }
                if attribute.attr_type == AttributeType::Ref && attribute.target.is_empty() {
                    return Err(validation(format!(
                        "attribute {} in global entity {} has type 'ref' but no target specified",
                        attribute.name, entity.id
                    )));
                }
            }
        }

        let mut predicates = HashSet::new();
        for relation in &self.relations {
            if relation.predicate.is_empty() {
                return Err(validation("global relation Predicate is required"));
            }
            if !predicates.insert(relation.predicate.as_str()) {
                return Err(validation(format!(
                    "duplicate global relation Predicate: {}",
                    relation.predicate
                )));
            }
            if !entity_ids.contains(relation.source.as_str()) {
                return Err(validation(format!(
                    "global relation {} references non-existent source entity {}",
                    relation.predicate, relation.source
                )));
            }
            if !entity_ids.contains(relation.target.as_str()) {
                return Err(validation(format!(
                    "global relation {} references non-existent target entity {}",
                    relation.predicate, relation.target
                )));
            }
        }

        for rule in &mut self.extraction.regex_rules {
            rule.validate_and_compile(file)?;
        }
        Ok(())
    }
}

// ── Loader ─────────────────────────────────────────────────────────────────

/// Loads the global ontology from `ontology_dir/global.xml`: parse → oracle defaults →
/// source-path anchoring → validation + regex compilation (design D5/D6) in one pass.
///
/// File-presence semantics follow the oracle exactly: an empty directory name or a missing file
/// yields `Ok(None)`; any other I/O failure, malformed XML, a structurally unexpected document,
/// a violated semantic invariant, or an uncompilable regex pattern is an error
/// ([`ConfigError::Io`] / [`Xml`](ConfigError::Xml) / [`Validation`](ConfigError::Validation) /
/// [`Regex`](ConfigError::Regex)). The returned config is fully normalized: defaults applied,
/// every relative source path anchored to `ontology_dir`, every section validated, every rule
/// compiled.
pub fn load_global_config(
    ontology_dir: impl AsRef<Path>,
) -> Result<Option<GlobalConfig>, ConfigError> {
    let dir = ontology_dir.as_ref();
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }

    let path = dir.join(GLOBAL_XML_FILE);
    match std::fs::metadata(&path) {
        // Existence established; the read/parse pair and its error decoration live in `io_util`.
        Ok(_) => {
            let mut config = crate::io_util::read_xml_file::<GlobalConfig>(&path)?;
            config.apply_defaults();
            config.resolve_source_paths(dir);
            config.validate(&crate::io_util::display_path(&path))?;
            Ok(Some(config))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError::Io {
            path: crate::io_util::display_path(&path),
            source: err,
        }),
    }
}
