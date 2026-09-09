//! Integration tests for [`crate::preset`] against the verbatim fixture.
//!
//! The fixture `tests/data/config.default.yaml` is a byte-for-byte copy of the
//! recorded default preset (see `tests/data/README.md`). These tests assert that
//! every section of that file parses and that its values match, which is
//! acceptance criterion (a) for task 1.1; the defaulting test covers criterion
//! (b) of task 1.2.
//!
//! Also hosts the tests relocated from the inline `#[cfg(test)]` module in
//! `src/preset.rs` (change `test-hygiene-phase-2`, task 2.1): they exercise only
//! the public API, so names and assertions are carried over verbatim and the
//! move changes no behavior.

// Test target: expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::onnx::ModelInfo;
use config::{Config, ConfigError, OnnxConfig, load};
// Pull in the enum variants and nested struct names used by the assertions below.
use config::preset::*;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/config.default.yaml")
}

/// Assert two `f64` values are equal within a tiny epsilon (avoids brittle bit-exact
/// float comparisons across the YAML parser and Rust literals).
fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "expected ~{b}, got {a}");
}

#[test]
fn fixture_parses_all_sections_with_expected_values() {
    let cfg: Config = load(fixture()).expect("config.default.yaml must parse and be valid YAML");

    // database --------------------------------------------------------------
    // The verbatim fixture still carries `database.path`, but the
    // field is gone (revision 1.1: the DB path is derived from
    // workspace_dir + dataset.name, not configurable) — the key is now
    // ignored like any unknown key.
    assert_eq!(
        cfg.database.pragma.get("mmap_size").map(String::as_str),
        Some("268435456")
    );
    assert_eq!(
        cfg.database.pragma.get("journal_mode").map(String::as_str),
        Some("WAL")
    );
    assert_eq!(
        cfg.database.pragma.get("synchronous").map(String::as_str),
        Some("NORMAL")
    );
    assert_eq!(
        cfg.database.pragma.get("cache_size").map(String::as_str),
        Some("-64000")
    );

    // embeddings ------------------------------------------------------------
    assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Local);
    assert!(!cfg.embeddings.auto_rebuild_vectors);
    assert_eq!(cfg.embeddings.local.model_name, "bge-small-en-v1.5");
    assert_eq!(cfg.embeddings.api.base_url, "http://localhost:1234/v1");
    assert_eq!(cfg.embeddings.api.model_name, "text-embedding-3-large");
    assert_eq!(cfg.embeddings.api.max_retries, 3);
    assert_eq!(cfg.embeddings.api.timeout_ms, 30_000);

    // ingestion.chunking ----------------------------------------------------
    let md = &cfg.ingestion.chunking.markdown;
    assert_eq!(md.strategy, ChunkingStrategy::Hybrid);
    assert_eq!(md.max_chunk_size, 8192);
    assert_eq!(md.overlap_size, 100);
    assert_eq!(md.min_section_size, 500);

    let js = &cfg.ingestion.chunking.json;
    assert_eq!(js.text_fields, vec!["description", "title", "details"]);
    assert!(js.combine_fields);
    assert_eq!(js.max_objects, 0); // absent in the fixture -> zero value (1.2 applies defaults)

    // ingestion.ner.llm -----------------------------------------------------
    let llm = &cfg.ingestion.ner.llm;
    assert_eq!(llm.api_base_url, "http://192.168.1.7:1234/v1");
    assert_eq!(llm.model_name, "openai/gpt-oss-20b");
    assert_eq!(llm.api_key, "sk-none");
    approx(llm.temperature, 0.01);
    assert_eq!(llm.seed, 12);
    assert_eq!(llm.max_tokens, 4096);
    assert_eq!(llm.response_format, ResponseFormat::JsonSchema);
    assert_eq!(llm.timeout_ms, 120_000);
    assert_eq!(llm.max_retries, 3);

    // ingestion.batch_size / max_retries / resolver --------------------------
    assert_eq!(cfg.ingestion.batch_size, 100);
    assert_eq!(cfg.ingestion.max_retries, 3); // document-jobs-queue 1.2
    approx(cfg.ingestion.resolver.similarity_threshold, 0.85);

    // linker ----------------------------------------------------------------
    assert!(!cfg.linker.disabled);
    assert_eq!(cfg.linker.llm.api_base_url, "http://192.168.1.7:1234/v1");
    assert_eq!(cfg.linker.llm.model_name, "openai/gpt-oss-20b");
    assert_eq!(cfg.linker.llm.max_tokens, 2048);
    assert_eq!(cfg.linker.llm.response_format, ResponseFormat::JsonSchema);

    // search ----------------------------------------------------------------
    let s = &cfg.search;
    assert_eq!(s.rrf_k, 20);
    assert_eq!(s.lexical_top_k, 20);
    assert_eq!(s.semantic_top_k, 20);
    assert_eq!(s.final_top_k, 10);
    assert!(s.enable_lexical && s.enable_semantic);
    assert_eq!(s.timeout_ms, 10_000);
    approx(s.deprecated_boost, 0.2);
    approx(s.official_boost, 1.5);
    approx(s.recent_boost, 1.2);
    assert_eq!(s.recent_days, 90);
    approx(*s.authority_boost.get("policy").expect("policy"), 1.5);
    approx(
        *s.authority_boost.get("regulation").expect("regulation"),
        1.3,
    );
    approx(*s.authority_boost.get("default").expect("default"), 1.0);
    assert_eq!(s.authority_boost.len(), 3);

    // graph -----------------------------------------------------------------
    let g = &cfg.graph;
    assert!(g.enable_graph && g.load_on_startup);
    assert_eq!(g.max_depth, 5);
    assert_eq!(g.max_nodes, 1000);

    // auto_update (present in the fixture -> Some) --------------------------
    let au = cfg
        .auto_update
        .as_ref()
        .expect("auto_update section present");
    assert!(au.enabled && au.watch_sources && au.initial_sync);
    assert_eq!(au.debounce_seconds, 1);
    // retry_failed (document-jobs-queue 1.2, explicit in the fixture).
    assert!(au.retry_failed.enabled);
    assert_eq!(au.retry_failed.poll_interval_seconds, 60);

    // scheduler -------------------------------------------------------------
    let job = cfg
        .scheduler
        .jobs
        .get("orphan_cleanup")
        .expect("orphan_cleanup job");
    assert!(job.enabled);
    assert_eq!(job.interval_seconds, 300);
    assert_eq!(cfg.scheduler.jobs.len(), 1);

    // logging ---------------------------------------------------------------
    assert_eq!(cfg.logging.level, LogLevel::Debug);
    assert_eq!(cfg.logging.format, LogFormat::Console);
    assert_eq!(cfg.logging.output, LogOutput::Stderr);

    // paths -----------------------------------------------------------------
    // The verbatim fixture predates the storage-layout-restructure: its
    // `data_dir` / `documents_dir` / `global_config_path` keys are now ignored
    // (unknown-key tolerance), so the fields resolve to their new
    // defaults.
    let p = &cfg.paths;
    assert_eq!(p.workspace_dir, "workspace");
    assert_eq!(p.migrations_dir, "migrations");
    // Explicit fixture value survives: `prompts_path` still exists in the new
    // schema, so the fixture's "configs/prompts" is respected.
    assert_eq!(p.prompts_path, "configs/prompts");
    // Absent in the fixture -> normalized to its default at parse time (D12).
    assert_eq!(p.onnx_config, "workspace/configs/onnx.yaml");

    // dataset (absent in the fixture -> default) -----------------------------
    // No dataset by default (revision 1.1): empty name means "no data".
    assert_eq!(cfg.dataset.name, "");

    // server ----------------------------------------------------------------
    let sv = &cfg.server;
    assert_eq!(sv.name, "synopsis");
    assert_eq!(sv.version, "0.1.0-dev");
    assert_eq!(sv.host, "0.0.0.0");
    assert_eq!(sv.port, 8080);

    // vectors (additive section, design D7) ---------------------------------
    // The verbatim fixture predates the section: it parses fine and the
    // section stays absent, resolving to the ADR 0003 defaults.
    assert!(cfg.vectors.is_none());
    assert_eq!(cfg.vectors_config().dim, 1024);
    assert_eq!(cfg.vectors_config().ef_search, 200);

    // The whole document is internally consistent for its (local) embeddings mode.
    cfg.validate()
        .expect("fixture must pass validation in local mode");
}

#[test]
fn fixture_load_returns_error_for_missing_file() {
    // `load` maps a missing path to ConfigError::Io carrying the path.
    let missing =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/definitely-missing.yaml");
    match load(missing) {
        Err(config::ConfigError::Io { path, .. }) => {
            assert!(path.ends_with("definitely-missing.yaml"))
        }
        other => panic!("expected Io error for missing file, got: {other:?}"),
    }
}

#[test]
fn apply_defaults_does_not_change_explicit_fixture_values() {
    // Task 1.2 criterion (b): every value explicitly set in the verbatim fixture
    // survives defaulting; only fields absent from the file get filled in.
    let mut cfg: Config = load(fixture()).expect("config.default.yaml must parse");
    cfg.apply_defaults();

    // Explicit values that differ from the defaults (survival checks).
    // (The fixture's `database.path` no longer exists in the schema —
    // revision 1.1 — so it has nothing to survive.)
    assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Local);
    assert_eq!(
        cfg.embeddings.local.model_name,
        "bge-small-en-v1.5" // explicit fixture value survives defaulting
    );
    let md = &cfg.ingestion.chunking.markdown;
    assert_eq!(md.strategy, ChunkingStrategy::Hybrid);
    assert_eq!(md.max_chunk_size, 8192);
    assert_eq!(md.overlap_size, 100);
    assert_eq!(
        cfg.ingestion.chunking.json.text_fields,
        vec!["description", "title", "details"] // not the default field list
    );
    approx(cfg.ingestion.resolver.similarity_threshold, 0.85); // > 0 -> preserved

    let llm = &cfg.ingestion.ner.llm;
    assert_eq!(llm.response_format, ResponseFormat::JsonSchema);
    assert_eq!(llm.timeout_ms, 120_000); // not reset to 60000
    assert_eq!(cfg.linker.llm.timeout_ms, 120_000);

    let s = &cfg.search;
    assert_eq!(s.rrf_k, 20);
    approx(*s.authority_boost.get("policy").expect("policy"), 1.5);
    assert_eq!(s.authority_boost.len(), 3); // non-empty map -> no "default" insertion

    let au = cfg
        .auto_update
        .as_ref()
        .expect("auto_update present in fixture");
    assert!(au.enabled && au.initial_sync);
    assert_eq!(au.debounce_seconds, 1); // explicit 1s debounce survives (not reset to 30)

    let job = cfg
        .scheduler
        .jobs
        .get("orphan_cleanup")
        .expect("orphan_cleanup job");
    assert!(job.enabled);
    assert_eq!(job.interval_seconds, 300); // explicit interval respected (not 3600)
    assert_eq!(cfg.scheduler.jobs.len(), 1);

    assert_eq!(cfg.logging.level, LogLevel::Debug); // not reset to info
    let p = &cfg.paths;
    // The fixture's `data_dir` key is ignored by the new schema -> default.
    assert_eq!(p.workspace_dir, "workspace");
    // Fields ABSENT from the fixture get their defaults:
    assert_eq!(p.onnx_config, "workspace/configs/onnx.yaml");

    // Derived helpers: the knowledge DB is dataset-derived (the fixture has
    // no `dataset:` section, so the default empty name yields the degenerate
    // form — revision 1.1), while the cache is global under workspace_dir
    // (storage-layout-restructure D1).
    assert_eq!(
        cfg.dataset.db_path(&cfg.paths.workspace_dir),
        PathBuf::from("workspace/datasets/state/db/knowledge.db")
    );
    assert_eq!(
        cfg.cache_db_path(),
        PathBuf::from("workspace/db/cache/cache.db")
    );
}

// ── Relocated from src/preset.rs (test-hygiene-phase-2 task 2.1) ────────────

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
    // Parity (criterion d): `mode` parses as a plain value, so a bogus
    // mode does NOT fail parsing; it is rejected by `validate()` with the
    // expected message — a ConfigError::Validation, not a YAML/parse error.
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
fn validate_local_mode_has_no_field_rules() {
    // Local mode has no per-field validation rules: an empty model_name
    // selects the registry default (models.default from onnx.yaml, resolved
    // at resolution time — not filled by apply_defaults), and the dimension
    // comes from the registry entry (resolved_vector_dim).
    let cfg = Config::default(); // mode == Local, empty model_name
    assert!(cfg.validate().is_ok());
}

#[test]
fn removed_embedding_keys_are_ignored_on_parse() {
    // A YAML preset that still sets the removed keys (vector_dim,
    // model_path, tokenizer_path) under embeddings.local parses
    // successfully — unknown keys are ignored (the "Removed keys ignored"
    // scenario from the config-format spec delta).
    let cfg = parse(
        r#"
embeddings:
  mode: local
  local:
    model_name: bge-m3-int8
    model_path: /old/path/model.onnx
    tokenizer_path: /old/path/tokenizer.json
    vector_dim: 1024
  api:
    base_url: http://localhost:11434/v1
    model_name: text-embedding-3-large
    vector_dim: 3072
"#,
    );
    assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Local);
    assert_eq!(cfg.embeddings.local.model_name, "bge-m3-int8");
    assert_eq!(cfg.embeddings.api.base_url, "http://localhost:11434/v1");
    assert!(cfg.validate().is_ok());
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
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_api_rejects_missing_model_name() {
    let mut cfg = Config::default();
    cfg.embeddings.mode = EmbeddingsMode::Api;
    cfg.embeddings.api.base_url = "http://localhost:11434/v1".into();
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
fn empty_config_gets_all_defaults() {
    // Criterion (a): Config::default() after apply_defaults, checked against
    // every documented default.
    let mut cfg = Config::default();
    cfg.apply_defaults();

    // Ingestion.
    assert_eq!(cfg.ingestion.batch_size, 100);
    assert_eq!(cfg.ingestion.max_retries, 3); // document-jobs-queue 1.2
    let md = &cfg.ingestion.chunking.markdown;
    assert_eq!(md.strategy, ChunkingStrategy::Headers);
    assert_eq!(md.max_chunk_size, 1000);
    // A zero overlap is preserved, not defaulted to 100 (the check is `< 0`).
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
    // Both legs absent -> both force-enabled (only when BOTH are false).
    assert!(s.enable_lexical && s.enable_semantic);
    assert_eq!(s.timeout_ms, 10_000);
    assert_eq!(s.deprecated_boost, 0.2);
    assert_eq!(s.official_boost, 1.5);
    assert_eq!(s.recent_boost, 1.2);
    assert_eq!(s.recent_days, 90);
    assert_eq!(s.authority_boost.len(), 1);
    assert_eq!(s.authority_boost.get("default"), Some(&1.0));

    // Graph: presence semantics (design D13) — absent keys mean the documented
    // default true.
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
    // Retry sweep defaults (document-jobs-queue 1.2).
    assert!(au.retry_failed.enabled);
    assert_eq!(au.retry_failed.poll_interval_seconds, 60);

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

    // NER LLM defaults; linker.llm must stay untouched (defaults apply to the
    // NER provider only).
    let llm = &cfg.ingestion.ner.llm;
    assert_eq!(llm.response_format, ResponseFormat::JsonObject);
    assert_eq!(llm.timeout_ms, 60_000);
    assert_eq!(llm.max_retries, 3);
    assert_eq!(cfg.linker.llm.timeout_ms, 0); // not defaulted
    assert_eq!(cfg.linker.llm.max_retries, 0);

    // Resolver threshold.
    assert_eq!(cfg.ingestion.resolver.similarity_threshold, 0.8);
    // An empty model_name stays empty after apply_defaults: the registry
    // models.default is applied at resolution time, not at defaulting time
    // (registry-as-model-source-of-truth).
    assert_eq!(cfg.embeddings.local.model_name, "");

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
  enable_lexical: false   # single leg off -> respected (defaults flip only when BOTH are off)
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

    assert_eq!(cfg.embeddings.local.model_name, "custom-model"); // explicit value preserved

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
    // the force-enable is skipped for present sections.
    let mut cfg = parse("auto_update:\n  enabled: false\n");
    cfg.apply_defaults();
    let au = cfg.auto_update.expect("present section survives as Some");
    assert!(!au.enabled);
    assert!(!au.initial_sync); // not force-enabled either (the section was present)
    assert_eq!(au.debounce_seconds, 30); // field defaults still apply inside it
    assert!(au.watch_sources);
}

// ── retry config (document-jobs-queue task 1.2) ─────────────────────────

#[test]
fn retry_fields_default_when_keys_absent() {
    // Backward compatibility: a preset that predates the retry keys parses and
    // gets the documented defaults — max_retries=3, retry sweep enabled with a
    // 60 s poll interval — both when the whole sections are missing and when
    // the section is present but the keys are not.
    let mut cfg = parse("server:\n  name: x\n");
    // Absent `ingestion:` section -> zero value; apply_defaults fills it.
    cfg.apply_defaults();
    assert_eq!(cfg.ingestion.max_retries, 3);
    let au = cfg.auto_update.expect("absent auto_update becomes Some");
    assert!(au.retry_failed.enabled);
    assert_eq!(au.retry_failed.poll_interval_seconds, 60);

    // Keys absent inside present sections: serde defaults at parse time.
    let partial = parse(
        r#"
ingestion:
  batch_size: 100
auto_update:
  enabled: true
"#,
    );
    assert_eq!(partial.ingestion.max_retries, 3);
    let au = partial.auto_update.expect("present section");
    assert!(au.retry_failed.enabled);
    assert_eq!(au.retry_failed.poll_interval_seconds, 60);
}

#[test]
fn retry_failed_partial_map_fills_missing_field() {
    // A present `retry_failed:` map with only one key: the missing key gets its
    // per-field serde default (no zero poll interval from a partial map).
    let cfg = parse("auto_update:\n  retry_failed:\n    enabled: false\n");
    let rf = &cfg.auto_update.expect("present section").retry_failed;
    assert!(!rf.enabled); // explicit false respected (design D13)
    assert_eq!(rf.poll_interval_seconds, 60); // absent field -> default
}

#[test]
fn retry_fields_explicit_values_respected() {
    let mut cfg = parse(
        r#"
ingestion:
  max_retries: 5
auto_update:
  retry_failed:
    enabled: false
    poll_interval_seconds: 120
"#,
    );
    cfg.apply_defaults();
    assert_eq!(cfg.ingestion.max_retries, 5); // not reset to 3
    let rf = &cfg.auto_update.expect("present section").retry_failed;
    assert!(!rf.enabled);
    assert_eq!(rf.poll_interval_seconds, 120); // not reset to 60
}

#[test]
fn retry_fields_non_positive_values_get_numeric_fallback() {
    // A configured 0/negative on the numeric fields is treated as unset
    // (same rule as `batch_size` / `debounce_seconds`).
    let mut cfg = parse(
        r#"
ingestion:
  max_retries: 0
auto_update:
  retry_failed:
    poll_interval_seconds: -5
"#,
    );
    cfg.apply_defaults();
    assert_eq!(cfg.ingestion.max_retries, 3);
    assert_eq!(
        cfg.auto_update
            .expect("present section")
            .retry_failed
            .poll_interval_seconds,
        60
    );
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
    // add-usearch-ann-engine task 1.5: the engine-tagged subdirectory
    // (usearch is the only engine).
    assert_eq!(
        dataset.vectors_engine_path("ws", "usearch"),
        PathBuf::from("ws").join("datasets/edtech/state/vectors/usearch")
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

// (The old `vector_dim()` accessor was removed in task 1.5; the registry
// resolver `resolved_vector_dim` is tested below.)

// ── resolved_vector_dim (registry-as-model-source-of-truth task 1.1) ─────

/// Builds an in-memory `onnx.yaml` registry fixture: the given
/// `(name, vector_dim)` entries plus the given `models.default`.
fn onnx_registry(default: &str, entries: &[(&str, i32)]) -> OnnxConfig {
    let mut onnx = OnnxConfig::default();
    onnx.models.default = default.to_string();
    onnx.models.entries = entries
        .iter()
        .map(|(name, dim)| ModelInfo {
            name: (*name).to_string(),
            vector_dim: *dim,
            ..Default::default()
        })
        .collect();
    onnx
}

#[test]
fn resolved_vector_dim_uses_registry_entry_for_named_model() {
    // The named model's registry entry is the dimension source.
    let mut cfg = Config::default(); // mode == Local by the enum default
    cfg.embeddings.local.model_name = "bge-small-en-v1.5".into();
    let onnx = onnx_registry(
        "bge-m3-int8",
        &[("bge-m3-int8", 1024), ("bge-small-en-v1.5", 384)],
    );
    assert_eq!(
        cfg.resolved_vector_dim(&onnx)
            .expect("registry entry exists"),
        384
    );
}

#[test]
fn resolved_vector_dim_empty_name_uses_registry_default() {
    // An empty model_name selects the registry's models.default entry.
    let cfg = Config::default(); // local mode, empty model_name
    let onnx = onnx_registry("bge-m3-int8", &[("bge-m3-int8", 1024), ("other", 384)]);
    assert_eq!(
        cfg.resolved_vector_dim(&onnx)
            .expect("default entry exists"),
        1024
    );
}

#[test]
fn resolved_vector_dim_empty_name_after_apply_defaults_uses_registry_default() {
    // End to end (defaulting + resolver, no ONNX runtime): an empty
    // model_name survives apply_defaults and resolves to the registry
    // models.default entry's vector_dim — the shipped default (bge-small-
    // en-v1.5) comes from the registry, not a hardcoded fallback
    // (registry-as-model-source-of-truth).
    let mut cfg = Config::default(); // local mode, empty model_name
    cfg.apply_defaults();
    assert!(
        cfg.embeddings.local.model_name.is_empty(),
        "apply_defaults must not fill the model name"
    );
    let onnx = onnx_registry(
        "bge-small-en-v1.5",
        &[("bge-m3-int8", 1024), ("bge-small-en-v1.5", 384)],
    );
    assert_eq!(
        cfg.resolved_vector_dim(&onnx)
            .expect("default entry exists"),
        384
    );
}

#[test]
fn resolved_vector_dim_unknown_model_is_validation_error() {
    let mut cfg = Config::default();
    cfg.embeddings.local.model_name = "not-in-registry".into();
    let onnx = onnx_registry("bge-m3-int8", &[("bge-m3-int8", 1024)]);
    match cfg.resolved_vector_dim(&onnx) {
        Err(ConfigError::Validation { message }) => {
            assert!(
                message.contains("not-in-registry"),
                "the error must name the model: {message}"
            );
        }
        other => panic!("expected a Validation error, got: {other:?}"),
    }
}

#[test]
fn resolved_vector_dim_non_positive_entry_dim_is_validation_error() {
    for dim in [0, -1] {
        let mut cfg = Config::default();
        cfg.embeddings.local.model_name = "bad-dim".into();
        let onnx = onnx_registry("bad-dim", &[("bad-dim", dim)]);
        match cfg.resolved_vector_dim(&onnx) {
            Err(ConfigError::Validation { message }) => {
                assert!(
                    message.contains("bad-dim"),
                    "the error must name the model: {message}"
                );
            }
            other => {
                panic!("expected a Validation error for vector_dim {dim}, got: {other:?}")
            }
        }
    }
}

#[test]
fn resolved_vector_dim_empty_name_without_registry_default_is_validation_error() {
    // Empty model_name + empty models.default: nothing to resolve.
    let cfg = Config::default(); // local mode, empty model_name
    let onnx = onnx_registry("", &[]);
    match cfg.resolved_vector_dim(&onnx) {
        Err(ConfigError::Validation { message }) => {
            assert!(
                message.contains("model_name"),
                "the error must name the missing field: {message}"
            );
        }
        other => panic!("expected a Validation error, got: {other:?}"),
    }
}

#[test]
fn resolved_vector_dim_api_mode_returns_zero() {
    // Api mode is unsupported by the build; the dimension is not resolvable
    // without the removed config field, so the resolver returns Ok(0).
    let mut cfg = Config::default();
    cfg.embeddings.mode = EmbeddingsMode::Api;
    let onnx = onnx_registry("", &[]);
    assert_eq!(
        cfg.resolved_vector_dim(&onnx).expect("api mode -> Ok(0)"),
        0
    );
}

#[test]
fn resolved_vector_dim_unrecognized_mode_is_zero() {
    // As the legacy vector_dim() accessor does.
    let cfg = parse("embeddings:\n  mode: bogus\n");
    assert_eq!(
        cfg.resolved_vector_dim(&OnnxConfig::default())
            .expect("unknown mode -> Ok(0)"),
        0
    );
}

#[test]
fn orphan_cleanup_explicit_enabled_keeps_default_interval() {
    // Criterion (f) / "explicitly enabled job preserved":
    // {enabled: true} -> interval still defaults to 3600.
    let mut cfg = parse("scheduler:\n  jobs:\n    orphan_cleanup:\n      enabled: true\n");
    cfg.apply_defaults();
    let oc = cfg.scheduler.jobs["orphan_cleanup"];
    assert!(oc.enabled);
    assert_eq!(oc.interval_seconds, 3600);
}

#[test]
fn orphan_cleanup_explicit_interval_respected() {
    // Criterion (f) / "explicitly configured interval preserved".
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
    // Criterion (f) / "explicitly disabled with custom interval preserved".
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
    // Design D13: an explicit false survives defaulting — a naive
    // `if !x { x = true }` pattern would make these settings non-functional.
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
fn absent_sections_default_to_expected_values_at_parse_time() {
    // Design D12: a whole section missing from YAML yields the struct's Default —
    // serde does not run per-field attributes on an absent struct.
    // The fixture's `database.path` key no longer exists in the schema
    // (revision 1.1): it is ignored like any unknown key.
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
    // EmbeddingsMode has no default, so it is NOT one of D12's normalized
    // enums: "" stays Unknown("") and validate() rejects it with the expected
    // message, like any other unrecognized mode.
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

    // Zero value stays zero (the check is `< 0`, not `<= 0`).
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
            engine: None,
            quantization: "bf16".to_string(),
            usearch: None,
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
fn vectors_engine_field_parses_valid_values() {
    // usearch is the only accepted engine value (design D2 of the
    // engine-removal change).
    let cfg = parse("vectors:\n  engine: usearch\n");
    assert_eq!(cfg.vectors_config().engine.as_deref(), Some("usearch"));
}

#[test]
fn vectors_engine_field_removed_engine_is_rejected() {
    // The removed engine is rejected with an explicit, actionable error
    // naming the removal and pointing to usearch (design D2) — a loud
    // parse failure instead of a silent engine swap.
    let err = load_from_str("vectors:\n  engine: lance\n")
        .expect_err("the removed engine must fail the parse");
    match err {
        ConfigError::Yaml { .. } => {
            assert!(
                err.to_string()
                    .contains("the \"lance\" engine was removed; the only engine is \"usearch\""),
                "the error must name the removal and point to usearch: {err}"
            );
        }
        other => panic!("expected a YAML parse error, got: {other:?}"),
    }
}

#[test]
fn vectors_engine_field_absent_stays_none() {
    // Absent key → None; the wiring (vectors::create_vector_engine)
    // resolves the default engine ("usearch") — backward compatible with
    // presets written before the field.
    let cfg = parse("vectors:\n  dim: 512\n");
    assert_eq!(cfg.vectors_config().engine, None);

    let cfg = parse("server:\n  name: x\n");
    assert_eq!(cfg.vectors_config().engine, None);
}

#[test]
fn vectors_engine_field_invalid_is_a_parse_error() {
    let err = load_from_str("vectors:\n  engine: foo\n")
        .expect_err("an invalid engine must fail the parse");
    match err {
        ConfigError::Yaml { .. } => {
            assert!(
                err.to_string().contains("vectors.engine"),
                "the error must name the field: {err}"
            );
        }
        other => panic!("expected a YAML parse error, got: {other:?}"),
    }
}

#[test]
fn vectors_engine_field_roundtrip() {
    // A present engine value survives serialize -> reparse.
    let cfg = parse("vectors:\n  dim: 512\n  engine: usearch\n");
    let yaml = noyalib::to_string(&cfg).expect("serialize");
    let back: Config = noyalib::from_str(&yaml).expect("reparse");
    assert_eq!(back.vectors, cfg.vectors);
}

#[test]
fn vectors_quantization_field_parses_valid_values() {
    for value in ["u8", "i8", "f16", "bf16", "f32"] {
        let cfg = parse(&format!("vectors:\n  quantization: {value}\n"));
        assert_eq!(cfg.vectors_config().quantization, value);
    }
}

#[test]
fn vectors_quantization_field_default_is_bf16() {
    // Absent key inside a present section -> default "bf16".
    let cfg = parse("vectors:\n  dim: 512\n");
    assert_eq!(cfg.vectors_config().quantization, "bf16");

    // Absent section -> default "bf16" via vectors_config().
    let cfg = parse("server:\n  name: x\n");
    assert_eq!(cfg.vectors_config().quantization, "bf16");

    // The zero-value config resolves the same way.
    assert_eq!(VectorsConfig::default().quantization, "bf16");
}

#[test]
fn vectors_quantization_field_is_case_insensitive() {
    let cfg = parse("vectors:\n  quantization: BF16\n");
    assert_eq!(cfg.vectors_config().quantization, "bf16");
}

#[test]
fn vectors_quantization_field_invalid_is_a_parse_error() {
    let err = load_from_str("vectors:\n  quantization: fp8\n")
        .expect_err("an invalid quantization must fail the parse");
    match err {
        ConfigError::Yaml { .. } => {
            assert!(
                err.to_string().contains("vectors.quantization"),
                "the error must name the field: {err}"
            );
        }
        other => panic!("expected a YAML parse error, got: {other:?}"),
    }
}

#[test]
fn vectors_quantization_field_roundtrip() {
    // A present quantization value survives serialize -> reparse.
    let cfg = parse("vectors:\n  dim: 512\n  quantization: f32\n");
    let yaml = noyalib::to_string(&cfg).expect("serialize");
    let back: Config = noyalib::from_str(&yaml).expect("reparse");
    assert_eq!(back.vectors, cfg.vectors);
    assert_eq!(back.vectors_config().quantization, "f32");
}

// ── vectors.usearch section (usearch-wal-persistence task 2.1) ────────

#[test]
fn vectors_usearch_section_parses_explicit_values() {
    let cfg = parse("vectors:\n  usearch:\n    max_segment_vectors: 500000\n");
    assert_eq!(
        cfg.vectors_config().usearch,
        Some(UsearchConfig {
            max_segment_vectors: 500000,
            compaction_stale_threshold: 30,
            search_threads: 4,
        })
    );
}

#[test]
fn vectors_usearch_section_defaults_when_absent() {
    // Absent `usearch` key (present or absent section) -> None; the
    // engine resolves UsearchConfig::default (1M / 30% / 4 threads).
    let cfg = parse("vectors:\n  dim: 512\n");
    assert!(cfg.vectors_config().usearch.is_none());

    let cfg = parse("server:\n  name: x\n");
    assert!(cfg.vectors_config().usearch.is_none());

    let defaults = UsearchConfig::default();
    assert_eq!(defaults.max_segment_vectors, 1_000_000);
    assert_eq!(defaults.compaction_stale_threshold, 30);
    assert_eq!(defaults.search_threads, 4);
}

#[test]
fn vectors_usearch_section_partial_override_fills_defaults() {
    let cfg = parse("vectors:\n  usearch:\n    compaction_stale_threshold: 50\n");
    assert_eq!(
        cfg.vectors_config().usearch,
        Some(UsearchConfig {
            max_segment_vectors: 1_000_000,
            compaction_stale_threshold: 50,
            search_threads: 4,
        })
    );
}

#[test]
fn vectors_usearch_section_invalid_values_are_rejected() {
    for yaml in [
        "vectors:\n  usearch:\n    max_segment_vectors: 0\n",
        "vectors:\n  usearch:\n    compaction_stale_threshold: 0\n",
        "vectors:\n  usearch:\n    compaction_stale_threshold: 150\n",
        "vectors:\n  usearch:\n    search_threads: 0\n",
    ] {
        let err = load_from_str(yaml)
            .expect_err(&format!("an invalid value must fail the parse: {yaml}"));
        match err {
            ConfigError::Yaml { .. } => {
                // The 1..=100 range and the > 0 rules are ours; the
                // out-of-u8-range case (150) is rejected by the YAML
                // layer itself. Both are parse errors.
                assert!(
                    err.to_string().contains("vectors.usearch") || err.to_string().contains("u8"),
                    "the error must name the field or the offending type: {err}"
                );
            }
            other => panic!("expected a YAML parse error, got: {other:?}"),
        }
    }
}

#[test]
fn vectors_usearch_section_roundtrip() {
    // A present usearch section survives serialize -> reparse.
    let cfg =
        parse("vectors:\n  usearch:\n    max_segment_vectors: 500000\n    search_threads: 8\n");
    let yaml = noyalib::to_string(&cfg).expect("serialize");
    let back: Config = noyalib::from_str(&yaml).expect("reparse");
    assert_eq!(back.vectors, cfg.vectors);
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
