//! Global ontology (`global.xml`) loading — parsing only (task 3.1, revision 4).
//!
//! One parser for the whole file (design D6): in the Go oracle this document was parsed twice —
//! `config.LoadGlobalConfig` (sources / cross-domain links / NER) and `domain.LoadGlobalPool`
//! (entities / relations / extraction). Here a single [`load_global_config`] returns one
//! [`GlobalConfig`] with every block. This task performs **parsing only**: no oracle defaults,
//! no semantic validation, no regex compilation — those land in task 3.1b on top of this parser.
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
//!   preserves the raw word in `Unknown(String)` because task 3.1b's validation needs to tell
//!   "absent/empty" (`Unknown("")`) from a non-empty unrecognized word (the oracle checks
//!   emptiness only: `src.Type == ""`).

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// File name of the global ontology inside the ontology directory (oracle convention).
pub const GLOBAL_XML_FILE: &str = "global.xml";

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
    /// Reference to another entity (requires a non-empty `target` — checked by task 3.1b).
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
/// — task 3.1b turns that into the "type is required" error. The raw word must therefore survive:
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
        // Absent `type` attribute: the oracle's empty-string zero value, rejected later by 3.1b.
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
        // preserved verbatim for 3.1b's validation and diagnostics.
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

/// The parsed global ontology: every block of one `global.xml` file in a single pass (design D6).
///
/// Produced by [`load_global_config`] — parsing only: fields carry the raw values from the file,
/// no defaults applied and nothing validated yet (task 3.1b layers both on top). Every repeated
/// block lives inside its plural wrapper element in the D15 revision-4 format; each list field is
/// flat (`Vec`) with the wrapper consumed by a `deserialize_with` helper.
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    /// Ingestion data sources (`<sources><source/></sources>`); empty when absent.
    #[serde(default, deserialize_with = "de_sources")]
    pub sources: Vec<SourceConfig>,
    /// Cross-domain linking settings; `None` when the file has no `<cross-domain-links>`.
    #[serde(rename = "cross-domain-links")]
    pub cross_domain_links: Option<CrossDomainLinksConfig>,
    /// NER extraction methods; an absent block parses to empty (task 3.1b applies the oracle
    /// fallback `["regex", "llm"]`).
    #[serde(default)]
    pub ner: GlobalNerConfig,
    /// Global entity pool (`<entities><entity/></entities>`); empty when absent.
    #[serde(default, deserialize_with = "de_entities")]
    pub entities: Vec<EntityDef>,
    /// Global relation pool (`<relations><relation/></relations>`); empty when absent.
    #[serde(default, deserialize_with = "de_relations")]
    pub relations: Vec<RelationDef>,
    /// Extraction section with regex patterns as text (task 3.1b compiles them, design D5).
    #[serde(default)]
    pub extraction: ExtractionDef,
}

/// One ingestion data source (`<source>` element): scalar data in attributes plus a
/// `<domains><domain/></domains>` wrapper of domain names.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SourceConfig {
    /// Directory to index (`path` attribute); required — enforced by task 3.1b's validation.
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
    /// Domain names this source belongs to; an empty list stays empty here (task 3.1b defaults
    /// it to `["default"]`, matching the oracle).
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
    /// Linking methods in priority order; must be non-empty (checked by 3.1b) — unknown values
    /// already fail the parse ([`LinkMethod`]).
    #[serde(default, deserialize_with = "de_link_methods")]
    pub methods: Vec<LinkMethod>,
    /// Equals-method settings; `None` when the `<equals>` element is absent.
    pub equals: Option<EqualsConfig>,
    /// Minimum confidence for LLM linking results; an absent element parses to `0.0` (task 3.1b
    /// applies the oracle default of 0.7).
    #[serde(default, rename = "llm-confidence-threshold")]
    pub llm_confidence_threshold: f64,
    /// Entity pairs per LLM call; an absent element parses to `0` (task 3.1b applies 5).
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
    /// oracle default (2) in task 3.1b.
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
    /// (`"same_entity"`) in task 3.1b.
    #[serde(default, rename = "relation-type")]
    pub relation_type: String,
}

/// NER extraction settings (`<ner>` element) with its `<methods><method/></methods>` wrapper.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct GlobalNerConfig {
    /// Pipeline stages in order; an absent block or list parses to empty (task 3.1b applies the
    /// oracle fallback `["regex", "llm"]`).
    #[serde(default, deserialize_with = "de_ner_methods")]
    pub methods: Vec<NerMethod>,
}

/// An entity type of the global pool (`<entity>` element inside `<entities>`): scalar data in
/// attributes plus `<attributes>` and `<synonyms>` wrappers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EntityDef {
    /// Unique entity id; required — enforced by task 3.1b's validation.
    #[serde(default, rename = "@id")]
    pub id: String,
    /// Display name.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Description.
    #[serde(default, rename = "@description")]
    pub description: String,
    /// Attribute definitions; names must be unique per entity (checked by 3.1b).
    #[serde(default, deserialize_with = "de_entity_attributes")]
    pub attributes: Vec<AttributeDef>,
    /// Alternative surface forms used by the linker (`<synonyms><synonym/></synonyms>`).
    #[serde(default, deserialize_with = "de_synonyms")]
    pub synonyms: Vec<String>,
}

/// An attribute of an entity (`<attribute>` element inside `<attributes>`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AttributeDef {
    /// Unique within the owning entity; required — enforced by task 3.1b's validation.
    #[serde(default, rename = "@name")]
    pub name: String,
    /// Value kind (see [`AttributeType`]; absent `@type` → `Unknown`).
    #[serde(default, rename = "@type")]
    pub attr_type: AttributeType,
    /// Whether ingestion must find this attribute.
    #[serde(default, rename = "@required")]
    pub required: bool,
    /// Target entity id when `attr_type` is [`AttributeType::Ref`] (must be non-empty then — 3.1b).
    #[serde(default, rename = "@target")]
    pub target: String,
}

/// A relation type of the global pool (`<relation>` element inside `<relations>`): scalar data in
/// attributes plus an `<attributes>` wrapper.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RelationDef {
    /// Source entity id; must exist in the pooled entities (checked by 3.1b).
    #[serde(default, rename = "@source")]
    pub source: String,
    /// Unique relation predicate; required — enforced by task 3.1b's validation.
    #[serde(default, rename = "@predicate")]
    pub predicate: String,
    /// Target entity id; must exist in the pooled entities (checked by 3.1b).
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
    /// Regex rules with their patterns as text (task 3.1b compiles them, design D5); an empty
    /// list means "no rules". The field is renamed because quick-xml maps it to the wrapper
    /// element `<regex-rules>` by name.
    #[serde(default, rename = "regex-rules", deserialize_with = "de_regex_rules")]
    pub regex_rules: Vec<RegexRuleDef>,
}

/// A regex-based extraction rule (`<regex>` element inside `<extraction><regex-rules>`). Parse-only:
/// the pattern is stored as text — task 3.1b compiles it and adds the compiled form (design D5).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegexRuleDef {
    /// Rule identifier; required — enforced by task 3.1b's validation.
    #[serde(default, rename = "@id")]
    pub id: String,
    /// Entity id that matches are attributed to; required (checked by 3.1b).
    #[serde(default, rename = "@entity")]
    pub entity: String,
    /// Pattern source text exactly as written in the XML.
    #[serde(default, rename = "@pattern")]
    pub pattern: String,
    /// Confidence assigned to matches; must lie in `[0, 1]` (checked by 3.1b).
    #[serde(default, rename = "@confidence")]
    pub confidence: f64,
}

// ── Wrapper helpers ────────────────────────────────────────────────────────
//
// One private local struct + one `deserialize_with` function per wrapped list (design D15): the
// public API stays flat while quick-xml consumes each plural wrapper element through its own
// single-field struct. All are crate-private; they are serde glue, not a second model of the file.

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
fn de_entities<'de, D>(deserializer: D) -> Result<Vec<EntityDef>, D::Error>
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
fn de_relations<'de, D>(deserializer: D) -> Result<Vec<RelationDef>, D::Error>
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
fn de_domains<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(DomainList::deserialize(deserializer)?.items)
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

/// Unwraps `<synonyms><synonym/></synonyms>` to the flat [`EntityDef::synonyms`] field.
fn de_synonyms<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(SynonymList::deserialize(deserializer)?.items)
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

// ── Loader ─────────────────────────────────────────────────────────────────

/// Loads the global ontology from `ontology_dir/global.xml` — parsing only (task 3.1).
///
/// File-presence semantics follow the oracle exactly: an empty directory name or a missing file
/// yields `Ok(None)`; any other I/O failure, malformed XML, or a structurally unexpected document
/// is an error ([`ConfigError::Io`] / [`ConfigError::Xml`]). No defaults are applied and no
/// validation runs — task 3.1b layers both on top of the returned structure.
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
        Ok(_) => crate::io_util::read_xml_file::<GlobalConfig>(&path).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError::Io {
            path: crate::io_util::display_path(&path),
            source: err,
        }),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Parses an in-memory document through the exact deserializer of `load_global_config` minus
    /// file I/O.
    fn parse(xml: &str) -> Result<GlobalConfig, ConfigError> {
        quick_xml::de::from_str::<GlobalConfig>(xml).map_err(|source| ConfigError::Xml {
            path: "test.xml".to_string(),
            source,
        })
    }

    #[test]
    fn load_with_empty_dir_yields_none() {
        assert!(load_global_config("").is_ok_and(|cfg| cfg.is_none()));
    }

    #[test]
    fn load_with_missing_file_yields_none() {
        let dir =
            std::env::temp_dir().join(format!("synopsis-ontology-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_global_config(&dir).is_ok_and(|cfg| cfg.is_none()));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn minimal_document_parses_to_empty_structure() {
        // Parse-only: absent blocks stay empty — task 3.1b applies the oracle defaults on top.
        let cfg = parse("<global></global>").unwrap();
        assert!(cfg.sources.is_empty());
        assert!(cfg.cross_domain_links.is_none(), "absent block stays None");
        assert!(cfg.ner.methods.is_empty());
        assert!(cfg.entities.is_empty());
        assert!(cfg.relations.is_empty());
        assert!(cfg.extraction.regex_rules.is_empty());

        // D15 revision 4: <expression> sits inside an <expressions> wrapper, which in turn is a
        // direct child of <cross-domain-links>.
        let cdl = parse(
            "<global><cross-domain-links>\
             <methods><method>expression</method></methods>\
             <expressions><expression><name>x</name></expression></expressions>\
             </cross-domain-links></global>",
        )
        .unwrap()
        .cross_domain_links
        .expect("block present");
        // Both threshold elements are absent: the raw zero values survive, defaults come in 3.1b.
        assert_eq!(cdl.llm_confidence_threshold, 0.0);
        assert_eq!(cdl.batch_size, 0);
        assert!(cdl.equals.is_none());
        assert_eq!(cdl.expressions[0].relation_type, "");
    }

    #[test]
    fn source_fields_parse_to_raw_values() {
        // Parse-only: absent fields keep raw zero values; 3.1b defaults empty domains to
        // ["default"] and rejects missing path/type with the oracle's messages. D15 revision 4:
        // <domain> items sit inside a <domains> wrapper.
        let cfg = parse("<global><sources><source type=\"markdown\"/></sources></global>").unwrap();
        assert!(cfg.sources[0].path.is_empty());
        assert_eq!(cfg.sources[0].domains, Vec::<String>::new());

        // Absent `type` attribute → the enum's Default (Unknown("")), not a parse error.
        let cfg =
            parse("<global><sources><source path=\"a\"></source></sources></global>").unwrap();
        assert_eq!(cfg.sources[0].path, "a");
        assert_eq!(cfg.sources[0].source_type, SourceType::default());

        // Unknown (non-empty) types are tolerated at parse time — the oracle only checks presence.
        let cfg = parse(
            "<global><sources><source path=\"a\" type=\"confluence\"></source></sources></global>",
        )
        .unwrap();
        assert_eq!(
            cfg.sources[0].source_type,
            SourceType::Unknown("confluence".to_string())
        );

        // Multiple <domain> items in file order.
        let cfg = parse(
            "<global><sources><source path=\"a\" type=\"markdown\">\
             <domains><domain>x</domain><domain>y</domain></domains>\
             </source></sources></global>",
        )
        .unwrap();
        assert_eq!(
            cfg.sources[0].domains,
            vec!["x".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn unknown_method_values_are_rejected_at_parse() {
        // Strict enums (design D15 revision 4): the Go oracle validates exactly these sets in
        // Validate(), so an unknown word is a parse error here instead of a 3.1b validation one.
        let err = parse(
            "<global><cross-domain-links>\
             <methods><method>bogus</method></methods>\
             </cross-domain-links></global>",
        )
        .expect_err("bogus link method must fail parsing");
        match &err {
            ConfigError::Xml { path, .. } => assert_eq!(path, "test.xml"),
            other => panic!("expected Xml error, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("\"bogus\""), "message names the word: {msg}");

        // Matching is case-sensitive (the oracle compares exact words; derived matching of
        // rename_all-lowercase identifiers is too).
        let err = parse(
            "<global><cross-domain-links>\
             <methods><method>Expression</method></methods>\
             </cross-domain-links></global>",
        )
        .expect_err("wrong-case link method must fail parsing");
        assert!(err.to_string().contains("\"Expression\""));

        // Same strictness for the NER method list.
        let err = parse("<global><ner><methods><method>spacy</method></methods></ner></global>")
            .expect_err("bogus ner method must fail parsing");
        assert!(err.to_string().contains("\"spacy\""));
    }

    #[test]
    fn empty_method_elements_contribute_nothing_like_the_oracle() {
        // Go's encoding/xml skips a text-less <method> element when unmarshalling into []string;
        // the helpers mirror that by dropping empty items before strict mapping.
        let cfg = parse(
            "<global><ner>\
             <methods><method></method><method>regex</method></methods>\
             </ner></global>",
        )
        .unwrap();
        assert_eq!(cfg.ner.methods, vec![NerMethod::Regex]);
    }

    #[test]
    fn attribute_only_method_element_parses_as_its_attribute_name() {
        // The oracle fixture writes the equals method as `<method>equals</method>` (an attribute,
        // not text). Go's encoding/xml and quick-xml both surface that element's single empty
        // attribute name as its string value, so it parses to `Equals` in both — the spelling is
        // preserved verbatim in the fixture precisely because this quirk keeps parity.
        let cfg = parse(
            "<global><cross-domain-links>\
             <methods><method>expression</method><method>equals</method><method>llm</method></methods>\
             </cross-domain-links></global>",
        )
        .unwrap();
        assert_eq!(
            cfg.cross_domain_links.expect("block present").methods,
            vec![LinkMethod::Expression, LinkMethod::Equals, LinkMethod::Llm]
        );
    }

    #[test]
    fn absent_attributes_parse_to_empty_defaults() {
        // D15 revision 4: <entity> sits inside an <entities> wrapper; its attributes and
        // synonyms sit in their own wrappers.
        let cfg = parse(
            "<global><entities>\
             <entity name=\"A\"><attributes>\
             <attribute name=\"x\" required=\"true\"></attribute>\
             </attributes></entity>\
             </entities></global>",
        )
        .unwrap();
        let entity = &cfg.entities[0];
        // Absent @id stays "" (3.1b: "global entity id is required").
        assert!(entity.id.is_empty());
        assert_eq!(entity.name, "A");
        assert!(entity.description.is_empty());
        let attribute = &entity.attributes[0];
        // Absent @type → the tolerant enum's Default (Unknown); absent @target stays "".
        assert_eq!(attribute.attr_type, AttributeType::default());
        assert!(attribute.required);
        assert!(attribute.target.is_empty());
    }

    #[test]
    fn regex_rules_parse_without_compiling() {
        // Parse-only (3.1b compiles per D5): the pattern stays text; even an invalid pattern parses.
        let cfg = parse(
            "<global><extraction>\
             <regex-rules><regex id=\"email\" entity=\"email\" pattern=\"[unclosed\"\
             confidence=\"0.9\"></regex></regex-rules>\
             </extraction></global>",
        )
        .unwrap();
        assert_eq!(cfg.extraction.regex_rules.len(), 1);
        let rule = &cfg.extraction.regex_rules[0];
        assert_eq!(rule.id, "email");
        assert_eq!(rule.entity, "email");
        assert_eq!(rule.pattern, "[unclosed");
        assert!((rule.confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn wrapped_attribute_and_synonym_blocks_parse_independently() {
        // D15 revision 4: <attribute> and <synonym> items live in separate wrappers, so their
        // relative order inside the entity no longer matters — each list is read from its own
        // container. Both orders below parse to the same values.
        let doc_a = "<global><entities>\
             <entity id=\"a\" name=\"A\">\
               <attributes><attribute name=\"x\" type=\"string\"></attribute></attributes>\
               <synonyms><synonym>s1</synonym><synonym>s2</synonym></synonyms>\
             </entity></entities></global>";
        let doc_b = "<global><entities>\
             <entity id=\"a\" name=\"A\">\
               <synonyms><synonym>s1</synonym><synonym>s2</synonym></synonyms>\
               <attributes><attribute name=\"x\" type=\"string\"></attribute></attributes>\
             </entity></entities></global>";

        for doc in [doc_a, doc_b] {
            let entity = &parse(doc).unwrap().entities[0];
            assert_eq!(entity.attributes.len(), 1);
            assert_eq!(entity.attributes[0].attr_type, AttributeType::String);
            assert_eq!(entity.synonyms, vec!["s1".to_string(), "s2".to_string()]);
        }
    }

    #[test]
    fn relation_children_parse_with_attributes() {
        // D15 revision 4: <relation> sits inside a <relations> wrapper; its <attribute> items
        // keep the oracle's attribute-only shape inside an <attributes> wrapper.
        let cfg = parse(
            "<global><relations>\
             <relation source=\"a\" predicate=\"p\" target=\"b\" description=\"d\">\
               <attributes><attribute name=\"since\" type=\"date\"/></attributes>\
             </relation></relations></global>",
        )
        .unwrap();
        let relation = &cfg.relations[0];
        assert_eq!(relation.source, "a");
        assert_eq!(relation.predicate, "p");
        assert_eq!(relation.target, "b");
        assert_eq!(relation.description, "d");
        assert_eq!(relation.attributes.len(), 1);
        assert_eq!(relation.attributes[0].name, "since");
        assert_eq!(relation.attributes[0].attr_type, "date");
    }

    #[test]
    fn attribute_types_match_known_words_and_tolerate_others() {
        // Tolerant enum via pure derive + #[serde(other)]: known words (case-insensitive per the
        // rename_all-lowercase identifiers are exact — the oracle ships lowercase words, so match
        // the fixture's spelling) map to variants; anything else lands in Unknown.
        let cfg = parse(
            "<global><entities>\
             <entity id=\"a\" name=\"A\"><attributes>\
               <attribute name=\"x\" type=\"date\"></attribute>\
               <attribute name=\"y\" type=\"ref\" target=\"b\"></attribute>\
               <attribute name=\"z\" type=\"weird\"></attribute>\
             </attributes></entity>\
             </entities></global>",
        )
        .unwrap();
        let attributes = &cfg.entities[0].attributes;
        assert_eq!(attributes[0].attr_type, AttributeType::Date);
        assert_eq!(attributes[1].attr_type, AttributeType::Ref);
        assert!(!attributes[1].target.is_empty());
        assert_eq!(attributes[2].attr_type, AttributeType::Unknown);
    }
}
