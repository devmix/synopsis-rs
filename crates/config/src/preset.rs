//! Main YAML preset: typed [`Config`] structures plus load / validate / defaults.
//!
//! This module mirrors the Go oracle's `internal/config/config.go`: YAML
//! structures, `Load` and `Validate`, and `ApplyDefaults` with the derived
//! helpers `vector_dim` / `cache_db_path`. The knowledge-DB path is NOT a
//! config field: it is derived from `workspace_dir` + `dataset.name` via
//! [`DatasetConfig::db_path`]. Field names, YAML keys
//! and validation semantics stay faithful to the oracle; defaulting follows
//! design D12/D13 (revision of task 1.2):
//!
//! * **YAML artifacts are normalized at deserialization** ("parse, don't
//!   validate"): an absent key on a defaulted field yields that field's oracle
//!   default, and an explicit `""` on the five tolerant enums / eight string
//!   fields normalizes to the same default. After [`load`](crate::load) those
//!   values are always valid — no runtime code ever sees an empty artifact.
//! * **Semantic rules live in [`Config::apply_defaults`]**: numeric `<= 0`
//!   fallbacks (`overlap_size`: `< 0`, so a configured `0` survives), conditional
//!   pairs (both search legs off → both on; no local model set → `"bge-m3-int8"`),
//!   maps/lists (`text_fields`, `authority_boost`, scheduler jobs) and the
//!   `auto_update:` section-presence rule (design D8).
//! * **Go's buggy bool defaults are fixed** (D13, BREAKING): `enable_graph`,
//!   `load_on_startup` and `watch_sources` use presence semantics — absent →
//!   true, an explicit `false` is respected.
//! * **`vectors:` is an additive extension with no oracle counterpart** (design
//!   D7, human decision 2026-08-21): the oracle's vec0 brute-force had no ANN
//!   parameters to tune, so the section stores raw fields with ADR 0003
//!   defaults and Go's ignore-unknown-keys behavior keeps old presets
//!   compatible. An absent section stays `None` (and is skipped on
//!   re-serialization); [`Config::vectors_config`] resolves it to the defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

// ── Tolerant-enum machinery (defined before first use) ────────────────────

/// Declares a tolerant string enum (design D7): known values map to named
/// variants, any other value is captured verbatim in `Unknown(String)` instead
/// of failing the parse. Serializes back to its canonical lowercase word (or the
/// original text for unknowns), so round-tripping preserves intent. The second
/// token names the variant used by [`Default`], chosen as that field's oracle
/// default so an absent YAML key deserializes straight to the intended value.
///
/// A trailing clause selects how an explicit empty string is handled (design D12):
/// * `empty_to_default` — `""` normalizes to the Default variant at parse time;
///   used by every enum whose field has an unconditional oracle default (`strategy`,
///   `level`, `format`, `output`, `response_format`).
/// * *(clause absent)* — `""` stays `Unknown("")`; used by strict enums without an
///   oracle default ([`EmbeddingsMode`]): an empty mode must still be rejected by
///   [`Config::validate`] with the oracle's message, like any other bogus value.
macro_rules! tolerant_enum {
    ($(#[$doc:meta])* $name:ident, $default:ident { $($variant:ident => $value:expr),* $(,)? } empty_to_default) => {
        __tolerant_enum_impl__!($(#[$doc])* $name, $default { $($variant => $value),* }, Self::$default);
    };
    ($(#[$doc:meta])* $name:ident, $default:ident { $($variant:ident => $value:expr),* $(,)? }) => {
        __tolerant_enum_impl__!($(#[$doc])* $name, $default { $($variant => $value),* }, Self::Unknown(String::new()));
    };
}

// Shared body of [`tolerant_enum`]; `$empty_fallback` is the expression produced for an
// explicit empty string (design D12): `Self::$default` for enums that normalize to their
// oracle default, or `Self::Unknown(String::new())` for strict enums without one. It must
// not reference local variables — tokens passed through fragment slots keep their
// definition-site span and cannot see locals of the generated function (macro hygiene).
macro_rules! __tolerant_enum_impl__ {
    ($(#[$doc:meta])* $name:ident, $default:ident { $($variant:ident => $value:expr),* }, $empty_fallback:expr) => {
        // Caller-supplied doc lines (descriptive); the fixed line below guarantees a
        // doc is always present so `missing_docs` holds even if none are provided.
        $(#[$doc])*
        /// Unrecognized values are kept in `$crate::preset::$name::Unknown`.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $(
                /// Recognized value.
                $variant,
            )*
            /// A value the schema does not recognize (kept verbatim).
            Unknown(String),
        }

        impl Default for $name {
            fn default() -> Self {
                // The zero value equals this field's oracle default, so an absent
                // YAML key deserializes straight to the intended value.
                Self::$default
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                match self {
                    $(Self::$variant => serializer.serialize_str($value),)*
                    Self::Unknown(s) => serializer.serialize_str(s),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                // Case-insensitive match on known words (oracle uses lowercase); unknown
                // values are preserved verbatim for diagnostics. An explicit "" is a YAML
                // artifact of an unset field: `$empty_fallback` normalizes it to the
                // oracle default (design D12) or keeps it as Unknown("") for strict enums.
                Ok(match raw.to_ascii_lowercase().as_str() {
                    $($value => Self::$variant,)*
                    "" => $empty_fallback,
                    other => Self::Unknown(other.to_string()),
                })
            }
        }
    };
}

/// Serde default for presence-semantics bool fields (design D13): an absent YAML
/// key means `true`, while an explicit `false` is deserialized and respected.
fn default_true() -> bool {
    true
}

/// Declares the serde helper pair for a config string field with an unconditional
/// oracle default (design D12): `default_*()` supplies the value when the YAML key is
/// absent, and `de_*()` normalizes an explicit empty string to it. serde's
/// `deserialize_with` accepts only a zero-argument path, so each field gets its own
/// pair even though the logic is identical (the D12 "de_empty_to_default" pattern).
macro_rules! empty_string_default {
    ($default_fn:ident, $de_fn:ident, $value:expr) => {
        /// Oracle default for a config string field (design D12).
        fn $default_fn() -> String {
            $value.to_string()
        }

        /// Deserialize a config string; an explicit `""` becomes the field's oracle
        /// default so runtime code never sees it (design D12, "parse, don't validate").
        fn $de_fn<'de, D>(deserializer: D) -> Result<String, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer).map(|value| {
                if value.is_empty() {
                    $default_fn()
                } else {
                    value
                }
            })
        }
    };
}

// The string fields with unconditional defaults (design D12 list, revised by
// storage-layout-restructure D1 + revision 1.1): four paths + three server
// identification fields. `server_*` helpers are prefixed to keep the names
// unambiguous at module level. The dataset name has default `""` (no dataset)
// and the database path no longer exists (derived, not configurable), so
// neither needs a D12 helper.
empty_string_default!(default_workspace_dir, de_workspace_dir, "workspace");
empty_string_default!(default_migrations_dir, de_migrations_dir, "migrations");
empty_string_default!(
    default_prompts_path,
    de_prompts_path,
    "workspace/configs/prompts"
);
empty_string_default!(
    default_onnx_config,
    de_onnx_config,
    "workspace/configs/onnx.yaml"
);
empty_string_default!(default_server_name, de_server_name, "synopsis");
empty_string_default!(default_server_version, de_server_version, "0.1.0-dev");
empty_string_default!(default_server_host, de_server_host, "0.0.0.0");

// ── Root ──────────────────────────────────────────────────────────────────

/// Root of the application configuration (top-level YAML document).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// SQLite storage settings.
    pub database: DatabaseConfig,
    /// Embedding provider selection and per-provider settings.
    pub embeddings: EmbeddingsConfig,
    /// Document ingestion pipeline (chunking, NER, batching, resolution).
    pub ingestion: IngestionConfig,
    /// Filesystem paths used across the application.
    pub paths: PathsConfig,
    /// Active dataset: per-dataset artifacts (ontology, content, state,
    /// knowledge DB, vector index) resolve under
    /// `<workspace_dir>/datasets/<name>/` (storage-layout-restructure D1).
    pub dataset: DatasetConfig,
    /// MCP server identification and bind settings.
    pub server: ServerConfig,
    /// Hybrid (lexical + semantic) search tuning.
    pub search: SearchConfig,
    /// Knowledge-graph module toggle and traversal bounds.
    pub graph: GraphConfig,
    /// Automatic re-indexing on file change; `None` means the section was
    /// absent (design D8 — presence is carried by the option itself).
    pub auto_update: Option<AutoUpdateConfig>,
    /// Universal job scheduler.
    pub scheduler: SchedulerConfig,
    /// Logging level / format / output.
    pub logging: LoggingConfig,
    /// Cross-domain entity linker.
    pub linker: LinkerConfig,
    /// ANN index tuning for the vector search leg (design D7 — additive
    /// extension of the frozen config format, human decision 2026-08-21).
    /// `None` means the section was absent from the YAML; an absent section
    /// resolves to the ADR 0003 defaults via
    /// [`Config::vectors_config`](Config::vectors_config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<VectorsConfig>,
}

impl Config {
    /// Validates the required-fields invariants of a fully-loaded config.
    ///
    /// Mirrors `config.go Validate`: exactly one embeddings mode is legal and
    /// each mode has its own set of mandatory fields. Returns [`ConfigError::Validation`]
    /// on the first violated invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Oracle parity (config.go `Validate` switch): the mode itself is checked
        // here, not at parse time. An unrecognized value yields a Validation error
        // with the oracle's exact message rather than a YAML/parse failure.
        match &self.embeddings.mode {
            EmbeddingsMode::Local => {
                let local = &self.embeddings.local;
                if local.model_path.is_empty() && local.model_name.is_empty() {
                    return Err(validation(
                        "embeddings.local.model_path or model_name is required in local mode",
                    ));
                }
                if local.vector_dim <= 0 {
                    return Err(validation("embeddings.local.vector_dim must be positive"));
                }
            }
            EmbeddingsMode::Api => {
                let api = &self.embeddings.api;
                if api.base_url.is_empty() {
                    return Err(validation(
                        "embeddings.api.base_url is required in api mode",
                    ));
                }
                if api.model_name.is_empty() {
                    return Err(validation(
                        "embeddings.api.model_name is required in api mode",
                    ));
                }
                if api.vector_dim <= 0 {
                    return Err(validation("embeddings.api.vector_dim must be positive"));
                }
            }
            EmbeddingsMode::Unknown(mode) => {
                return Err(validation(&format!(
                    "unknown embeddings mode \"{mode}\", want \"local\" or \"api\""
                )));
            }
        }
        Ok(())
    }

    /// Fills zero-value fields with the oracle's defaults — the **semantic** half of
    /// the defaulting split (design D12). YAML artifacts are already normalized by
    /// deserialization: absent keys and explicit `""` on defaulted string/enum fields,
    /// plus presence-semantics bools (`enable_graph`, `load_on_startup`,
    /// `watch_sources`, design D13) never reach this method empty or false-by-default.
    /// What remains here are the oracle's semantic rules:
    ///
    /// * Numeric `<= 0` fallbacks — except `markdown.overlap_size`, which Go checks
    ///   with `< 0`, so a configured `0` survives.
    /// * Conditional pairs: both search legs force-enabled only when **both** are
    ///   disabled (an explicit single-leg disable is respected); and the local model
    ///   fallback — `embeddings.local.model_name` becomes `"bge-m3-int8"` in **both**
    ///   modes, exactly as Go applies it regardless of `mode`.
    /// * Maps / lists: an empty `json.text_fields` gains the oracle's four fields; an
    ///   empty `authority_boost` map gains `"default": 1.0`; the scheduler always
    ///   gains an `orphan_cleanup` job — **disabled** with a 3600 s interval unless
    ///   explicitly configured (Go defaults it disabled, not enabled: the job performs
    ///   full-table scans).
    /// * Section presence: an `auto_update:` section missing from YAML is materialized
    ///   fully enabled (`enabled = initial_sync = true`); a present one is respected as
    ///   parsed. Presence rides on the [`Option`] itself (design D8) instead of Go's
    ///   hidden `autoUpdateConfigured` flag that scans YAML nodes.
    pub fn apply_defaults(&mut self) {
        // Ingestion ------------------------------------------------------------------
        if self.ingestion.batch_size <= 0 {
            self.ingestion.batch_size = 100;
        }
        let md = &mut self.ingestion.chunking.markdown;
        if md.max_chunk_size <= 0 {
            md.max_chunk_size = 1000;
        }
        // Go checks `< 0`, not `<= 0`: a configured overlap of 0 is preserved.
        if md.overlap_size < 0 {
            md.overlap_size = 100;
        }
        if md.min_section_size <= 0 {
            md.min_section_size = 500;
        }
        if self.ingestion.chunking.json.text_fields.is_empty() {
            self.ingestion.chunking.json.text_fields = vec![
                "description".to_string(),
                "title".to_string(),
                "wikitext".to_string(),
                "html".to_string(),
            ];
        }

        // Search -----------------------------------------------------------------------
        let s = &mut self.search;
        if s.rrf_k <= 0 {
            s.rrf_k = 20; // Calibrated k: lower value increases rank sensitivity (~8x vs k=60).
        }
        if s.lexical_top_k <= 0 {
            s.lexical_top_k = 20;
        }
        if s.semantic_top_k <= 0 {
            s.semantic_top_k = 20;
        }
        if s.final_top_k <= 0 {
            s.final_top_k = 10;
        }
        // Force-enable both legs only when neither is requested at all.
        if !s.enable_lexical && !s.enable_semantic {
            s.enable_lexical = true;
            s.enable_semantic = true;
        }
        if s.timeout_ms <= 0 {
            s.timeout_ms = 10_000;
        }

        // Graph --------------------------------------------------------------------------
        // `enable_graph` / `load_on_startup` are normalized at deserialization time
        // (design D13 presence semantics) — nothing to do here.
        if self.graph.max_depth <= 0 {
            self.graph.max_depth = 5;
        }
        if self.graph.max_nodes <= 0 {
            self.graph.max_nodes = 1000;
        }

        // Auto-update ----------------------------------------------------------------------
        let auto_update_configured = self.auto_update.is_some();
        let auto = self
            .auto_update
            .get_or_insert_with(AutoUpdateConfig::default);
        if auto.debounce_seconds <= 0 {
            auto.debounce_seconds = 30;
        }
        // `watch_sources` is normalized at deserialization time (design D13).
        // Absent section defaults to fully enabled; a present one is respected as
        // parsed (Go: the hidden `autoUpdateConfigured` flag skips this branch).
        if !auto_update_configured {
            auto.enabled = true;
            auto.initial_sync = true;
        }

        // Scheduler ------------------------------------------------------------------------
        // An entry missing from YAML decodes to the zero value, so `entry` + default
        // reproduces Go's nil-map handling. The job is disabled by default (full-table
        // scans on large databases); a zero-value entry already has `enabled == false`,
        // so only the interval needs its default and any explicit configuration wins.
        let orphan_cleanup = self
            .scheduler
            .jobs
            .entry("orphan_cleanup".to_string())
            .or_default();
        if orphan_cleanup.interval_seconds <= 0 {
            orphan_cleanup.interval_seconds = 3600;
        }

        // Logging, paths and dataset name are normalized at deserialization
        // time (design D12) — nothing to do here.

        // NER LLM (Go applies these to the NER provider only; `linker.llm` is untouched) ----
        let llm = &mut self.ingestion.ner.llm;
        if llm.timeout_ms <= 0 {
            llm.timeout_ms = 60_000;
        }
        if llm.max_retries <= 0 {
            llm.max_retries = 3;
        }

        // Resolver --------------------------------------------------------------------------------
        if self.ingestion.resolver.similarity_threshold <= 0.0 {
            self.ingestion.resolver.similarity_threshold = 0.8;
        }

        // Local embedding (Go applies this fallback in both modes) ---------------------------------
        let local = &mut self.embeddings.local;
        if local.model_name.is_empty() && local.model_path.is_empty() {
            local.model_name = "bge-m3-int8".to_string();
        }

        // Server (name/version/host are normalized at deserialization time, D12) ------
        if self.server.port <= 0 {
            self.server.port = 8080;
        }

        // Reranker ------------------------------------------------------------------------------------
        if self.search.deprecated_boost <= 0.0 {
            self.search.deprecated_boost = 0.2;
        }
        if self.search.official_boost <= 0.0 {
            self.search.official_boost = 1.5;
        }
        if self.search.recent_boost <= 0.0 {
            self.search.recent_boost = 1.2;
        }
        if self.search.recent_days <= 0 {
            self.search.recent_days = 90;
        }
        // Go checks `nil`; an explicit empty map is indistinguishable from an absent one here,
        // so `is_empty` is the closest faithful check.
        if self.search.authority_boost.is_empty() {
            self.search
                .authority_boost
                .insert("default".to_string(), 1.0);
        }
    }

    /// Returns the configured embedding vector dimension for the active mode
    /// (Go `VectorDim`): local → `local.vector_dim`, api → `api.vector_dim`,
    /// any unrecognized mode → 0.
    pub fn vector_dim(&self) -> i32 {
        match self.embeddings.mode {
            EmbeddingsMode::Local => self.embeddings.local.vector_dim,
            EmbeddingsMode::Api => self.embeddings.api.vector_dim,
            EmbeddingsMode::Unknown(_) => 0,
        }
    }

    /// Returns the effective ANN index configuration (design D7): the parsed
    /// `vectors:` section when present, otherwise the ADR 0003 defaults
    /// ([`VectorsConfig::default`]). The mapping to `vectors::VectorIndexConfig`
    /// happens at the wiring level (a future change); field names and types
    /// already mirror it, so the mapping is a field-by-field copy.
    pub fn vectors_config(&self) -> VectorsConfig {
        self.vectors.unwrap_or_default()
    }

    /// Returns the global cache database path
    /// ([`PathsConfig::cache_db_path`]): `<workspace_dir>/db/cache/cache.db`.
    /// The cache is global (LLM linker/NER + embeddings), not per-dataset
    /// (storage-layout-restructure D1).
    pub fn cache_db_path(&self) -> PathBuf {
        self.paths.cache_db_path()
    }
}

/// Builds a [`ConfigError::Validation`] from a message.
fn validation(message: &str) -> ConfigError {
    ConfigError::Validation {
        message: message.to_string(),
    }
}

// ── Database ──────────────────────────────────────────────────────────────

/// SQLite storage settings.
///
/// The knowledge-DB path is intentionally NOT a field here (revision 1.1):
/// it is derived from `workspace_dir` + `dataset.name` via
/// [`DatasetConfig::db_path`] and cannot be overridden in a preset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Custom PRAGMA overrides keyed by pragma name.
    pub pragma: HashMap<String, String>,
}

// ── Embeddings ────────────────────────────────────────────────────────────

tolerant_enum! {
    /// Embedding provider mode (design D7): `"local"` or `"api"`. It deserializes
    /// leniently so an unrecognized value does NOT fail parsing — the oracle keeps
    /// `mode` as a plain string and checks it in `Validate()`, so a config that loads
    /// there must load here too. [`Config::validate`] then rejects any non-`local`/
    /// non-`api` value with the oracle's message, which is what makes this "strict"
    /// (invalid values error at validation) versus the purely tolerant enums that are
    /// preserved without ever failing. `mode` has no oracle default, so it does NOT
    /// opt into D12 empty-string normalization: an explicit `""` stays `Unknown("")`
    /// and is rejected by [`Config::validate`] exactly like Go rejects it.
    EmbeddingsMode, Local {
        Local => "local",
        Api   => "api",
    }
}

/// Embedding provider selection plus per-provider settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingsConfig {
    /// Which provider to use (`"local"` or `"api"`).
    pub mode: EmbeddingsMode,
    /// Local ONNX provider settings (used when `mode == local`).
    pub local: LocalEmbedding,
    /// Remote API provider settings (used when `mode == api`).
    pub api: ApiEmbedding,
    /// Auto-recreate the vector index on dimension mismatch.
    pub auto_rebuild_vectors: bool,
}

/// Settings for the local ONNX embedding provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalEmbedding {
    /// Model name from the registry (e.g. `"bge-m3-int8"`).
    pub model_name: String,
    /// Explicit path that overrides `model_name` resolution.
    pub model_path: String,
    /// Optional tokenizer path override.
    pub tokenizer_path: String,
    /// Embedding vector dimension; must be positive in local mode.
    pub vector_dim: i32,
}

/// Settings for the remote API embedding provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiEmbedding {
    /// Base URL of the embeddings endpoint.
    pub base_url: String,
    /// Bearer token; empty if not required.
    pub api_key: String,
    /// Model name to request from the API.
    pub model_name: String,
    /// Embedding vector dimension; must be positive in api mode.
    pub vector_dim: i32,
    /// Max retry attempts after an initial failure.
    pub max_retries: i32,
    /// HTTP request timeout in milliseconds.
    pub timeout_ms: i64,
}

// ── Ingestion ─────────────────────────────────────────────────────────────

/// Document ingestion pipeline settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestionConfig {
    /// Per-format chunking configuration.
    pub chunking: ChunkingConfig,
    /// Named-entity-recognition (NER) configuration.
    pub ner: NerConfig,
    /// Batch size for embedding generation.
    pub batch_size: i32,
    /// Entity-resolver settings.
    pub resolver: ResolverConfig,
}

/// Per-format chunker settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkingConfig {
    /// Markdown chunker.
    pub markdown: MarkdownChunkerConfig,
    /// JSON chunker (YAML key `json`).
    #[serde(default, rename = "json")]
    pub json: JsonChunkerConfig,
}

tolerant_enum! {
    /// Chunking strategy for the markdown chunker (design D7: tolerant; an explicit
    /// `""` normalizes to [`ChunkingStrategy::Headers`] at parse time, design D12).
    ChunkingStrategy, Headers {
        Headers => "headers",
        Fixed   => "fixed",
        Hybrid  => "hybrid",
    } empty_to_default
}

/// Markdown chunker settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownChunkerConfig {
    /// Strategy to use (`"headers"`, `"fixed"` or `"hybrid"`; tolerant).
    pub strategy: ChunkingStrategy,
    /// Maximum characters per chunk.
    pub max_chunk_size: i32,
    /// Overlap between consecutive chunks in characters.
    pub overlap_size: i32,
    /// Minimum section size before it is split.
    pub min_section_size: i32,
}

/// JSON chunker settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JsonChunkerConfig {
    /// Fields to index as text.
    pub text_fields: Vec<String>,
    /// Merge all indexed fields into a single chunk per object.
    pub combine_fields: bool,
    /// Limit on the number of objects processed (0 = unlimited).
    pub max_objects: i32,
}

/// NER configuration (prose and/or LLM providers).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NerConfig {
    /// When true no NER runs at all.
    pub disabled: bool,
    /// Rule / POS-based prose NER settings.
    pub prose: ProseNerConfig,
    /// LLM-based NER provider settings.
    pub llm: LlmConfig,
}

/// Settings for the prose (POS + regex) NER provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProseNerConfig {
    /// Enable part-of-speech tagging.
    pub enable_pos: bool,
    /// Enable named-entity recognition.
    pub enable_ner: bool,
    /// User-supplied regex patterns for custom entities.
    pub custom_patterns: Vec<String>,
    /// Entity types to keep (filter); empty = all.
    pub entity_types: Vec<String>,
    /// Minimum confidence for any entity.
    pub min_confidence: f64,
    /// Higher threshold applied to LOCATION entities.
    pub location_min_confidence: f64,
}

/// LLM provider settings (shared by NER and the linker).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// OpenAI-compatible API base URL.
    pub api_base_url: String,
    /// Bearer token; empty if not required.
    pub api_key: String,
    /// Model identifier to call.
    pub model_name: String,
    /// Sampling temperature (0.0 for determinism).
    pub temperature: f64,
    /// Maximum tokens per response.
    pub max_tokens: i32,
    /// Random seed for deterministic sampling.
    pub seed: i64,
    /// Structured-output mode (`"json_object"` or `"json_schema"`; tolerant).
    pub response_format: ResponseFormat,
    /// HTTP request timeout in milliseconds.
    pub timeout_ms: i64,
    /// Max retry attempts after an initial failure.
    pub max_retries: i32,
}

tolerant_enum! {
    /// LLM structured-output format (design D7: tolerant; an explicit `""` normalizes
    /// to [`ResponseFormat::JsonObject`] at parse time, design D12).
    ResponseFormat, JsonObject {
        JsonObject => "json_object",
        JsonSchema => "json_schema",
    } empty_to_default
}

/// Entity resolution / deduplication thresholds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolverConfig {
    /// Jaro-Winkler similarity threshold for merging entities.
    pub similarity_threshold: f64,
}

// ── Linker ────────────────────────────────────────────────────────────────

/// Cross-domain entity linker settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkerConfig {
    /// When true the LLM cross-domain linker is off.
    pub disabled: bool,
    /// LLM provider used for linking.
    pub llm: LlmConfig,
}

// ── Search ────────────────────────────────────────────────────────────────

/// Hybrid search tuning (reciprocal-rank fusion of lexical + semantic).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// RRF constant `k`.
    pub rrf_k: i32,
    /// Top-K for the lexical (FTS5) leg.
    pub lexical_top_k: i32,
    /// Top-K for the semantic (vector) leg.
    pub semantic_top_k: i32,
    /// Final merged top-K returned to callers.
    pub final_top_k: i32,
    /// Enable the lexical search leg.
    pub enable_lexical: bool,
    /// Enable the semantic search leg.
    pub enable_semantic: bool,
    /// Per-search timeout in milliseconds.
    pub timeout_ms: i64,
    /// Boost penalty for deprecated content.
    pub deprecated_boost: f64,
    /// Boost for official sources.
    pub official_boost: f64,
    /// Boost for recent content.
    pub recent_boost: f64,
    /// Recency window in days for `recent_boost`.
    pub recent_days: i32,
    /// Authority-based boost factors keyed by authority name.
    pub authority_boost: HashMap<String, f64>,
}

// ── Vectors (ANN index) ───────────────────────────────────────────────────
//
// Additive extension of the frozen config format (design D7, human decision
// 2026-08-21): the oracle has no ANN parameters (vec0 brute-force had nothing
// to tune) and Go ignores unknown keys, so presets without the section stay
// compatible. Defaults are the ADR 0003 configuration.

/// Declares the serde default helper for a `vectors:` section field (design
/// D7): a key missing inside a present section resolves to the ADR 0003 value,
/// the same as an absent section. The generated function is the single source
/// of truth for [`VectorsConfig::default()`].
macro_rules! vectors_adr_default {
    ($name:ident, $value:literal) => {
        /// ADR 0003 default for the matching `vectors:` section field (design D7).
        fn $name() -> usize {
            $value
        }
    };
}

vectors_adr_default!(default_vectors_dim, 1024);
vectors_adr_default!(default_vectors_m, 16);
vectors_adr_default!(default_vectors_ef_construction, 100);
vectors_adr_default!(default_vectors_num_partitions, 256);
vectors_adr_default!(default_vectors_nprobes, 32);
vectors_adr_default!(default_vectors_ef_search, 200);

/// ANN index and query parameters for the vector search leg (design D7).
///
/// Raw preset fields only: this crate does not depend on `vectors`
/// (dependency direction, design D7), so the mapping to
/// `vectors::VectorIndexConfig` happens at the wiring level (a future
/// change). Field names and types mirror `VectorIndexConfig` so the mapping
/// is a field-by-field copy.
///
/// Defaults are the ADR 0003 configuration; a key missing inside a present
/// section resolves to the same value as an absent section
/// ([`Config::vectors_config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VectorsConfig {
    /// Vector dimensionality (bge-m3: 1024).
    #[serde(default = "default_vectors_dim")]
    pub dim: usize,
    /// HNSW graph degree M.
    #[serde(default = "default_vectors_m")]
    pub m: usize,
    /// HNSW efConstruction: candidate list size during index build.
    #[serde(default = "default_vectors_ef_construction")]
    pub ef_construction: usize,
    /// Number of IVF partitions (coarse-quantizer centroids).
    #[serde(default = "default_vectors_num_partitions")]
    pub num_partitions: usize,
    /// IVF partitions probed per query; runtime-tunable (ADR 0003 mitigation #1).
    #[serde(default = "default_vectors_nprobes")]
    pub nprobes: usize,
    /// HNSW efSearch: candidate list size during query; runtime-tunable.
    #[serde(default = "default_vectors_ef_search")]
    pub ef_search: usize,
}

impl Default for VectorsConfig {
    /// ADR 0003 configuration: 1024-dim, M=16, efConstruction=100, 256
    /// partitions, nprobes=32, efSearch=200 — the per-field serde defaults.
    fn default() -> Self {
        Self {
            dim: default_vectors_dim(),
            m: default_vectors_m(),
            ef_construction: default_vectors_ef_construction(),
            num_partitions: default_vectors_num_partitions(),
            nprobes: default_vectors_nprobes(),
            ef_search: default_vectors_ef_search(),
        }
    }
}

// ── Graph / storage ───────────────────────────────────────────────────────

/// Knowledge-graph module settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Enable graph traversal in queries. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected — Go's `if !x { x = true }` pattern that
    /// forced it back to true was a bug (and its doc comment always said "default true").
    #[serde(default = "default_true")]
    pub enable_graph: bool,
    /// Maximum BFS depth.
    pub max_depth: i32,
    /// Maximum nodes returned per query.
    pub max_nodes: i32,
    /// Load the graph into memory on startup. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected (Go forced it back to true — bug fixed).
    #[serde(default = "default_true")]
    pub load_on_startup: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        // Absent section means enabled / loaded-on-startup: the documented Go intent
        // ("default true") without its buggy force-enable (design D13). Depth/node
        // bounds stay zero here; `apply_defaults` fills them.
        Self {
            enable_graph: true,
            max_depth: 0,
            max_nodes: 0,
            load_on_startup: true,
        }
    }
}

/// Automatic file-watching / re-indexing settings (serve mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUpdateConfig {
    /// Enable filesystem monitoring. Governed by section presence (design D8): an absent
    /// `auto_update:` section is materialized with `enabled = true` by
    /// [`Config::apply_defaults`](crate::preset::Config::apply_defaults), while a present
    /// section keeps its parsed value — a key missing inside a present section stays false,
    /// matching the oracle.
    pub enabled: bool,
    /// Minimum interval between re-indexings in seconds.
    pub debounce_seconds: i32,
    /// Watch all sources listed under ingestion. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected (Go forced it back to true — bug fixed).
    #[serde(default = "default_true")]
    pub watch_sources: bool,
    /// Run a full source scan on startup. Same presence rule as [`Self::enabled`].
    pub initial_sync: bool,
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        // `watch_sources` follows presence semantics (absent → true, design D13).
        // `enabled` / `initial_sync` stay zero here on purpose: a materialized absent
        // section is force-enabled by `apply_defaults` (design D8), and a present
        // section must keep its parsed values.
        Self {
            enabled: false,
            debounce_seconds: 0,
            watch_sources: true,
            initial_sync: false,
        }
    }
}

/// Universal job scheduler settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// Registered jobs keyed by name.
    pub jobs: HashMap<String, JobConfig>,
}

/// A single scheduled job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobConfig {
    /// Whether the job is scheduled.
    pub enabled: bool,
    /// Run interval in seconds.
    pub interval_seconds: i64,
}

// ── Logging / paths / server ──────────────────────────────────────────────

tolerant_enum! {
    /// Log level (design D7: tolerant) — unknown values are preserved rather than
    /// rejected so an unrecognised value cannot break startup. An explicit `""`
    /// normalizes to [`LogLevel::Info`] at parse time (design D12).
    LogLevel, Info {
        Trace => "trace",
        Debug => "debug",
        Info  => "info",
        Warn  => "warn",
        Error => "error",
    } empty_to_default
}

tolerant_enum! {
    /// Log output format (`"console"` or `"json"`; design D7: tolerant). An explicit
    /// `""` normalizes to [`LogFormat::Console`] at parse time (design D12).
    LogFormat, Console {
        Console => "console",
        Json    => "json",
    } empty_to_default
}

tolerant_enum! {
    /// Log output destination (`"stderr"` or `"stdout"`; design D7: tolerant). An
    /// explicit `""` normalizes to [`LogOutput::Stderr`] at parse time (design D12).
    LogOutput, Stderr {
        Stderr  => "stderr",
        Stdout  => "stdout",
    } empty_to_default
}

/// Logging settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Log level (trace/debug/info/warn/error; tolerant).
    pub level: LogLevel,
    /// Output format (`"console"` or `"json"`; tolerant).
    pub format: LogFormat,
    /// Destination stream (`"stderr"` or `"stdout"`; tolerant).
    pub output: LogOutput,
}

/// Filesystem paths used by the application.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Workspace root directory for the global artifacts (models, onnxruntime,
    /// cache DB, and the `datasets/` tree). Absent or `""` → `"workspace"` at
    /// parse time (D12; storage-layout-restructure D1).
    #[serde(
        default = "default_workspace_dir",
        deserialize_with = "de_workspace_dir"
    )]
    pub workspace_dir: String,
    /// Database migrations directory. Absent or `""` → `"migrations"` (D12).
    #[serde(
        default = "default_migrations_dir",
        deserialize_with = "de_migrations_dir"
    )]
    pub migrations_dir: String,
    /// Prompt template files directory. Absent or `""` →
    /// `"workspace/configs/prompts"` (D12).
    #[serde(default = "default_prompts_path", deserialize_with = "de_prompts_path")]
    pub prompts_path: String,
    /// Path to the external `onnx.yaml` registry file. Absent or `""` →
    /// `"workspace/configs/onnx.yaml"` (D12).
    #[serde(default = "default_onnx_config", deserialize_with = "de_onnx_config")]
    pub onnx_config: String,
}

impl PathsConfig {
    /// Global cache database: `<workspace_dir>/db/cache/cache.db`
    /// (storage-layout-restructure D1 — the cache is global, not per-dataset).
    pub fn cache_db_path(&self) -> PathBuf {
        Path::new(self.workspace_dir.as_str())
            .join("db")
            .join("cache")
            .join("cache.db")
    }
}

impl Default for PathsConfig {
    fn default() -> Self {
        // Serde calls this when the whole `paths:` section is absent — per-field D12
        // attributes do not apply to a missing struct, so the defaults live here too.
        Self {
            workspace_dir: "workspace".to_string(),
            migrations_dir: "migrations".to_string(),
            prompts_path: "workspace/configs/prompts".to_string(),
            onnx_config: "workspace/configs/onnx.yaml".to_string(),
        }
    }
}

/// Active dataset (storage-layout-restructure D1): per-dataset artifacts
/// (ontology, content, state, knowledge DB, vector index) live under
/// `<workspace_dir>/datasets/<name>/`. The helpers take the workspace dir as
/// an argument so a single [`Config`] can resolve any dataset (the `--dataset`
/// override rewrites [`Self::name`] before resolution).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DatasetConfig {
    /// Dataset name (directory under `<workspace_dir>/datasets/`). Default
    /// `""` — NO dataset (revision 1.1): an empty name means "no data", and
    /// the bootstrap (task 1.3) skips ontology load + ingestion for it.
    /// `edtech` is NOT a default; only presets that ingest the demo corpus
    /// set it.
    pub name: String,
}

impl DatasetConfig {
    /// Dataset root: `<workspace_dir>/datasets/<name>`.
    fn root(&self, workspace_dir: &str) -> PathBuf {
        Path::new(workspace_dir)
            .join("datasets")
            .join(self.name.as_str())
    }

    /// Ontology directory (`global.xml` + `domains/`).
    pub fn ontology_path(&self, workspace_dir: &str) -> PathBuf {
        self.root(workspace_dir).join("ontology")
    }

    /// Ingestable content directory.
    pub fn content_path(&self, workspace_dir: &str) -> PathBuf {
        self.root(workspace_dir).join("content")
    }

    /// Dataset state directory.
    pub fn state_path(&self, workspace_dir: &str) -> PathBuf {
        self.root(workspace_dir).join("state")
    }

    /// Dataset-bound knowledge database file.
    pub fn db_path(&self, workspace_dir: &str) -> PathBuf {
        self.state_path(workspace_dir)
            .join("db")
            .join("knowledge.db")
    }

    /// Dataset-bound ANN index directory.
    pub fn vectors_path(&self, workspace_dir: &str) -> PathBuf {
        self.state_path(workspace_dir).join("vectors")
    }
}

/// MCP server identification / bind settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Human-readable service name. Absent or `""` → `"synopsis"` at parse time (D12).
    #[serde(default = "default_server_name", deserialize_with = "de_server_name")]
    pub name: String,
    /// Service version string. Absent or `""` → `"0.1.0-dev"` (D12).
    #[serde(
        default = "default_server_version",
        deserialize_with = "de_server_version"
    )]
    pub version: String,
    /// HTTP listen host. Absent or `""` → `"0.0.0.0"` (D12).
    #[serde(default = "default_server_host", deserialize_with = "de_server_host")]
    pub host: String,
    /// HTTP listen port; a non-positive value is replaced by 8080 in
    /// [`Config::apply_defaults`](crate::preset::Config::apply_defaults).
    pub port: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        // See `PathsConfig::default`: the defaults live here because serde uses the
        // struct's `Default` when the whole `server:` section is absent (D12). Port
        // stays zero; `apply_defaults` fills it (numeric rule, not a string artifact).
        Self {
            name: "synopsis".to_string(),
            version: "0.1.0-dev".to_string(),
            host: "0.0.0.0".to_string(),
            port: 0,
        }
    }
}

// ── Loading ───────────────────────────────────────────────────────────────

/// Reads and parses the YAML config at `path` into a [`Config`].
///
/// Unknown keys are ignored (matching the oracle). This performs parsing only —
/// it does **not** apply defaults or validate; call
/// [`Config::validate`](Config::validate) and
/// [`Config::apply_defaults`](Config::apply_defaults) as separate phases.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    crate::io_util::read_yaml_file(path.as_ref(), "config")
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn load_from_str(text: &str) -> Result<Config, ConfigError> {
        noyalib::from_str::<Config>(text).map_err(|source| ConfigError::Yaml {
            path: "<inline>".to_string(),
            source,
        })
    }

    fn parse(yaml: &str) -> Config {
        load_from_str(yaml).expect("test YAML should parse")
    }

    #[test]
    fn parse_unknown_logging_level_becomes_unknown() {
        let cfg = parse("logging:\n  level: verbose\n");
        assert_eq!(cfg.logging.level, LogLevel::Unknown("verbose".into()));
    }

    #[test]
    fn parse_known_logging_level_maps_to_variant() {
        let cfg = parse("logging:\n  level: warn\n");
        assert_eq!(cfg.logging.level, LogLevel::Warn);
    }

    #[test]
    fn parse_unknown_chunking_strategy_is_tolerated() {
        let cfg = parse("ingestion:\n  chunking:\n    markdown:\n      strategy: rolling\n");
        assert_eq!(
            cfg.ingestion.chunking.markdown.strategy,
            ChunkingStrategy::Unknown("rolling".into())
        );
    }

    #[test]
    fn unknown_top_level_key_does_not_break_parse() {
        let cfg = parse("totally_unknown_section:\n  foo: bar\nlogging:\n  level: info\n");
        assert_eq!(cfg.logging.level, LogLevel::Info);
    }

    #[test]
    fn validate_rejects_unknown_embeddings_mode() {
        // Oracle parity (criterion г): `mode` parses as a plain value, so a bogus
        // mode does NOT fail parsing; it is rejected by `validate()` with the
        // oracle's exact message — a ConfigError::Validation, not a YAML/parse error.
        let cfg = parse("embeddings:\n  mode: bogus\n");
        assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Unknown("bogus".into()));
        match cfg.validate() {
            Err(ConfigError::Validation { message }) => {
                assert_eq!(
                    message,
                    "unknown embeddings mode \"bogus\", want \"local\" or \"api\""
                )
            }
            other => panic!("expected Validation error for unknown mode, got: {other:?}"),
        }
    }

    #[test]
    fn validate_local_requires_model_and_dim() {
        // model_name/model_path empty and vector_dim == 0 -> error (criterion e).
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_local_ok_with_model_name_and_dim() {
        let mut cfg = Config::default();
        cfg.embeddings.mode = EmbeddingsMode::Local;
        cfg.embeddings.local.model_name = "bge-m3-int8".into();
        cfg.embeddings.local.vector_dim = 1024;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_local_rejects_zero_vector_dim() {
        let mut cfg = Config::default();
        cfg.embeddings.mode = EmbeddingsMode::Local;
        cfg.embeddings.local.model_name = "bge-m3-int8".into();
        cfg.embeddings.local.vector_dim = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_api_requires_base_url() {
        let mut cfg = Config::default();
        cfg.embeddings.mode = EmbeddingsMode::Api;
        // base_url empty -> error (criterion f).
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_api_ok_with_all_fields() {
        let mut cfg = Config::default();
        cfg.embeddings.mode = EmbeddingsMode::Api;
        cfg.embeddings.api.base_url = "http://localhost:11434/v1".into();
        cfg.embeddings.api.model_name = "text-embedding-3-large".into();
        cfg.embeddings.api.vector_dim = 3072;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_api_rejects_missing_model_name() {
        let mut cfg = Config::default();
        cfg.embeddings.mode = EmbeddingsMode::Api;
        cfg.embeddings.api.base_url = "http://localhost:11434/v1".into();
        cfg.embeddings.api.vector_dim = 3072;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_error_displays_and_is_std_error() {
        // Display + std::error::Error + source chaining (criterion g).
        let err = ConfigError::Validation {
            message: "embeddings.api.base_url is required in api mode".into(),
        };
        assert_eq!(
            err.to_string(),
            "invalid configuration: embeddings.api.base_url is required in api mode"
        );

        // Yaml variant carries a `source`.
        let yaml_err = ConfigError::Yaml {
            path: "/tmp/x.yaml".into(),
            source: noyalib::Error::Custom("bad yaml".into()),
        };
        assert!(yaml_err.to_string().contains("/tmp/x.yaml"));
        // Exercise the std::error::Error + Source plumbing (source is non-None).
        let boxed: Box<dyn std::error::Error> = Box::new(yaml_err);
        assert!(boxed.source().is_some());
    }

    #[test]
    fn tolerant_enum_round_trips_through_noyalib() {
        // Serialize -> reparse through the real YAML lib; known values map to
        // their variants and unknown values are preserved verbatim. Tuples
        // serialize as a YAML sequence, so no wrapper type is needed.
        let doc: (LogLevel, ChunkingStrategy) = (LogLevel::Info, ChunkingStrategy::Hybrid);
        let yaml = noyalib::to_string(&doc).expect("serialize");
        let back: (LogLevel, ChunkingStrategy) = noyalib::from_str(&yaml).expect("reparse");
        assert_eq!(back.0, LogLevel::Info);
        assert_eq!(back.1, ChunkingStrategy::Hybrid);

        let doc2: (LogLevel, ChunkingStrategy) = (
            LogLevel::Unknown("verbose".into()),
            ChunkingStrategy::Unknown("rolling".into()),
        );
        let yaml2 = noyalib::to_string(&doc2).expect("serialize");
        let back2: (LogLevel, ChunkingStrategy) = noyalib::from_str(&yaml2).expect("reparse");
        assert_eq!(back2.0, LogLevel::Unknown("verbose".into()));
        assert_eq!(back2.1, ChunkingStrategy::Unknown("rolling".into()));
    }

    #[test]
    fn absent_sections_default_to_zero_values() {
        // No embeddings section at all -> Local (default) + empty local fields.
        let cfg = parse("server:\n  name: synopsis\n");
        assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Local);
        assert!(cfg.embeddings.local.model_name.is_empty());
    }

    #[test]
    fn auto_update_presence_is_carried_by_option() {
        let absent = parse("server:\n  name: x\n");
        assert!(absent.auto_update.is_none());

        let present = parse("auto_update:\n  enabled: false\n");
        assert!(!present.auto_update.unwrap().enabled);
    }

    // ── apply_defaults + helpers (task 1.2) ────────────────────────────────

    #[test]
    fn empty_config_gets_all_oracle_defaults() {
        // Criterion (a): Config::default() after apply_defaults, checked against
        // every value in Go's ApplyDefaults (config.go).
        let mut cfg = Config::default();
        cfg.apply_defaults();

        // Ingestion.
        assert_eq!(cfg.ingestion.batch_size, 100);
        let md = &cfg.ingestion.chunking.markdown;
        assert_eq!(md.strategy, ChunkingStrategy::Headers);
        assert_eq!(md.max_chunk_size, 1000);
        // Go checks `< 0`: a zero overlap is preserved, not defaulted to 100.
        assert_eq!(md.overlap_size, 0);
        assert_eq!(md.min_section_size, 500);
        assert_eq!(
            cfg.ingestion.chunking.json.text_fields,
            vec!["description", "title", "wikitext", "html"]
        );

        // Search (including the reranker factors).
        let s = &cfg.search;
        assert_eq!(s.rrf_k, 20);
        assert_eq!(s.lexical_top_k, 20);
        assert_eq!(s.semantic_top_k, 20);
        assert_eq!(s.final_top_k, 10);
        // Both legs absent -> both force-enabled (Go: only when BOTH are false).
        assert!(s.enable_lexical && s.enable_semantic);
        assert_eq!(s.timeout_ms, 10_000);
        assert_eq!(s.deprecated_boost, 0.2);
        assert_eq!(s.official_boost, 1.5);
        assert_eq!(s.recent_boost, 1.2);
        assert_eq!(s.recent_days, 90);
        assert_eq!(s.authority_boost.len(), 1);
        assert_eq!(s.authority_boost.get("default"), Some(&1.0));

        // Graph: presence semantics (design D13) — absent keys mean the documented
        // default true; Go left enable_graph false despite its "default true" comment.
        let g = &cfg.graph;
        assert!(g.enable_graph);
        assert_eq!(g.max_depth, 5);
        assert_eq!(g.max_nodes, 1000);
        assert!(g.load_on_startup);

        // Auto-update: absent section -> fully enabled (D8).
        let au = cfg
            .auto_update
            .as_ref()
            .expect("absent auto_update becomes Some");
        assert!(au.enabled && au.initial_sync);
        assert_eq!(au.debounce_seconds, 30);
        assert!(au.watch_sources);

        // Scheduler gains exactly one default job: orphan_cleanup, disabled / 3600s.
        assert_eq!(cfg.scheduler.jobs.len(), 1);
        let oc = cfg.scheduler.jobs["orphan_cleanup"];
        assert!(!oc.enabled);
        assert_eq!(oc.interval_seconds, 3600);

        // Logging / paths / dataset / database (all normalized at parse time).
        assert_eq!(cfg.logging.level, LogLevel::Info);
        assert_eq!(cfg.logging.format, LogFormat::Console);
        assert_eq!(cfg.logging.output, LogOutput::Stderr);
        let p = &cfg.paths;
        assert_eq!(p.workspace_dir, "workspace");
        assert_eq!(p.migrations_dir, "migrations");
        assert_eq!(p.prompts_path, "workspace/configs/prompts");
        assert_eq!(p.onnx_config, "workspace/configs/onnx.yaml");
        // No dataset by default (revision 1.1): empty name means "no data".
        assert_eq!(cfg.dataset.name, "");

        // NER LLM defaults; linker.llm must stay untouched (Go: NER provider only).
        let llm = &cfg.ingestion.ner.llm;
        assert_eq!(llm.response_format, ResponseFormat::JsonObject);
        assert_eq!(llm.timeout_ms, 60_000);
        assert_eq!(llm.max_retries, 3);
        assert_eq!(cfg.linker.llm.timeout_ms, 0); // not defaulted
        assert_eq!(cfg.linker.llm.max_retries, 0);

        // Resolver + local embedding fallback model (applies in both modes).
        assert_eq!(cfg.ingestion.resolver.similarity_threshold, 0.8);
        assert_eq!(cfg.embeddings.local.model_name, "bge-m3-int8");

        // Server.
        let sv = &cfg.server;
        assert_eq!(sv.name, "synopsis");
        assert_eq!(sv.version, "0.1.0-dev");
        assert_eq!(sv.host, "0.0.0.0");
        assert_eq!(sv.port, 8080);
    }

    #[test]
    fn apply_defaults_preserves_explicitly_set_values() {
        let mut cfg = parse(
            r#"
ingestion:
  batch_size: 250
  chunking:
    markdown:
      strategy: fixed
      max_chunk_size: 4096
      overlap_size: 7
      min_section_size: 100
    json:
      text_fields: [a, b]
  ner:
    llm:
      response_format: json_schema
      timeout_ms: 5000
      max_retries: 9
  resolver:
    similarity_threshold: 0.42
search:
  rrf_k: 60
  enable_lexical: false   # single leg off -> respected (Go flips only when BOTH are off)
  enable_semantic: true
graph:
  max_depth: 3
auto_update:
  enabled: false          # present section -> flags respected as parsed (D8)
scheduler:
  jobs:
    orphan_cleanup:
      enabled: true
      interval_seconds: 1800
logging:
  level: warn
paths:
  workspace_dir: /var/synopsis
dataset:
  name: custom
embeddings:
  mode: local
  local:
    model_name: custom-model
server:
  port: 9090
"#,
        );
        cfg.apply_defaults();

        assert_eq!(cfg.ingestion.batch_size, 250);
        let md = &cfg.ingestion.chunking.markdown;
        assert_eq!(md.strategy, ChunkingStrategy::Fixed);
        assert_eq!(md.max_chunk_size, 4096);
        assert_eq!(md.overlap_size, 7);
        assert_eq!(md.min_section_size, 100);
        assert_eq!(cfg.ingestion.chunking.json.text_fields, vec!["a", "b"]);

        let llm = &cfg.ingestion.ner.llm;
        assert_eq!(llm.response_format, ResponseFormat::JsonSchema);
        assert_eq!(llm.timeout_ms, 5000); // not reset to 60000
        assert_eq!(llm.max_retries, 9);
        assert_eq!(cfg.ingestion.resolver.similarity_threshold, 0.42);

        let s = &cfg.search;
        assert_eq!(s.rrf_k, 60);
        assert!(!s.enable_lexical); // explicit single-leg disable survives
        assert!(s.enable_semantic);

        assert_eq!(cfg.graph.max_depth, 3);

        // Present auto_update section: parsed flags respected (D8), field defaults apply.
        let au = cfg.auto_update.as_ref().expect("auto_update present");
        assert!(!au.enabled);
        assert!(!au.initial_sync); // NOT force-enabled: the section is present
        assert_eq!(au.debounce_seconds, 30);
        assert!(au.watch_sources);

        let oc = cfg.scheduler.jobs["orphan_cleanup"];
        assert!(oc.enabled);
        assert_eq!(oc.interval_seconds, 1800); // explicit interval respected

        assert_eq!(cfg.logging.level, LogLevel::Warn); // not reset to info
        assert_eq!(cfg.paths.workspace_dir, "/var/synopsis");
        assert_eq!(cfg.dataset.name, "custom"); // explicit dataset name preserved

        assert_eq!(cfg.embeddings.local.model_name, "custom-model"); // not bge-m3-int8

        assert_eq!(cfg.server.port, 9090);
    }

    #[test]
    fn auto_update_absent_section_defaults_to_fully_enabled() {
        // D8 criterion (c): `auto_update` missing from YAML -> enabled + initial_sync.
        let mut cfg = parse("server:\n  name: x\n");
        assert!(cfg.auto_update.is_none()); // load does not invent the section
        cfg.apply_defaults();
        let au = cfg
            .auto_update
            .expect("defaults must materialize the section");
        assert!(au.enabled);
        assert!(au.initial_sync);
        assert_eq!(au.debounce_seconds, 30);
        assert!(au.watch_sources);
    }

    #[test]
    fn auto_update_present_section_respects_parsed_flags() {
        // D8 criterion (c): explicit `auto_update:` with enabled=false stays false —
        // Go's autoUpdateConfigured flag skips the force-enable for present sections.
        let mut cfg = parse("auto_update:\n  enabled: false\n");
        cfg.apply_defaults();
        let au = cfg.auto_update.expect("present section survives as Some");
        assert!(!au.enabled);
        assert!(!au.initial_sync); // not force-enabled either (the section was present)
        assert_eq!(au.debounce_seconds, 30); // field defaults still apply inside it
        assert!(au.watch_sources);
    }

    #[test]
    fn db_path_is_dataset_derived_and_not_configurable() {
        // storage-layout-restructure D1 (revision 1.1): the knowledge-DB path
        // is NOT configurable — `database.path` is gone; the path is derived
        // from workspace_dir + dataset.name via DatasetConfig::db_path.
        let mut cfg = Config::default();
        cfg.paths.workspace_dir = "mydata".to_string();
        cfg.dataset.name = "edtech".to_string();
        assert_eq!(
            cfg.dataset.db_path(&cfg.paths.workspace_dir),
            PathBuf::from("mydata").join("datasets/edtech/state/db/knowledge.db")
        );

        // The default empty name (no dataset) yields the degenerate form;
        // guarding against it is the bootstrap's job (task 1.3), not the
        // config model's.
        let mut no_dataset = Config::default();
        no_dataset.paths.workspace_dir = "mydata".to_string();
        assert_eq!(
            no_dataset.dataset.db_path(&no_dataset.paths.workspace_dir),
            PathBuf::from("mydata").join("datasets/state/db/knowledge.db")
        );
    }

    #[test]
    fn cache_db_path_is_global_under_workspace_dir() {
        // storage-layout-restructure D1: the cache is global, at
        // <workspace_dir>/db/cache/cache.db, independent of the dataset.
        let mut cfg = Config::default();
        cfg.paths.workspace_dir = "mydata".to_string();
        assert_eq!(
            cfg.cache_db_path(),
            PathBuf::from("mydata").join("db/cache/cache.db")
        );
        assert_eq!(cfg.paths.cache_db_path(), cfg.cache_db_path());
    }

    #[test]
    fn dataset_helpers_resolve_under_workspace_dir() {
        // storage-layout-restructure D1: the five per-dataset helpers.
        // The default is NO dataset (revision 1.1); the helper assertions
        // below use an explicit name.
        assert_eq!(DatasetConfig::default().name, "");
        let dataset = DatasetConfig {
            name: "edtech".to_string(),
        };
        assert_eq!(
            dataset.ontology_path("ws"),
            PathBuf::from("ws").join("datasets/edtech/ontology")
        );
        assert_eq!(
            dataset.content_path("ws"),
            PathBuf::from("ws").join("datasets/edtech/content")
        );
        assert_eq!(
            dataset.state_path("ws"),
            PathBuf::from("ws").join("datasets/edtech/state")
        );
        assert_eq!(
            dataset.db_path("ws"),
            PathBuf::from("ws").join("datasets/edtech/state/db/knowledge.db")
        );
        assert_eq!(
            dataset.vectors_path("ws"),
            PathBuf::from("ws").join("datasets/edtech/state/vectors")
        );

        // A different dataset name rewrites every helper.
        let other = DatasetConfig {
            name: "medtech".to_string(),
        };
        assert_eq!(
            other.db_path("ws"),
            PathBuf::from("ws").join("datasets/medtech/state/db/knowledge.db")
        );
    }

    #[test]
    fn vector_dim_follows_embeddings_mode() {
        // Criterion (e).
        let mut cfg = Config::default(); // mode == Local by the enum default
        cfg.embeddings.local.vector_dim = 1024;
        assert_eq!(cfg.vector_dim(), 1024);

        cfg.embeddings.mode = EmbeddingsMode::Api;
        cfg.embeddings.api.vector_dim = 3072;
        assert_eq!(cfg.vector_dim(), 3072);

        // Unrecognized mode -> 0 (Go default branch).
        let bogus = parse("embeddings:\n  mode: bogus\n");
        assert_eq!(bogus.vector_dim(), 0);
    }

    #[test]
    fn orphan_cleanup_explicit_enabled_keeps_default_interval() {
        // Criterion (f) / Go "explicitly enabled job preserved":
        // {enabled: true} -> interval still defaults to 3600.
        let mut cfg = parse("scheduler:\n  jobs:\n    orphan_cleanup:\n      enabled: true\n");
        cfg.apply_defaults();
        let oc = cfg.scheduler.jobs["orphan_cleanup"];
        assert!(oc.enabled);
        assert_eq!(oc.interval_seconds, 3600);
    }

    #[test]
    fn orphan_cleanup_explicit_interval_respected() {
        // Criterion (f) / Go "explicitly configured interval preserved".
        let mut cfg = parse(
            r#"
scheduler:
  jobs:
    nightly:
      enabled: true
      interval_seconds: 60
    orphan_cleanup:
      enabled: true
      interval_seconds: 7200
"#,
        );
        cfg.apply_defaults();
        let oc = cfg.scheduler.jobs["orphan_cleanup"];
        assert!(oc.enabled);
        assert_eq!(oc.interval_seconds, 7200);
        // Unrelated jobs are untouched.
        let nightly = cfg.scheduler.jobs["nightly"];
        assert!(nightly.enabled && nightly.interval_seconds == 60);
    }

    #[test]
    fn orphan_cleanup_explicit_disabled_with_interval_preserved() {
        // Criterion (f) / Go "explicitly disabled with custom interval preserved".
        let mut cfg = parse(
            r#"
scheduler:
  jobs:
    orphan_cleanup:
      enabled: false
      interval_seconds: 1800
"#,
        );
        cfg.apply_defaults();
        let oc = cfg.scheduler.jobs["orphan_cleanup"];
        assert!(!oc.enabled);
        assert_eq!(oc.interval_seconds, 1800);
    }

    #[test]
    fn explicit_false_presence_bools_are_respected() {
        // Design D13 (BREAKING vs Go): an explicit false survives defaulting — Go's
        // `if !x { x = true }` pattern made these settings non-functional.
        let mut cfg = parse(
            r#"
graph:
  enable_graph: false
  load_on_startup: false
auto_update:
  enabled: false
  watch_sources: false
"#,
        );
        cfg.apply_defaults();
        assert!(!cfg.graph.enable_graph);
        assert!(!cfg.graph.load_on_startup);
        let au = cfg.auto_update.expect("present section survives");
        assert!(!au.enabled);
        assert!(!au.watch_sources);
    }

    #[test]
    fn absent_presence_bools_default_to_true() {
        // Design D13: an absent key means the documented default (true) — both when
        // the whole section is missing and when only the key is missing.
        let cfg = parse("server:\n  name: x\n");
        assert!(cfg.graph.enable_graph);
        assert!(cfg.graph.load_on_startup);

        let partial = parse("auto_update:\n  enabled: false\n");
        assert!(
            partial.auto_update.expect("present section").watch_sources,
            "a key absent inside a present section defaults to true"
        );
    }

    #[test]
    fn empty_defaulted_strings_and_enums_normalize_at_parse_time() {
        // Design D12: an explicit "" is normalized during deserialization — the
        // assertions below hold WITHOUT calling apply_defaults.
        let cfg = parse(
            r#"
paths:
  workspace_dir: ""
  migrations_dir: ""
  prompts_path: ""
  onnx_config: ""
dataset:
  name: ""
server:
  name: ""
  version: ""
  host: ""
logging:
  level: ""
  format: ""
  output: ""
ingestion:
  chunking:
    markdown:
      strategy: ""
  ner:
    llm:
      response_format: ""
"#,
        );
        let p = &cfg.paths;
        assert_eq!(p.workspace_dir, "workspace");
        assert_eq!(p.migrations_dir, "migrations");
        assert_eq!(p.prompts_path, "workspace/configs/prompts");
        assert_eq!(p.onnx_config, "workspace/configs/onnx.yaml");
        // An explicit empty dataset name stays empty (revision 1.1: no data).
        assert_eq!(cfg.dataset.name, "");
        let sv = &cfg.server;
        assert_eq!(sv.name, "synopsis");
        assert_eq!(sv.version, "0.1.0-dev");
        assert_eq!(sv.host, "0.0.0.0");
        assert_eq!(cfg.logging.level, LogLevel::Info);
        assert_eq!(cfg.logging.format, LogFormat::Console);
        assert_eq!(cfg.logging.output, LogOutput::Stderr);
        assert_eq!(
            cfg.ingestion.chunking.markdown.strategy,
            ChunkingStrategy::Headers
        );
        assert_eq!(
            cfg.ingestion.ner.llm.response_format,
            ResponseFormat::JsonObject
        );
    }

    #[test]
    fn absent_sections_default_to_oracle_values_at_parse_time() {
        // Design D12: a whole section missing from YAML yields the struct's Default —
        // serde does not run per-field attributes on an absent struct.
        // The fixture's `database.path` key no longer exists in the schema
        // (revision 1.1): it is ignored like any unknown key (Go parity).
        let cfg = parse("database:\n  path: x\n");
        assert_eq!(cfg.paths.workspace_dir, "workspace");
        assert_eq!(cfg.paths.onnx_config, "workspace/configs/onnx.yaml");
        // No dataset by default (revision 1.1).
        assert_eq!(cfg.dataset.name, "");
        assert_eq!(cfg.server.name, "synopsis");
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert!(cfg.graph.enable_graph);
        assert!(cfg.graph.load_on_startup);
    }

    #[test]
    fn empty_embeddings_mode_is_still_rejected_by_validate() {
        // EmbeddingsMode has no oracle default, so it is NOT one of D12's normalized
        // enums: "" stays Unknown("") and validate() rejects it with the oracle's
        // exact message (Go parity), like any other unrecognized mode.
        let cfg = parse("embeddings:\n  mode: \"\"\n");
        assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Unknown(String::new()));
        match cfg.validate() {
            Err(ConfigError::Validation { message }) => {
                assert_eq!(
                    message,
                    "unknown embeddings mode \"\", want \"local\" or \"api\""
                )
            }
            other => panic!("expected Validation error for empty mode, got: {other:?}"),
        }
    }

    #[test]
    fn negative_overlap_size_defaults_to_100_but_zero_is_kept() {
        let mut cfg = parse("ingestion:\n  chunking:\n    markdown:\n      overlap_size: -5\n");
        cfg.apply_defaults();
        assert_eq!(cfg.ingestion.chunking.markdown.overlap_size, 100);

        // Zero value stays zero (Go checks `< 0`, not `<= 0`).
        let mut zeroed = Config::default();
        zeroed.apply_defaults();
        assert_eq!(zeroed.ingestion.chunking.markdown.overlap_size, 0);
    }

    // ── vectors section (vectors change task 1.5, design D7) ───────────────

    #[test]
    fn vectors_section_absent_defaults_to_adr_0003() {
        // Preset without the section: load keeps it None; vectors_config()
        // resolves to the ADR 0003 defaults.
        let cfg = parse("server:\n  name: x\n");
        assert!(cfg.vectors.is_none());
        let eff = cfg.vectors_config();
        assert_eq!(eff.dim, 1024);
        assert_eq!(eff.m, 16);
        assert_eq!(eff.ef_construction, 100);
        assert_eq!(eff.num_partitions, 256);
        assert_eq!(eff.nprobes, 32);
        assert_eq!(eff.ef_search, 200);
        assert_eq!(eff, VectorsConfig::default());

        // The zero-value config resolves the same way.
        assert_eq!(Config::default().vectors_config(), VectorsConfig::default());
    }

    #[test]
    fn vectors_section_full_override() {
        let cfg = parse(
            r#"
vectors:
  dim: 512
  m: 8
  ef_construction: 64
  num_partitions: 16
  nprobes: 4
  ef_search: 100
"#,
        );
        assert_eq!(
            cfg.vectors_config(),
            VectorsConfig {
                dim: 512,
                m: 8,
                ef_construction: 64,
                num_partitions: 16,
                nprobes: 4,
                ef_search: 100,
            }
        );
    }

    #[test]
    fn vectors_section_partial_override_fills_adr_defaults() {
        // A key missing inside a present section resolves to the ADR 0003
        // value — the same as an absent section.
        let cfg = parse("vectors:\n  ef_search: 300\n");
        let eff = cfg.vectors_config();
        assert_eq!(eff.ef_search, 300);
        assert_eq!(eff.dim, 1024);
        assert_eq!(eff.m, 16);
        assert_eq!(eff.ef_construction, 100);
        assert_eq!(eff.num_partitions, 256);
        assert_eq!(eff.nprobes, 32);
    }

    #[test]
    fn vectors_section_unknown_key_does_not_break_parse() {
        // Additive safety (config-format spec): unknown keys are ignored.
        let cfg = parse("vectors:\n  bogus: 1\n  dim: 512\n");
        let eff = cfg.vectors_config();
        assert_eq!(eff.dim, 512);
        assert_eq!(eff.ef_search, 200);
    }

    #[test]
    fn vectors_section_yaml_roundtrip() {
        // Present section survives serialize -> reparse byte-stably.
        let cfg = parse("server:\n  name: synopsis\nvectors:\n  dim: 512\n  ef_search: 300\n");
        let yaml = noyalib::to_string(&cfg).expect("serialize");
        let back: Config = noyalib::from_str(&yaml).expect("reparse");
        assert_eq!(back.vectors, cfg.vectors);
        assert_eq!(noyalib::to_string(&back).expect("serialize"), yaml);
    }

    #[test]
    fn vectors_section_skipped_in_yaml_when_absent() {
        // Presets without the section stay compatible: serializing a config
        // parsed from one does not invent the section.
        let cfg = parse("server:\n  name: x\n");
        let yaml = noyalib::to_string(&cfg).expect("serialize");
        assert!(
            !yaml.contains("\nvectors:"),
            "absent section must not be serialized: {yaml}"
        );
        let back: Config = noyalib::from_str(&yaml).expect("reparse");
        assert!(back.vectors.is_none());
    }

    // ── Shipped presets (ship-ontology-demo-data task 1.3) ─────────────────

    /// Shared new-shape assertions for the shipped presets
    /// (storage-layout-restructure D1/D8). The per-preset `dataset.name` and
    /// its derived DB path are asserted by each test (revision 1.1: no
    /// default dataset; the DB path is derived, not configurable).
    fn assert_workspace_shape(cfg: &Config) {
        assert_eq!(cfg.paths.workspace_dir, "workspace");
        assert_eq!(cfg.paths.prompts_path, "workspace/configs/prompts");
        assert_eq!(cfg.paths.onnx_config, "workspace/configs/onnx.yaml");
        // The global cache path is identical for every preset.
        assert_eq!(
            cfg.paths.cache_db_path(),
            PathBuf::from("workspace/db/cache/cache.db")
        );
    }

    #[test]
    fn loads_demo_config() {
        // The demo preset (workspace/configs/config.demo.yaml) must parse
        // through the real file loader into the storage-layout-restructure
        // shape. Ingestion sources live in the dataset ontology (global.xml),
        // not in this file. CARGO_MANIFEST_DIR is `<repo>/crates/config`, so
        // two `..` reach the repo root where `workspace/configs/` lives.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../workspace/configs/config.demo.yaml"
        );
        let cfg = load(path).expect("demo config must parse");
        assert_workspace_shape(&cfg);
        // The demo preset ingests the shipped corpus: its dataset is edtech.
        assert_eq!(cfg.dataset.name, "edtech");
        assert_eq!(
            cfg.dataset.db_path(&cfg.paths.workspace_dir),
            PathBuf::from("workspace/datasets/edtech/state/db/knowledge.db")
        );
        assert_eq!(
            cfg.dataset.ontology_path(&cfg.paths.workspace_dir),
            PathBuf::from("workspace/datasets/edtech/ontology")
        );
    }

    #[test]
    fn loads_default_config() {
        // The default preset must parse into the same new shape — with NO
        // dataset (revision 1.1): empty name means "no data", and the
        // bootstrap (task 1.3) skips ontology load + ingestion for it.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../workspace/configs/config.default.yaml"
        );
        let cfg = load(path).expect("default config must parse");
        assert_workspace_shape(&cfg);
        assert_eq!(cfg.dataset.name, "");
    }
}
