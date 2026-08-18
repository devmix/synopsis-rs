//! Main YAML preset: typed [`Config`] structures plus `load` / `validate`.
//!
//! This module mirrors the Go oracle's `internal/config/config.go` (the
//! `Load` + `Validate` half; `ApplyDefaults` and the path helpers arrive in
//! task 1.2). Field names, YAML keys and validation semantics are kept
//! faithful to the oracle so behaviour is byte-compatible with the original.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

// ── Tolerant-enum machinery (defined before first use) ────────────────────

/// Declares a tolerant string enum (design D7): known values map to named
/// variants, any other value is captured verbatim in `Unknown(String)` instead
/// of failing the parse. Serializes back to its canonical lowercase word (or the
/// original text for unknowns), so round-tripping preserves intent. The second
/// token names the variant used by [`Default`], chosen as that field's oracle
/// default so an absent YAML key deserializes straight to the intended value.
macro_rules! tolerant_enum {
    ($(#[$doc:meta])* $name:ident, $default:ident { $($variant:ident => $value:expr),* $(,)? }) => {
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
                // Zero-value equals this field's oracle default; task 1.2's
                // apply_defaults leaves it untouched (exact Go "absent -> default").
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
                // Case-insensitive match on known words (oracle uses lowercase);
                // unknown values are preserved verbatim for diagnostics.
                Ok(match raw.to_ascii_lowercase().as_str() {
                    $($value => Self::$variant,)*
                    other => Self::Unknown(other.to_string()),
                })
            }
        }
    };
}

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
}

/// Builds a [`ConfigError::Validation`] from a message.
fn validation(message: &str) -> ConfigError {
    ConfigError::Validation {
        message: message.to_string(),
    }
}

// ── Database ──────────────────────────────────────────────────────────────

/// SQLite storage settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Path to the main SQLite database file.
    pub path: String,
    /// Explicit cache DB path (overrides the derived default).
    pub cache_path: String,
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
    /// preserved without ever failing.
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
    /// Chunking strategy for the markdown chunker (design D7: tolerant).
    ChunkingStrategy, Headers {
        Headers => "headers",
        Fixed   => "fixed",
        Hybrid  => "hybrid",
    }
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
    /// LLM structured-output format (design D7: tolerant).
    ResponseFormat, JsonObject {
        JsonObject => "json_object",
        JsonSchema => "json_schema",
    }
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

// ── Graph / storage ───────────────────────────────────────────────────────

/// Knowledge-graph module settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Enable graph traversal in queries.
    pub enable_graph: bool,
    /// Maximum BFS depth.
    pub max_depth: i32,
    /// Maximum nodes returned per query.
    pub max_nodes: i32,
    /// Load the graph into memory on startup.
    pub load_on_startup: bool,
}

/// Automatic file-watching / re-indexing settings (serve mode).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUpdateConfig {
    /// Enable filesystem monitoring.
    pub enabled: bool,
    /// Minimum interval between re-indexings in seconds.
    pub debounce_seconds: i32,
    /// Watch all sources listed under ingestion.
    pub watch_sources: bool,
    /// Run a full source scan on startup.
    pub initial_sync: bool,
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
    /// rejected so an unrecognised value cannot break startup.
    LogLevel, Info {
        Trace => "trace",
        Debug => "debug",
        Info  => "info",
        Warn  => "warn",
        Error => "error",
    }
}

tolerant_enum! {
    /// Log output format (`"console"` or `"json"`; design D7: tolerant).
    LogFormat, Console {
        Console => "console",
        Json    => "json",
    }
}

tolerant_enum! {
    /// Log output destination (`"stderr"` or `"stdout"`; design D7: tolerant).
    LogOutput, Stderr {
        Stderr => "stderr",
        Stdout => "stdout",
    }
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Data directory for DB / cache.
    pub data_dir: String,
    /// Documents storage directory.
    pub documents_dir: String,
    /// Database migrations directory.
    pub migrations_dir: String,
    /// Ontology directory (contains `global.xml` and `domains/`).
    pub global_config_path: String,
    /// Prompt template files directory.
    pub prompts_path: String,
    /// Path to the external `onnx.yaml` registry file.
    pub onnx_config: String,
}

/// MCP server identification / bind settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Human-readable service name.
    pub name: String,
    /// Service version string.
    pub version: String,
    /// HTTP listen host.
    pub host: String,
    /// HTTP listen port.
    pub port: i32,
}

// ── Loading ───────────────────────────────────────────────────────────────

/// Reads and parses the YAML config at `path` into a [`Config`].
///
/// Unknown keys are ignored (matching the oracle). This performs parsing only —
/// it does **not** apply defaults or validate; call
/// [`Config::validate`](Config::validate) (and, from task 1.2, `apply_defaults`)
/// as separate phases.
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: display_path(path),
        source,
    })?;
    let text = String::from_utf8(bytes).map_err(|e| ConfigError::Yaml {
        path: display_path(path),
        source: noyalib::Error::Custom(format!("config file is not valid UTF-8: {e}")),
    })?;
    noyalib::from_str(&text).map_err(|source| ConfigError::Yaml {
        path: display_path(path),
        source,
    })
}

/// Renders a path for error messages (lossy is fine in diagnostics).
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
}
