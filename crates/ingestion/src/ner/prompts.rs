//! NER prompt templates (ingestion-ner task 2.2, design D4).
//!
//! The `llm` provider renders a system + user prompt per domain and sends both
//! to the model. The templates are **data**, not code: they are embedded via
//! `include_str!` and a user can override them on disk under
//! `{prompts_path}/ner/{system,user}.tmpl` without a rebuild (the same pattern
//! as `crates/graph/src/prompts.rs`). A missing override file is the normal
//! case and is *not* an error; a loaded override is recorded in
//! [`NerPrompts::notes`].
//!
//! Oracle mapping: `../synopsis/configs/prompts/ner/{system,user}.tmpl` —
//! a functional copy, re-expressed for minijinja (same prompt text and data
//! shape; the field paths are snake_case). The oracle's `join` template
//! function is minijinja's built-in `join` filter here.
//!
//! # Deliberate deviations
//!
//! - Go `text/template` → Jinja2 / [`minijinja`]: block trimming via the
//!   environment's `trim_blocks`/`lstrip_blocks` instead of `{{- ... -}}`.
//! - The oracle's user template says "from the provided content in separate
//!   message" because the content went out as a separate attachment. Our
//!   `LlmClient::call` has no attachments parameter (design D4), so the
//!   content renders directly into the user prompt body under a `CONTENT:`
//!   label — the wire shape differs, extracted-output parity is what matters.
//! - **Binding requirement (human decision 2026-08-23):** the rendered user
//!   prompt includes an explicit *Document context* block (the chunk's
//!   section path) when the chunk metadata carries one, and omits it when it
//!   does not. The oracle only reached the LLM through breadcrumb prefixes
//!   baked into the chunk text; our chunks are clean slices (the offset-bug
//!   fix recorded in ingestion-sources), so the context must be explicit.
//! - The oracle's JSON example is missing the comma after `"confidence"`;
//!   the embedded default is a valid JSON example.
//!
//! # Cache key
//!
//! The SHA-256 hex of each template **source** (not the rendered data) is
//! exposed via [`NerPrompts::template_hashes`] (design D4): changing a prompt
//! changes the hash and invalidates the LLM-NER cache automatically
//! (task 2.4).

use std::path::Path;

use config::DomainConfig;
use config::ontology::{AttributeDef, AttributeType, EntityDef, RelationDef};
use minijinja::Environment;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::IngestionError;

/// Embedded system-prompt default (the functional Jinja2 rewrite of the
/// oracle `configs/prompts/ner/system.tmpl`).
const EMBEDDED_SYSTEM: &str = include_str!("templates/system.tmpl");
/// Embedded user-prompt default (the functional Jinja2 rewrite of the oracle
/// `configs/prompts/ner/user.tmpl`).
const EMBEDDED_USER: &str = include_str!("templates/user.tmpl");

// ── Render data (private: the template shape is this module's business) ────

/// One entity type as bound into the system prompt template
/// (`entity.id` / `entity.description` / `entity.attributes` /
/// `entity.synonyms`).
#[derive(Debug, Serialize)]
struct EntityData {
    /// Entity type id.
    id: String,
    /// Human description.
    description: String,
    /// Pre-formatted attribute line bodies (the template's `attribute`).
    attributes: Vec<String>,
    /// Alternative surface forms.
    synonyms: Vec<String>,
}

impl EntityData {
    /// Builds the template data from a domain entity definition.
    fn from_def(def: &EntityDef) -> Self {
        Self {
            id: def.id.clone(),
            description: def.description.clone(),
            attributes: def.attributes.iter().map(format_entity_attribute).collect(),
            synonyms: def.synonyms.clone(),
        }
    }
}

/// One relation type as bound into the system prompt template
/// (`relation.predicate` / `relation.description` / `relation.source` /
/// `relation.target` / `relation.attributes`).
#[derive(Debug, Serialize)]
struct RelationData {
    /// Relation predicate.
    predicate: String,
    /// Human description.
    description: String,
    /// Source entity id.
    source: String,
    /// Target entity id.
    target: String,
    /// Pre-formatted attribute line bodies (the template's `attribute`).
    attributes: Vec<String>,
}

impl RelationData {
    /// Builds the template data from a domain relation definition.
    fn from_def(def: &RelationDef) -> Self {
        Self {
            predicate: def.predicate.clone(),
            description: def.description.clone(),
            source: def.source.clone(),
            target: def.target.clone(),
            attributes: def
                .attributes
                .iter()
                .map(|attribute| format!("{} ({})", attribute.name, attribute.attr_type))
                .collect(),
        }
    }
}

/// Formats one entity-attribute line body: `name (type)`, with a
/// ` [required]` suffix when the attribute is required and a ` -> target`
/// suffix for `ref` attributes with a non-empty target — the oracle's
/// inline template conditionals, moved to code (the minijinja
/// `trim_blocks` setting eats the newline of a line ending in a block tag,
/// so the per-line logic lives here instead).
fn format_entity_attribute(def: &AttributeDef) -> String {
    let mut line = format!("{} ({})", def.name, attr_type_word(def.attr_type));
    if def.required {
        line.push_str(" [required]");
    }
    if def.attr_type == AttributeType::Ref && !def.target.is_empty() {
        line.push_str(&format!(" -> {}", def.target));
    }
    line
}

/// The render input for the system prompt: one domain's schema
/// (`entities` / `relations` / `with_json_example`).
#[derive(Debug, Serialize)]
struct SystemData {
    /// Entity types of the domain.
    entities: Vec<EntityData>,
    /// Relation types of the domain.
    relations: Vec<RelationData>,
    /// Whether to append the JSON output example.
    with_json_example: bool,
}

impl SystemData {
    /// Builds the system prompt data from a domain config.
    fn from_domain(domain: &DomainConfig, with_json_example: bool) -> Self {
        Self {
            entities: domain.entities.iter().map(EntityData::from_def).collect(),
            relations: domain
                .relations
                .iter()
                .map(RelationData::from_def)
                .collect(),
            with_json_example,
        }
    }
}

/// The render input for the user prompt: type lists + chunk context + content
/// (`entity_types` / `relation_types` / `section_path` / `content`).
#[derive(Debug, Serialize)]
struct UserData {
    /// Entity type ids of the domain.
    entity_types: Vec<String>,
    /// Relation predicates of the domain.
    relation_types: Vec<String>,
    /// The chunk's section path (`A > B > C`) from its metadata; `None`
    /// omits the Document context block.
    section_path: Option<String>,
    /// The clean chunk text.
    content: String,
}

/// The template word for an [`AttributeType`] (the oracle prints the raw XML
/// word; known words map 1:1). `Unknown` — an unrecognized word whose raw
/// text the config crate does not retain — renders as `"unknown"`.
fn attr_type_word(kind: AttributeType) -> &'static str {
    match kind {
        AttributeType::String => "string",
        AttributeType::Date => "date",
        AttributeType::Number => "number",
        AttributeType::Ref => "ref",
        AttributeType::Boolean => "boolean",
        AttributeType::Unknown => "unknown",
    }
}

/// The chunk's section path from its metadata (the binding D4 context block).
///
/// The chunkers store the heading path under the `breadcrumb` key as one
/// `> Title` line per ancestor (indent = depth). The prompt wants a single
/// line (`A > B > C`): each line is stripped of its indent and leading `>`,
/// then the parts are joined with `" > "`. `None` when the metadata carries
/// no (or an empty, or non-string) breadcrumb — the template then omits the
/// context block entirely.
fn section_path(metadata: &Map<String, Value>) -> Option<String> {
    let breadcrumb = metadata.get("breadcrumb")?.as_str()?;
    let path = breadcrumb
        .lines()
        .map(|line| line.trim().trim_start_matches('>').trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" > ");
    (!path.is_empty()).then_some(path)
}

// ── Loader + render ─────────────────────────────────────────────────────────

/// The SHA-256 hex digests of the two template sources (the D4 cache key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateHashes {
    /// Hex sha256 of the system template source.
    pub system: String,
    /// Hex sha256 of the user template source.
    pub user: String,
}

/// Loaded NER prompt templates (system + user), their source hashes, and the
/// notes recording which sources were used (design D4).
///
/// Built once per LLM-NER construction (task 2.5): the minijinja environment
/// is created at load and the compiled templates are cached on it, so
/// rendering in the per-domain loop does not re-parse.
/// [`NerPrompts::template_hashes`] feeds the LLM cache key (task 2.4).
#[derive(Debug)]
pub struct NerPrompts {
    /// The environment holding the compiled templates.
    env: Environment<'static>,
    /// Hex sha256 of the system template source.
    system_hash: String,
    /// Hex sha256 of the user template source.
    user_hash: String,
    /// Notes recording which override files were loaded (empty when both
    /// templates fell back to the embedded defaults).
    notes: Vec<String>,
}

impl NerPrompts {
    /// Internal name of the system template on the environment.
    const SYSTEM_NAME: &'static str = "ner-system";
    /// Internal name of the user template on the environment.
    const USER_NAME: &'static str = "ner-user";

    /// Render the system prompt for one domain.
    ///
    /// `with_json_example` appends the JSON output example block (the
    /// provider passes `true` — oracle `renderSystemPrompt(cfg, true)`).
    pub fn render_system(
        &self,
        domain: &DomainConfig,
        with_json_example: bool,
    ) -> Result<String, IngestionError> {
        let data = SystemData::from_domain(domain, with_json_example);
        self.render(Self::SYSTEM_NAME, "system", data)
    }

    /// Render the user prompt for one chunk of one domain.
    ///
    /// `content` is the clean chunk text (no prefixes — design D4); `metadata`
    /// is the chunk's metadata bag, from which the Document context block is
    /// taken when a `breadcrumb` is present (the binding requirement).
    pub fn render_user(
        &self,
        domain: &DomainConfig,
        content: &str,
        metadata: &Map<String, Value>,
    ) -> Result<String, IngestionError> {
        let data = UserData {
            entity_types: domain
                .entities
                .iter()
                .map(|entity| entity.id.clone())
                .collect(),
            relation_types: domain
                .relations
                .iter()
                .map(|relation| relation.predicate.clone())
                .collect(),
            section_path: section_path(metadata),
            content: content.to_owned(),
        };
        self.render(Self::USER_NAME, "user", data)
    }

    /// Look up a compiled template and render it with the given context.
    ///
    /// `template` is the environment's internal name; `display` is the short
    /// name (`"system"` / `"user"`) used in error messages.
    fn render<S: Serialize>(
        &self,
        template: &str,
        display: &str,
        ctx: S,
    ) -> Result<String, IngestionError> {
        let template = self.env.get_template(template).map_err(|source| {
            IngestionError::PromptTemplateRender {
                name: display.to_owned(),
                source,
            }
        })?;
        template
            .render(ctx)
            .map_err(|source| IngestionError::PromptTemplateRender {
                name: display.to_owned(),
                source,
            })
    }

    /// The template source hashes for the D4 cache key.
    #[must_use]
    pub fn template_hashes(&self) -> TemplateHashes {
        TemplateHashes {
            system: self.system_hash.clone(),
            user: self.user_hash.clone(),
        }
    }

    /// Notes recorded while loading (a loaded override is named here; empty
    /// when both templates fell back to the embedded defaults).
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// Load the NER templates from `{prompts_path}/ner/`.
///
/// A present `{system,user}.tmpl` file wins over the embedded default and is
/// recorded in the returned notes; a missing file falls back to the embedded
/// default silently (absence is the normal case). A file that is present but
/// unreadable, or a template that fails to parse, is an error.
pub fn load_ner_prompts(prompts_path: &str) -> Result<NerPrompts, IngestionError> {
    let base = Path::new(prompts_path).join("ner");

    let (system_source, system_note) = load_template("system", &base, EMBEDDED_SYSTEM)?;
    let (user_source, user_note) = load_template("user", &base, EMBEDDED_USER)?;

    // Hash the *sources* (design D4): a changed prompt changes the hash and
    // invalidates the LLM-NER cache.
    let system_hash = sha256_hex(system_source.as_bytes());
    let user_hash = sha256_hex(user_source.as_bytes());

    let mut env = Environment::new();
    // Clean prompt output: block tags ({% for %}/{% endfor %}) on their own
    // lines contribute no stray whitespace (the oracle trims with `{{- -}}`).
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    // Preserve the template's final newline (keeps the rendered prompt
    // byte-predictable).
    env.set_keep_trailing_newline(true);
    // `add_template_owned` parses eagerly, so a broken override fails at load.
    env.add_template_owned(NerPrompts::SYSTEM_NAME, system_source)
        .map_err(|source| IngestionError::PromptTemplateParse {
            name: "system".to_owned(),
            source,
        })?;
    env.add_template_owned(NerPrompts::USER_NAME, user_source)
        .map_err(|source| IngestionError::PromptTemplateParse {
            name: "user".to_owned(),
            source,
        })?;

    let mut notes = Vec::new();
    if let Some(note) = system_note {
        notes.push(note);
    }
    if let Some(note) = user_note {
        notes.push(note);
    }

    Ok(NerPrompts {
        env,
        system_hash,
        user_hash,
        notes,
    })
}

/// Read one template by name from `base/{name}.tmpl`, falling back to
/// `embedded` when the file is absent. Returns the source and, when an
/// override was actually loaded, a note naming the file.
fn load_template(
    name: &str,
    base: &Path,
    embedded: &'static str,
) -> Result<(String, Option<String>), IngestionError> {
    let path = base.join(format!("{name}.tmpl"));
    match std::fs::read_to_string(&path) {
        Ok(source) => Ok((
            source,
            Some(format!("prompt {name}: using override {}", path.display())),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((embedded.to_owned(), None)),
        Err(err) => Err(IngestionError::PromptTemplateIo { path, source: err }),
    }
}

/// Hex SHA-256 of a byte slice (the template-source hash for the D4 key).
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (the fixtures are
    // compile-time constants).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    use config::ontology::{
        AttributeDef, AttributeType, EntityDef, ExtractionDef, RelAttrDef, RelationDef,
    };
    use config::{ConfidencePolicy, DomainConfig};

    use super::*;

    /// A temp dir unique to one test (the repo's established pattern:
    /// `temp_dir()` + `process::id()` + a per-test name suffix).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ingestion-ner-prompts-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A small domain schema with attributes (incl. required + ref) and
    /// synonyms, and one relation with an attribute.
    fn sample_domain() -> DomainConfig {
        DomainConfig {
            name: "hr".to_owned(),
            version: "1".to_owned(),
            description: String::new(),
            entities: vec![
                EntityDef {
                    id: "employee".to_owned(),
                    name: "Employee".to_owned(),
                    description: "A person employed by the organization".to_owned(),
                    attributes: vec![
                        AttributeDef {
                            name: "email".to_owned(),
                            attr_type: AttributeType::String,
                            required: true,
                            target: String::new(),
                        },
                        AttributeDef {
                            name: "manager".to_owned(),
                            attr_type: AttributeType::Ref,
                            required: false,
                            target: "employee".to_owned(),
                        },
                    ],
                    synonyms: vec!["staff".to_owned(), "worker".to_owned()],
                },
                EntityDef {
                    id: "company".to_owned(),
                    name: "Company".to_owned(),
                    description: "An organization".to_owned(),
                    attributes: vec![],
                    synonyms: vec![],
                },
            ],
            relations: vec![RelationDef {
                source: "employee".to_owned(),
                predicate: "works_for".to_owned(),
                target: "company".to_owned(),
                description: "An employee works for a company".to_owned(),
                attributes: vec![RelAttrDef {
                    name: "since".to_owned(),
                    attr_type: "date".to_owned(),
                }],
            }],
            extraction: ExtractionDef::default(),
            confidence: ConfidencePolicy::default(),
        }
    }

    /// Loads the prompts against a nonexistent path: both templates fall back
    /// to the embedded defaults.
    fn embedded_prompts() -> NerPrompts {
        load_ner_prompts("/nonexistent/prompts-path").unwrap()
    }

    #[test]
    fn user_prompt_includes_context_block_with_breadcrumbs() {
        let prompts = embedded_prompts();
        // Embedded fallback: no override was loaded, so no notes.
        assert!(prompts.notes().is_empty());
        let mut metadata = Map::new();
        metadata.insert(
            "breadcrumb".to_owned(),
            Value::String("> A\n  > B\n    > C".to_owned()),
        );

        let user = prompts
            .render_user(&sample_domain(), "Alice works at Acme.", &metadata)
            .unwrap();

        // The binding block: explicit section path, rendered before the text.
        assert!(
            user.contains("---\nDocument context:\n  Section path: A > B > C\n"),
            "{user}"
        );
        let context_pos = user.find("Document context:").unwrap();
        let content_pos = user.find("CONTENT:").unwrap();
        assert!(context_pos < content_pos, "context precedes content");
        // The clean chunk text, unprefixed.
        assert!(user.contains("Alice works at Acme."), "{user}");
    }

    #[test]
    fn user_prompt_omits_context_block_without_breadcrumbs() {
        let prompts = embedded_prompts();

        // No breadcrumb key at all.
        let user = prompts
            .render_user(&sample_domain(), "Alice works at Acme.", &Map::new())
            .unwrap();
        assert!(!user.contains("Document context"), "{user}");
        assert!(user.contains("Alice works at Acme."), "{user}");

        // A present but empty breadcrumb is treated as absent.
        let mut metadata = Map::new();
        metadata.insert("breadcrumb".to_owned(), Value::String("  \n".to_owned()));
        let user = prompts
            .render_user(&sample_domain(), "Alice works at Acme.", &metadata)
            .unwrap();
        assert!(!user.contains("Document context"), "{user}");
    }

    #[test]
    fn user_prompt_lists_types_and_omits_empty_lists() {
        let prompts = embedded_prompts();
        let user = prompts
            .render_user(&sample_domain(), "text", &Map::new())
            .unwrap();
        assert!(
            user.contains("Entity types to extract: employee, company"),
            "{user}"
        );
        assert!(
            user.contains("Relation types to extract: works_for"),
            "{user}"
        );

        // An empty domain schema renders no type lists at all.
        let domain = DomainConfig {
            entities: vec![],
            relations: vec![],
            ..sample_domain()
        };
        let user = prompts.render_user(&domain, "text", &Map::new()).unwrap();
        assert!(!user.contains("Entity types to extract"), "{user}");
        assert!(!user.contains("Relation types to extract"), "{user}");
    }

    #[test]
    fn system_prompt_renders_schema_with_attributes_and_synonyms() {
        let system = embedded_prompts()
            .render_system(&sample_domain(), false)
            .unwrap();

        assert!(
            system.contains("- employee: A person employed by the organization"),
            "{system}"
        );
        assert!(system.contains("- email (string) [required]"), "{system}");
        assert!(system.contains("- manager (ref) -> employee"), "{system}");
        assert!(system.contains("Synonyms: staff, worker"), "{system}");
        // The second entity has no attributes and no synonyms.
        assert!(system.contains("- company: An organization"), "{system}");
        assert_eq!(
            system.matches("Attributes:").count(),
            2,
            "entity + relation attribute blocks only: {system}"
        );
        assert!(
            system.contains("- works_for: An employee works for a company"),
            "{system}"
        );
        assert!(
            system.contains("Source: employee -> Target: company"),
            "{system}"
        );
        assert!(system.contains("- since (date)"), "{system}");
    }

    #[test]
    fn system_prompt_empty_schema_keeps_the_header() {
        let domain = DomainConfig {
            entities: vec![],
            relations: vec![],
            ..sample_domain()
        };
        let system = embedded_prompts().render_system(&domain, false).unwrap();
        assert!(system.contains("ENTITY TYPES:"), "{system}");
        assert!(system.contains("No entity types defined."), "{system}");
        assert!(!system.contains("RELATION TYPES:"), "{system}");
        assert!(
            system.contains("entity and relation extraction system"),
            "{system}"
        );
    }

    #[test]
    fn system_prompt_json_example_flag() {
        let prompts = embedded_prompts();

        let with_example = prompts.render_system(&sample_domain(), true).unwrap();
        assert!(with_example.contains("OUTPUT FORMAT:"), "{with_example}");
        // The oracle's example is missing this comma; the default is valid.
        assert!(
            with_example.contains("\"confidence\": <estimated confidence>,"),
            "{with_example}"
        );
        assert!(
            with_example.contains("\"subject_type\": \"<entity type>\""),
            "{with_example}"
        );

        let without_example = prompts.render_system(&sample_domain(), false).unwrap();
        assert!(
            !without_example.contains("OUTPUT FORMAT:"),
            "{without_example}"
        );
    }

    #[test]
    fn override_files_win_and_are_noted() {
        // Both overrides present: both win, both are recorded.
        let dir = temp_dir("override-both");
        std::fs::create_dir_all(dir.join("ner")).unwrap();
        std::fs::write(dir.join("ner").join("system.tmpl"), "SYS OVERRIDE\n").unwrap();
        std::fs::write(dir.join("ner").join("user.tmpl"), "USER OVERRIDE\n").unwrap();

        let prompts = load_ner_prompts(&dir.to_string_lossy()).unwrap();

        assert_eq!(
            prompts.render_system(&sample_domain(), true).unwrap(),
            "SYS OVERRIDE\n"
        );
        assert_eq!(
            prompts
                .render_user(&sample_domain(), "x", &Map::new())
                .unwrap(),
            "USER OVERRIDE\n"
        );

        let notes = prompts.notes();
        assert_eq!(notes.len(), 2, "both overrides recorded");
        assert!(notes[0].contains("prompt system"));
        assert!(notes[0].ends_with("system.tmpl"));
        assert!(notes[1].contains("prompt user"));
        assert!(notes[1].ends_with("user.tmpl"));

        // Only the system override present: it wins, the user template falls
        // back to the embedded default, only one note is recorded.
        let dir = temp_dir("override-system-only");
        std::fs::create_dir_all(dir.join("ner")).unwrap();
        std::fs::write(dir.join("ner").join("system.tmpl"), "SYS OVERRIDE\n").unwrap();

        let prompts = load_ner_prompts(&dir.to_string_lossy()).unwrap();
        assert_eq!(
            prompts.render_system(&sample_domain(), true).unwrap(),
            "SYS OVERRIDE\n"
        );
        assert!(
            prompts
                .render_user(&sample_domain(), "x", &Map::new())
                .unwrap()
                .contains("IMPORTANT")
        );
        assert_eq!(prompts.notes().len(), 1);
        assert!(prompts.notes()[0].contains("prompt system"));
    }

    #[test]
    fn template_hashes_are_stable_and_track_the_source() {
        let base = embedded_prompts().template_hashes();
        let other = embedded_prompts().template_hashes();
        assert_eq!(base, other, "hashes must be stable across loads");
        // The hash is the sha256 of the embedded source (not rendered data).
        assert_eq!(base.system, sha256_hex(EMBEDDED_SYSTEM.as_bytes()));
        assert_eq!(base.user, sha256_hex(EMBEDDED_USER.as_bytes()));
        // A 64-char lowercase hex digest.
        assert_eq!(base.system.len(), 64);
        assert!(base.system.chars().all(|c| c.is_ascii_hexdigit()));

        // A changed override changes only the hash of the changed template.
        let dir = temp_dir("hash-change");
        std::fs::create_dir_all(dir.join("ner")).unwrap();
        std::fs::write(dir.join("ner").join("system.tmpl"), "DIFFERENT\n").unwrap();
        let changed = load_ner_prompts(&dir.to_string_lossy())
            .unwrap()
            .template_hashes();
        assert_ne!(changed.system, base.system, "changed template changes hash");
        assert_eq!(changed.user, base.user, "untouched template keeps hash");
        assert_eq!(
            changed.system,
            sha256_hex("DIFFERENT\n".as_bytes()),
            "the hash is of the override source"
        );
    }

    #[test]
    fn broken_override_template_fails_to_parse_at_load() {
        let dir = temp_dir("broken-user");
        std::fs::create_dir_all(dir.join("ner")).unwrap();
        std::fs::write(
            dir.join("ner").join("user.tmpl"),
            "{% for x in items %}unclosed",
        )
        .unwrap();

        let err = load_ner_prompts(&dir.to_string_lossy()).unwrap_err();
        assert!(
            matches!(err, IngestionError::PromptTemplateParse { ref name, .. } if name == "user"),
            "{err}"
        );
    }

    #[test]
    fn attribute_line_bodies_carry_required_and_ref_suffixes() {
        fn attribute(
            name: &str,
            kind: AttributeType,
            required: bool,
            target: &str,
        ) -> AttributeDef {
            AttributeDef {
                name: name.to_owned(),
                attr_type: kind,
                required,
                target: target.to_owned(),
            }
        }

        assert_eq!(
            format_entity_attribute(&attribute("email", AttributeType::String, false, "")),
            "email (string)"
        );
        assert_eq!(
            format_entity_attribute(&attribute("email", AttributeType::String, true, "")),
            "email (string) [required]"
        );
        assert_eq!(
            format_entity_attribute(&attribute("manager", AttributeType::Ref, false, "employee")),
            "manager (ref) -> employee"
        );
        // A ref without a target gets no arrow (oracle: `and (eq .Type "ref") .Target`).
        assert_eq!(
            format_entity_attribute(&attribute("email", AttributeType::Ref, false, "")),
            "email (ref)"
        );
    }
}
