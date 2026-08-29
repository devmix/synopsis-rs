//! Entity-linker prompt templates (llm change, design D3).
//!
//! The `llm` linking method renders a system + user prompt from a candidate
//! entity pair and sends both to the model. The templates are **data**, not
//! code: they live under `paths.prompts_path/entity-linker/{system,user}.tmpl`
//! so a user can tune the prompt without a rebuild. When a file is absent the
//! embedded default (compiled in via `include_str!`) is used instead — absence
//! is the normal case and is *not* an error. A loaded override is recorded in
//! [`EntityLinkerPrompts::notes`]: the crate has no logger, and the CLI /
//! linker surfaces the note (the `LinkResult.notes` pattern).
//!
//! Oracle mapping: `../synopsis/internal/prompts/{loader,funcmap}.go` +
//! `../synopsis/configs/prompts/entity-linker/*.tmpl` — a functional copy,
//! re-architected for Rust (migration principle: not a code copy).
//!
//! # Deliberate deviations
//!
//! - Go `text/template` → Jinja2 / [`minijinja`]. The oracle templates are
//!   re-expressed functionally (same prompt text and data shape); the field
//!   paths are `entity_a.*` / `entity_b.*` instead of `.EntityA.*` / `.EntityB.*`.
//! - The oracle's `join`/`truncate` are template **functions** (not filters);
//!   they are registered as minijinja functions with the same semantics
//!   (`join(sep, list)`; `truncate(s, max)` rune-safe with a `...` suffix and
//!   `max <= 0` → `""`).
//! - Go `range` yields an index + value; Jinja2's `for i, x in seq` instead
//!   *unpacks* each element. The oracle's `Context [{{ $i }}]:` numbering is
//!   therefore produced with a registered `enumerate` filter
//!   (`for i, chunk in context|enumerate`).
//! - The oracle's system template example JSON is missing the comma after
//!   `"confidence"`; the embedded default adds it (a valid JSON example).
//!
//! # Cache key
//!
//! The decision-cache key is the LLM **request signature** (task 1.10):
//! `sha256(model:temperature:max_tokens:rendered_system_prompt:
//! rendered_user_prompt)`, computed in `crate::linker`. The rendered prompts
//! subsume the template content, so a changed template (or a changed model /
//! sampling parameters / entity data) invalidates the cache automatically.
//! [`EntityLinkerPrompts::template_hashes`] (the SHA-256 of each template
//! **source**) is retained for template-change detection and tests.

use std::path::Path;

use minijinja::{Environment, Value};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::GraphError;

/// Embedded system-prompt default (the functional Jinja2 rewrite of the oracle
/// `configs/prompts/entity-linker/system.tmpl`).
const EMBEDDED_SYSTEM: &str = include_str!("templates/entity-linker/system.tmpl");
/// Embedded user-prompt default (the functional Jinja2 rewrite of the oracle
/// `configs/prompts/entity-linker/user.tmpl`).
const EMBEDDED_USER: &str = include_str!("templates/entity-linker/user.tmpl");

/// One entity's data as bound into the user prompt template.
///
/// Field names match the template's `entity_a.*` / `entity_b.*` paths. Task 2.2
/// fills these from the pair's entities and their chunk texts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EntityData {
    /// The entity's name.
    pub name: String,
    /// The entity's type (the template's `Type`).
    pub entity_type: String,
    /// The entity's domain (unnormalized, as stored).
    pub domain: String,
    /// The entity's description (empty when absent).
    pub description: String,
    /// Up to three context chunk texts (the oracle's `Context`).
    pub context: Vec<String>,
}

/// The render input for the user prompt: both entities of a candidate pair.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LinkerInput {
    /// The first entity (the template's `entity_a`).
    pub entity_a: EntityData,
    /// The second entity (the template's `entity_b`).
    pub entity_b: EntityData,
}

/// The SHA-256 hex digests of the two template sources (template-change
/// detection; the decision-cache key is the request signature, task 1.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateHashes {
    /// Hex sha256 of the system template source.
    pub system: String,
    /// Hex sha256 of the user template source.
    pub user: String,
}

/// Loaded entity-linker prompt templates (system + user), their source hashes,
/// and the notes recording which sources were used (design D3).
///
/// Built once per linking run by the `llm` method
/// ([`crate::linker::build_entity_links`] with `LinkMethod::Llm`): the
/// minijinja environment (with the `join`/`truncate` helpers and block
/// trimming) is created at load and the compiled templates are cached on it,
/// so rendering in the per-pair loop does not re-parse. The system prompt
/// (no data) is rendered once per run; the user prompt is rendered per pair
/// from a [`LinkerInput`]. The rendered prompts are the decision-cache key
/// inputs (task 1.10); [`EntityLinkerPrompts::template_hashes`] is retained
/// for template-change detection.
#[derive(Debug)]
pub struct EntityLinkerPrompts {
    /// The environment holding the compiled templates and the helpers.
    env: Environment<'static>,
    /// Hex sha256 of the system template source.
    system_hash: String,
    /// Hex sha256 of the user template source.
    user_hash: String,
    /// Notes recording which override files were loaded (empty when both
    /// templates fell back to the embedded defaults).
    notes: Vec<String>,
}

impl EntityLinkerPrompts {
    /// Internal name of the system template on the environment.
    const SYSTEM_NAME: &'static str = "entity-linker-system";
    /// Internal name of the user template on the environment.
    const USER_NAME: &'static str = "entity-linker-user";

    /// Render the system prompt (it takes no data).
    pub fn render_system(&self) -> Result<String, GraphError> {
        self.render(Self::SYSTEM_NAME, "system", minijinja::context! {})
    }

    /// Render the user prompt for one candidate pair.
    pub fn render_user(&self, input: &LinkerInput) -> Result<String, GraphError> {
        self.render(Self::USER_NAME, "user", input)
    }

    /// Look up a compiled template and render it with the given context.
    ///
    /// `template` is the environment's internal name; `display` is the short
    /// name (`"system"` / `"user"`) used in error messages, matching the parse
    /// errors.
    fn render<S: Serialize>(
        &self,
        template: &str,
        display: &str,
        ctx: S,
    ) -> Result<String, GraphError> {
        let template =
            self.env
                .get_template(template)
                .map_err(|source| GraphError::PromptTemplateRender {
                    name: display.to_owned(),
                    source,
                })?;
        template
            .render(ctx)
            .map_err(|source| GraphError::PromptTemplateRender {
                name: display.to_owned(),
                source,
            })
    }

    /// The template source hashes (template-change detection; the decision
    /// cache key is the request signature, task 1.10).
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

/// Load the entity-linker templates from `{prompts_path}/entity-linker/`.
///
/// A present `{system,user}.tmpl` file wins over the embedded default and is
/// recorded in the returned notes (design D3); a missing file falls back to the
/// embedded default silently (absence is the normal case). A file that is
/// present but unreadable, or a template that fails to parse, is an error.
pub fn load_entity_linker_prompts(prompts_path: &str) -> Result<EntityLinkerPrompts, GraphError> {
    let base = Path::new(prompts_path).join("entity-linker");

    let (system_source, system_note) = load_template("system", &base, EMBEDDED_SYSTEM)?;
    let (user_source, user_note) = load_template("user", &base, EMBEDDED_USER)?;

    // Hash the *sources* (design D4): a changed prompt changes the hash.
    let system_hash = sha256_hex(system_source.as_bytes());
    let user_hash = sha256_hex(user_source.as_bytes());

    let mut env = Environment::new();
    // Clean prompt output: block tags ({% for %}/{% endfor %}) on their own
    // lines contribute no stray whitespace (the oracle trims with `{{- ... -}}`).
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    // Preserve the template's final newline (Go text/template does; keeps the
    // rendered prompt byte-predictable).
    env.set_keep_trailing_newline(true);
    register_helpers(&mut env);
    // `add_template_owned` parses eagerly, so a broken override fails at load.
    env.add_template_owned(EntityLinkerPrompts::SYSTEM_NAME, system_source)
        .map_err(|source| GraphError::PromptTemplateParse {
            name: "system".to_owned(),
            source,
        })?;
    env.add_template_owned(EntityLinkerPrompts::USER_NAME, user_source)
        .map_err(|source| GraphError::PromptTemplateParse {
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

    Ok(EntityLinkerPrompts {
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
) -> Result<(String, Option<String>), GraphError> {
    let path = base.join(format!("{name}.tmpl"));
    match std::fs::read_to_string(&path) {
        Ok(source) => Ok((
            source,
            Some(format!("prompt {name}: using override {}", path.display())),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((embedded.to_owned(), None)),
        Err(err) => Err(GraphError::PromptTemplateIo {
            path: path.display().to_string(),
            source: err,
        }),
    }
}

/// Register the template helpers used by the prompts.
///
/// `join`/`truncate` are minijinja **functions** matching the oracle's
/// funcmap.go signatures: `join(sep, list)` joins a list of strings;
/// `truncate(s, max)` shortens a string rune-safely with a `...` suffix.
///
/// `enumerate` is a **filter** added because Jinja2 has no Go-style `range`
/// index: the user template's `Context [{{ i }}]:` numbering is produced with
/// `for i, chunk in context|enumerate`.
fn register_helpers(env: &mut Environment<'static>) {
    env.add_function("join", |sep: String, items: Vec<String>| -> Value {
        Value::from(items.join(&sep))
    });
    env.add_function("truncate", |text: String, max: i64| -> Value {
        Value::from(truncate(&text, max))
    });
    env.add_filter("enumerate", |items: Vec<Value>| -> Value {
        let pairs: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(i, item)| Value::from_object(vec![Value::from(i as i64), item.clone()]))
            .collect();
        Value::from_object(pairs)
    });
}

/// The oracle's `utils.Truncate`: shorten `s` to at most `max` chars, appending
/// `...` when truncated. `max <= 0` yields `""`. Rune-safe (never splits a
/// multi-byte character).
///
/// Also used by the `llm` linking method (task 2.2) to bound the prompt's
/// description and context-chunk lengths before rendering.
pub(crate) fn truncate(s: &str, max: i64) -> String {
    if max <= 0 {
        return String::new();
    }
    let max = max as usize;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_owned();
    }
    let truncated: String = chars.into_iter().take(max).collect();
    format!("{truncated}...")
}

/// Hex SHA-256 of a byte slice (the template-source hash for the D4 key).
///
/// Also used by the `llm` linking method (task 2.2) for the decision-cache
/// key digest.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
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

    use super::*;
    use std::path::PathBuf;

    /// A temp dir unique to one test (the repo's established pattern:
    /// `temp_dir()` + `process::id()` + a per-test name suffix).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graph-prompts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A sample render input with two distinct entities and context chunks.
    fn sample_input() -> LinkerInput {
        LinkerInput {
            entity_a: EntityData {
                name: "Acme Corp".to_owned(),
                entity_type: "ORGANIZATION".to_owned(),
                domain: "hr".to_owned(),
                description: "A software company".to_owned(),
                context: vec![
                    "Acme hired 100 engineers".to_owned(),
                    "Acme is in Berlin".to_owned(),
                ],
            },
            entity_b: EntityData {
                name: "Acme".to_owned(),
                entity_type: "ORGANIZATION".to_owned(),
                domain: "it".to_owned(),
                description: String::new(),
                context: vec!["the acme server".to_owned()],
            },
        }
    }

    #[test]
    fn embedded_fallback_renders_and_has_no_notes() {
        // A path that does not exist: both templates fall back to embedded.
        let prompts = load_entity_linker_prompts("/nonexistent/prompts-path").unwrap();

        let system = prompts.render_system().unwrap();
        assert!(system.contains("entity resolution assistant"));
        assert!(system.contains("\"same_entity\""));

        let user = prompts.render_user(&sample_input()).unwrap();
        assert!(user.contains("- Name: Acme Corp"));
        assert!(user.contains("- Type: ORGANIZATION"));
        assert!(user.contains("- Domain: hr"));
        assert!(user.contains("- Description: A software company"));
        assert!(user.contains("- Context [0]: Acme hired 100 engineers"));
        assert!(user.contains("- Context [1]: Acme is in Berlin"));
        assert!(user.contains("- Name: Acme"));
        // entity_b has an empty description and one context chunk.
        assert!(user.contains("- Description: "));
        assert!(user.contains("- Context [0]: the acme server"));
        assert!(user.contains("Respond with a JSON object"));

        // No override was loaded: no notes.
        assert!(prompts.notes().is_empty());
    }

    #[test]
    fn override_files_win_and_are_noted() {
        let dir = temp_dir("override-both");
        std::fs::create_dir_all(dir.join("entity-linker")).unwrap();
        std::fs::write(
            dir.join("entity-linker").join("system.tmpl"),
            "SYS OVERRIDE\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("entity-linker").join("user.tmpl"),
            "USER OVERRIDE\n",
        )
        .unwrap();

        let prompts = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap();

        assert_eq!(prompts.render_system().unwrap(), "SYS OVERRIDE\n");
        assert_eq!(
            prompts.render_user(&sample_input()).unwrap(),
            "USER OVERRIDE\n"
        );

        let notes = prompts.notes();
        assert_eq!(notes.len(), 2, "both overrides recorded");
        assert!(notes[0].contains("prompt system"));
        assert!(notes[0].ends_with("system.tmpl"));
        assert!(notes[1].contains("prompt user"));
        assert!(notes[1].ends_with("user.tmpl"));
    }

    #[test]
    fn partial_override_falls_back_for_the_other_template() {
        let dir = temp_dir("override-system-only");
        std::fs::create_dir_all(dir.join("entity-linker")).unwrap();
        std::fs::write(
            dir.join("entity-linker").join("system.tmpl"),
            "SYS OVERRIDE\n",
        )
        .unwrap();

        let prompts = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap();

        // The system override wins; the user template falls back to embedded.
        assert_eq!(prompts.render_system().unwrap(), "SYS OVERRIDE\n");
        assert!(
            prompts
                .render_user(&sample_input())
                .unwrap()
                .contains("Entity A")
        );

        // Only the system override is noted.
        let notes = prompts.notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("prompt system"));
    }

    #[test]
    fn missing_override_directory_is_not_an_error() {
        // The prompts_path exists but has no entity-linker subdir at all.
        let dir = temp_dir("no-entity-linker-dir");
        let prompts = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap();
        assert!(
            prompts
                .render_system()
                .unwrap()
                .contains("entity resolution assistant")
        );
        assert!(prompts.notes().is_empty());
    }

    #[test]
    fn join_and_truncate_helpers_render() {
        let mut env = Environment::new();
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        register_helpers(&mut env);
        let out = env
            .render_str(
                "{{ join(' | ', parts) }} / {{ truncate(long, 5) }}",
                minijinja::context! {
                    parts => vec!["a", "b", "c"],
                    long => "hello world",
                },
            )
            .unwrap();
        assert_eq!(out, "a | b | c / hello...");
    }

    #[test]
    fn truncate_is_rune_safe_and_edge_cases() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hello...");
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("hello", -3), "");
        // Multi-byte: never splits a character.
        assert_eq!(truncate("héllo", 4), "héll...");
        assert_eq!(truncate("héllo", 5), "héllo");
    }

    #[test]
    fn hashes_are_stable_and_match_the_source() {
        let a = load_entity_linker_prompts("/nonexistent/prompts-path").unwrap();
        let b = load_entity_linker_prompts("/nonexistent/prompts-path").unwrap();

        let ha = a.template_hashes();
        let hb = b.template_hashes();
        assert_eq!(ha, hb, "hashes must be stable across loads");

        // The hash is the sha256 of the embedded source (not rendered data).
        assert_eq!(ha.system, sha256_hex(EMBEDDED_SYSTEM.as_bytes()));
        assert_eq!(ha.user, sha256_hex(EMBEDDED_USER.as_bytes()));
        // A 64-char lowercase hex digest.
        assert_eq!(ha.system.len(), 64);
        assert!(ha.system.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_changes_when_the_template_changes() {
        let baseline = load_entity_linker_prompts("/nonexistent/prompts-path").unwrap();
        let base_hash = baseline.template_hashes();

        let dir = temp_dir("hash-change");
        std::fs::create_dir_all(dir.join("entity-linker")).unwrap();
        std::fs::write(dir.join("entity-linker").join("system.tmpl"), "DIFFERENT\n").unwrap();
        // user.tmpl absent: it keeps the embedded source (and hash).
        let changed = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap();
        let changed_hash = changed.template_hashes();

        assert_ne!(
            changed_hash.system, base_hash.system,
            "a changed system template must change the system hash"
        );
        assert_eq!(
            changed_hash.user, base_hash.user,
            "the untouched user template keeps its hash"
        );
        assert_eq!(
            changed_hash.system,
            sha256_hex("DIFFERENT\n".as_bytes()),
            "the hash is of the override source"
        );
    }

    #[test]
    fn broken_override_template_fails_to_parse_at_load() {
        let dir = temp_dir("broken-user");
        std::fs::create_dir_all(dir.join("entity-linker")).unwrap();
        std::fs::write(
            dir.join("entity-linker").join("user.tmpl"),
            "{% for x in items %}unclosed",
        )
        .unwrap();

        let err = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap_err();
        assert!(matches!(err, GraphError::PromptTemplateParse { ref name, .. } if name == "user"));
    }

    #[test]
    fn render_error_is_mapped() {
        // A valid template that passes a non-integer to `truncate`'s `max`
        // arg fails at render time (not load time): the `i64` coercion fails.
        let dir = temp_dir("render-error");
        std::fs::create_dir_all(dir.join("entity-linker")).unwrap();
        std::fs::write(
            dir.join("entity-linker").join("system.tmpl"),
            "{{ truncate('x', 'notanint') }}",
        )
        .unwrap();

        let prompts = load_entity_linker_prompts(&dir.to_string_lossy()).unwrap();
        let err = prompts.render_system().unwrap_err();
        assert!(
            matches!(err, GraphError::PromptTemplateRender { ref name, .. } if name == "system")
        );
    }
}
