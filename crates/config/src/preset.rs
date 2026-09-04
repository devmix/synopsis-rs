//! Main YAML preset: typed [`Config`] structures plus load / validate / defaults.
//!
//! [`Config`] structures, [`load`], [`Config::validate`] and
//! [`Config::apply_defaults`] with the derived helpers [`Config::vector_dim`]
//! / [`Config::cache_db_path`]. The knowledge-DB path is NOT a config field:
//! it is derived from `workspace_dir` + `dataset.name` via
//! [`DatasetConfig::db_path`]. Defaulting follows design D12/D13 (revision of
//! task 1.2):
//!
//! * **YAML artifacts are normalized at deserialization** ("parse, don't
//!   validate"): an absent key on a defaulted field yields that field's
//!   documented default, and an explicit `""` on the five tolerant enums /
//!   eight string fields normalizes to the same default. After
//!   [`load`](crate::load) those values are always valid — no runtime code
//!   ever sees an empty artifact.
//! * **Semantic rules live in [`Config::apply_defaults`]**: numeric `<= 0`
//!   fallbacks (`overlap_size`: `< 0`, so a configured `0` survives), conditional
//!   pairs (both search legs off → both on; no local model set → `"bge-m3-int8"`),
//!   maps/lists (`text_fields`, `authority_boost`, scheduler jobs) and the
//!   `auto_update:` section-presence rule (design D8).
//! * **Buggy bool defaults are fixed** (D13, BREAKING): `enable_graph`,
//!   `load_on_startup` and `watch_sources` use presence semantics — absent →
//!   true, an explicit `false` is respected.
//! * **`vectors:` is an additive extension** (design D7, human decision
//!   2026-08-21): the previous vector search (vec0 brute-force) had no ANN
//!   parameters to tune, so the section stores raw fields with ADR 0003
//!   defaults and unknown-key tolerance keeps old presets compatible. An
//!   absent section stays `None` (and is skipped on re-serialization);
//!   [`Config::vectors_config`] resolves it to the defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

// ── Tolerant-enum machinery (defined before first use) ────────────────────

/// Declares a tolerant string enum (design D7): known values map to named
/// variants, any other value is captured verbatim in `Unknown(String)` instead
/// of failing the parse. Serializes back to its canonical lowercase word (or the
/// original text for unknowns), so round-tripping preserves intent. The second
/// token names the variant used by [`Default`], chosen as that field's
/// documented default so an absent YAML key deserializes straight to the
/// intended value.
///
/// A trailing clause selects how an explicit empty string is handled (design D12):
/// * `empty_to_default` — `""` normalizes to the Default variant at parse time;
///   used by every enum whose field has an unconditional default (`strategy`,
///   `level`, `format`, `output`, `response_format`).
/// * *(clause absent)* — `""` stays `Unknown("")`; used by strict enums without a
///   default ([`EmbeddingsMode`]): an empty mode must still be rejected by
///   [`Config::validate`] with the established message, like any other bogus value.
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
// default, or `Self::Unknown(String::new())` for strict enums without one. It must
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
                // The zero value equals this field's default, so an absent
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
                // Case-insensitive match on known words (lowercase in the config
                // format); unknown values are preserved verbatim for diagnostics.
                // An explicit "" is a YAML artifact of an unset field:
                // `$empty_fallback` normalizes it to the field's default (design D12)
                // or keeps it as Unknown("") for strict enums.
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
/// default (design D12): `default_*()` supplies the value when the YAML key is
/// absent, and `de_*()` normalizes an explicit empty string to it. serde's
/// `deserialize_with` accepts only a zero-argument path, so each field gets its own
/// pair even though the logic is identical (the D12 "de_empty_to_default" pattern).
macro_rules! empty_string_default {
    ($default_fn:ident, $de_fn:ident, $value:expr) => {
        /// Default for a config string field (design D12).
        fn $default_fn() -> String {
            $value.to_string()
        }

        /// Deserialize a config string; an explicit `""` becomes the field's
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
    /// Exactly one embeddings mode is legal and each mode has its own set of
    /// mandatory fields. Returns [`ConfigError::Validation`] on the first
    /// violated invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // The mode itself is checked here, not at parse time: an unrecognized
        // value yields a Validation error rather than a YAML/parse failure.
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

    /// Fills zero-value fields with the documented defaults — the **semantic** half of
    /// the defaulting split (design D12). YAML artifacts are already normalized by
    /// deserialization: absent keys and explicit `""` on defaulted string/enum fields,
    /// plus presence-semantics bools (`enable_graph`, `load_on_startup`,
    /// `watch_sources`, design D13) never reach this method empty or false-by-default.
    /// What remains here are the semantic rules:
    ///
    /// * Numeric `<= 0` fallbacks — except `markdown.overlap_size`, which is checked
    ///   with `< 0`, so a configured `0` survives.
    /// * Conditional pairs: both search legs force-enabled only when **both** are
    ///   disabled (an explicit single-leg disable is respected); and the local model
    ///   fallback — `embeddings.local.model_name` becomes `"bge-m3-int8"` in **both**
    ///   modes, applied regardless of `mode`.
    /// * Maps / lists: an empty `json.text_fields` gains the four default fields; an
    ///   empty `authority_boost` map gains `"default": 1.0`; the scheduler always
    ///   gains an `orphan_cleanup` job — **disabled** with a 3600 s interval unless
    ///   explicitly configured (disabled by default, not enabled: the job performs
    ///   full-table scans).
    /// * Section presence: an `auto_update:` section missing from YAML is materialized
    ///   fully enabled (`enabled = initial_sync = true`); a present one is respected as
    ///   parsed. Presence rides on the [`Option`] itself (design D8).
    pub fn apply_defaults(&mut self) {
        // Ingestion ------------------------------------------------------------------
        if self.ingestion.batch_size <= 0 {
            self.ingestion.batch_size = 100;
        }
        // `max_retries` is normalized at deserialization time (serde default); this
        // fills the zero value left by an absent `ingestion:` section (numeric rule).
        if self.ingestion.max_retries <= 0 {
            self.ingestion.max_retries = 3;
        }
        let md = &mut self.ingestion.chunking.markdown;
        if md.max_chunk_size <= 0 {
            md.max_chunk_size = 1000;
        }
        // Checked with `< 0`, not `<= 0`: a configured overlap of 0 is preserved.
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
        // `retry_failed` is normalized at deserialization time: an absent key yields
        // `RetryFailedConfig::default()` (enabled, 60 s), and a key absent inside a
        // present `retry_failed:` map gets its per-field serde default. Only a
        // non-positive explicit interval needs the numeric fallback.
        if auto.retry_failed.poll_interval_seconds <= 0 {
            auto.retry_failed.poll_interval_seconds = 60;
        }
        // `watch_sources` is normalized at deserialization time (design D13).
        // Absent section defaults to fully enabled; a present one is respected as
        // parsed (this branch is skipped for a present section).
        if !auto_update_configured {
            auto.enabled = true;
            auto.initial_sync = true;
        }

        // Scheduler ------------------------------------------------------------------------
        // An entry missing from YAML decodes to the zero value, so `entry` + default
        // covers the absent-entry case. The job is disabled by default (full-table
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

        // NER LLM (these defaults apply to the NER provider only; `linker.llm` untouched) ----
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

        // Local embedding (this fallback applies in both modes) -------------------------------------
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
        // An explicit empty map is indistinguishable from an absent one here,
        // so `is_empty` is the closest check.
        if self.search.authority_boost.is_empty() {
            self.search
                .authority_boost
                .insert("default".to_string(), 1.0);
        }
    }

    /// Returns the configured embedding vector dimension for the active mode:
    /// local → `local.vector_dim`, api → `api.vector_dim`, any unrecognized mode → 0.
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
        self.vectors.clone().unwrap_or_default()
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
    /// leniently so an unrecognized value does NOT fail parsing — the value is
    /// checked by [`Config::validate`] instead, so a config that loads can still be
    /// rejected at validation. [`Config::validate`] then rejects any non-`local`/
    /// non-`api` value with the established message, which is what makes this
    /// "strict" (invalid values error at validation) versus the purely tolerant
    /// enums that are preserved without ever failing. `mode` has no default, so it
    /// does NOT opt into D12 empty-string normalization: an explicit `""` stays
    /// `Unknown("")` and is rejected by [`Config::validate`] like any other bogus
    /// value.
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

/// Default for [`IngestionConfig::max_retries`] (document-jobs-queue 1.2): the
/// maximum number of automatic re-index attempts for a problematic document
/// before the background worker flips it to `error` in the `document_jobs`
/// queue.
fn default_max_retries() -> i32 {
    3
}

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
    /// Max automatic re-index attempts for a problematic document before the
    /// background worker flips it to `error` (document-jobs-queue 1.2). Absent
    /// → 3 at parse time; a non-positive value is replaced by 3 in
    /// [`Config::apply_defaults`](crate::preset::Config::apply_defaults)
    /// (numeric rule, same as `batch_size`).
    #[serde(default = "default_max_retries")]
    pub max_retries: i32,
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

/// NER configuration (LLM provider).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NerConfig {
    /// When true no NER runs at all.
    pub disabled: bool,
    /// LLM-based NER provider settings.
    pub llm: LlmConfig,
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
// 2026-08-21): the previous vector search (vec0 brute-force) had no ANN
// parameters to tune, and unknown keys are ignored, so presets without the
// section stay compatible. Defaults are the ADR 0003 configuration.

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

/// Validates the `vectors.engine` field at parse time
/// (add-usearch-ann-engine, design.md "Runtime"): a present value must be
/// exactly `"usearch"` — the only engine. The removed engine is rejected with
/// an explicit removal error (a loud, actionable failure instead of a silent
/// engine swap, design D2 of the engine-removal change); any other value
/// fails the parse. An absent key stays `None` — the wiring
/// (`vectors::create_vector_engine`) resolves the default engine (`"usearch"`),
/// keeping pre-field presets backward compatible.
fn de_vectors_engine<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value {
        Some(engine) if engine == "usearch" => Ok(Some(engine)),
        Some(engine) if engine == "lance" => Err(SerdeError::custom(
            "vectors.engine: the \"lance\" engine was removed; the only engine is \"usearch\"",
        )),
        Some(engine) => Err(SerdeError::custom(format!(
            "vectors.engine must be \"usearch\", got {engine:?}"
        ))),
        None => Ok(None),
    }
}

/// Default for the `vectors.quantization` field: `"bf16"` — the usearch
/// engine's default scalar quantization (supersedes ADR 0003's u8).
fn default_quantization() -> String {
    "bf16".into()
}

/// Validates the `vectors.quantization` field at parse time: a present value
/// must be one of `"u8"`, `"i8"`, `"f16"`, `"bf16"`, `"f32"` (case-
/// insensitive, normalized to lowercase), otherwise the parse fails. An absent
/// key resolves to the default (`"bf16"`) via serde `default`, so this
/// deserializer only sees explicitly-present values.
fn de_vectors_quantization<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "u8" | "i8" | "f16" | "bf16" | "f32" => Ok(normalized),
        other => Err(SerdeError::custom(format!(
            "vectors.quantization must be one of \"u8\", \"i8\", \"f16\", \"bf16\", \"f32\", got {other:?}"
        ))),
    }
}

/// Default for `vectors.usearch.max_segment_vectors` (usearch-wal-persistence
/// task 2.1): maximum vectors per segment after compaction.
fn default_usearch_max_segment_vectors() -> usize {
    1_000_000
}

/// Default for `vectors.usearch.compaction_stale_threshold` (usearch-wal-
/// persistence task 2.1): the stale-vector percentage that triggers
/// compaction.
fn default_usearch_compaction_stale_threshold() -> u8 {
    30
}

/// Default for `vectors.usearch.search_threads` (usearch-wal-persistence
/// task 2.1): number of parallel search threads (rayon).
fn default_usearch_search_threads() -> usize {
    4
}

/// usearch engine tuning (usearch-wal-persistence task 2.1): segment size
/// after compaction, the stale-vector percentage that triggers compaction,
/// and the number of parallel search threads (rayon).
///
/// Mirrors `vectors::UsearchConfig` (dependency direction D7: this crate
/// cannot depend on `vectors`), so the wiring maps it field by field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsearchConfig {
    /// Maximum vectors per segment after compaction.
    #[serde(default = "default_usearch_max_segment_vectors")]
    pub max_segment_vectors: usize,
    /// Stale-vector percentage (1-100) that triggers compaction.
    #[serde(default = "default_usearch_compaction_stale_threshold")]
    pub compaction_stale_threshold: u8,
    /// Number of parallel search threads (rayon).
    #[serde(default = "default_usearch_search_threads")]
    pub search_threads: usize,
}

impl Default for UsearchConfig {
    /// Engine defaults: 1M vectors per segment, 30% stale threshold, 4
    /// search threads.
    fn default() -> Self {
        Self {
            max_segment_vectors: default_usearch_max_segment_vectors(),
            compaction_stale_threshold: default_usearch_compaction_stale_threshold(),
            search_threads: default_usearch_search_threads(),
        }
    }
}

/// Validates the `vectors.usearch` section at parse time (usearch-wal-
/// persistence task 2.1): `max_segment_vectors > 0`,
/// `compaction_stale_threshold` in 1..=100 and `search_threads > 0`. An
/// absent section stays `None` (the engine resolves
/// [`UsearchConfig::default`]).
fn de_usearch_config<'de, D>(deserializer: D) -> Result<Option<UsearchConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<UsearchConfig>::deserialize(deserializer)?;
    let Some(cfg) = value else {
        return Ok(None);
    };
    if cfg.max_segment_vectors == 0 {
        return Err(SerdeError::custom(
            "vectors.usearch.max_segment_vectors must be > 0",
        ));
    }
    if !(1..=100).contains(&cfg.compaction_stale_threshold) {
        return Err(SerdeError::custom(format!(
            "vectors.usearch.compaction_stale_threshold must be in 1..=100, got {}",
            cfg.compaction_stale_threshold
        )));
    }
    if cfg.search_threads == 0 {
        return Err(SerdeError::custom(
            "vectors.usearch.search_threads must be > 0",
        ));
    }
    Ok(Some(cfg))
}

/// ANN index and query parameters for the vector search leg (design D7).
///
/// Raw preset fields only: this crate does not depend on `vectors`
/// (dependency direction, design D7), so the mapping to
/// `vectors::VectorIndexConfig` happens at the wiring level (a future
/// change). Field names and types mirror `VectorIndexConfig` so the mapping
/// is a field-by-field copy — except `engine`, which selects the ANN engine
/// at the wiring level (add-usearch-ann-engine) and has no
/// `VectorIndexConfig` counterpart.
///
/// Defaults are the ADR 0003 configuration; a key missing inside a present
/// section resolves to the same value as an absent section
/// ([`Config::vectors_config`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// ANN engine selection (add-usearch-ann-engine, design.md):
    /// `"usearch"` — the only engine; the removed engine is rejected with an
    /// explicit error at parse time (see [`de_vectors_engine`]). `None` when
    /// the key is absent: the wiring resolves it to the default engine
    /// (`"usearch"`), so presets written before the field stay backward
    /// compatible.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_vectors_engine"
    )]
    pub engine: Option<String>,
    /// Scalar quantization for the ANN index (usearch engine): one of `"u8"`,
    /// `"i8"`, `"f16"`, `"bf16"`, `"f32"`. Default `"bf16"`. An absent key
    /// resolves to the default; an unrecognized value fails the parse.
    #[serde(
        default = "default_quantization",
        deserialize_with = "de_vectors_quantization"
    )]
    pub quantization: String,
    /// usearch engine tuning (WAL segments + compaction, usearch-wal-
    /// persistence task 2.1). `None` when the key is absent: the engine
    /// resolves it to [`UsearchConfig::default`]. Invalid values fail the
    /// parse (see [`de_usearch_config`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_usearch_config"
    )]
    pub usearch: Option<UsearchConfig>,
}

impl Default for VectorsConfig {
    /// ADR 0003 configuration: 1024-dim, M=16, efConstruction=100, 256
    /// partitions, nprobes=32, efSearch=200 — the per-field serde defaults.
    /// `engine` stays `None`: an absent key resolves to the default engine
    /// at the wiring level (add-usearch-ann-engine). `usearch` stays
    /// `None`: an absent key resolves to [`UsearchConfig::default`] at the
    /// engine level (usearch-wal-persistence task 2.1).
    fn default() -> Self {
        Self {
            dim: default_vectors_dim(),
            m: default_vectors_m(),
            ef_construction: default_vectors_ef_construction(),
            num_partitions: default_vectors_num_partitions(),
            nprobes: default_vectors_nprobes(),
            ef_search: default_vectors_ef_search(),
            engine: None,
            quantization: default_quantization(),
            usearch: None,
        }
    }
}

// ── Graph / storage ───────────────────────────────────────────────────────

/// Knowledge-graph module settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Enable graph traversal in queries. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected — the old force-back-to-true pattern was
    /// a bug (the documented intent was "default true").
    #[serde(default = "default_true")]
    pub enable_graph: bool,
    /// Maximum BFS depth.
    pub max_depth: i32,
    /// Maximum nodes returned per query.
    pub max_nodes: i32,
    /// Load the graph into memory on startup. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected (a silent force-back-to-true was a bug —
    /// fixed).
    #[serde(default = "default_true")]
    pub load_on_startup: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        // Absent section means enabled / loaded-on-startup: the documented intent
        // ("default true") without the buggy force-enable (design D13). Depth/node
        // bounds stay zero here; `apply_defaults` fills them.
        Self {
            enable_graph: true,
            max_depth: 0,
            max_nodes: 0,
            load_on_startup: true,
        }
    }
}

/// Default poll interval for the failed-document retry sweep
/// (document-jobs-queue 1.2): seconds between `document_jobs` queue polls.
fn default_retry_poll_interval() -> i32 {
    60
}

/// Background re-processing of failed documents (document-jobs-queue 1.2):
/// enables the worker's retry sweep of the `document_jobs` queue and sets its
/// poll interval. An absent key resolves to [`RetryFailedConfig::default`]:
/// enabled with a 60 s poll interval — backward compatible with presets that
/// predate the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryFailedConfig {
    /// Enable background re-processing of failed documents. Presence semantics
    /// (design D13): absent → true; an explicit `false` is respected.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Poll interval for the `document_jobs` queue in seconds. Absent → 60 at
    /// parse time; a non-positive value is replaced by 60 in
    /// [`Config::apply_defaults`](crate::preset::Config::apply_defaults)
    /// (numeric rule, same as `debounce_seconds`).
    #[serde(default = "default_retry_poll_interval")]
    pub poll_interval_seconds: i32,
}

impl Default for RetryFailedConfig {
    fn default() -> Self {
        // The retry sweep is on by default with a 60 s poll interval
        // (document-jobs-queue 1.2, backward compatible).
        Self {
            enabled: default_true(),
            poll_interval_seconds: default_retry_poll_interval(),
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
    /// section keeps its parsed value — a key missing inside a present section stays
    /// false.
    pub enabled: bool,
    /// Minimum interval between re-indexings in seconds.
    pub debounce_seconds: i32,
    /// Watch all sources listed under ingestion. Presence semantics (design D13): absent →
    /// true; an explicit `false` is respected (a silent force-back-to-true was a bug —
    /// fixed).
    #[serde(default = "default_true")]
    pub watch_sources: bool,
    /// Run a full source scan on startup. Same presence rule as [`Self::enabled`].
    pub initial_sync: bool,
    /// Background re-processing of failed documents (document-jobs-queue 1.2). Absent key
    /// → [`RetryFailedConfig::default`] (enabled, 60 s poll interval).
    #[serde(default)]
    pub retry_failed: RetryFailedConfig,
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        // `watch_sources` follows presence semantics (absent → true, design D13).
        // `enabled` / `initial_sync` stay zero here on purpose: a materialized absent
        // section is force-enabled by `apply_defaults` (design D8), and a present
        // section must keep its parsed values. `retry_failed` carries its documented
        // defaults (enabled, 60 s) — there is no force-off counterpart.
        Self {
            enabled: false,
            debounce_seconds: 0,
            watch_sources: true,
            initial_sync: false,
            retry_failed: RetryFailedConfig::default(),
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

    /// Dataset-bound ANN index directory for a specific engine
    /// (add-usearch-ann-engine task 1.5): `<vectors_path>/<engine>`. `engine`
    /// is the resolved engine name (`"usearch"` — the only engine; the
    /// absent-field default is resolved by the wiring, not here).
    pub fn vectors_engine_path(&self, workspace_dir: &str, engine: &str) -> PathBuf {
        self.vectors_path(workspace_dir).join(engine)
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
/// Unknown keys are ignored. This performs parsing only —
/// it does **not** apply defaults or validate; call
/// [`Config::validate`](Config::validate) and
/// [`Config::apply_defaults`](Config::apply_defaults) as separate phases.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    crate::io_util::read_yaml_file(path.as_ref(), "config")
}
