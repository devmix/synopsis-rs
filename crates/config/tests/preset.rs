//! Integration tests for [`crate::preset`] against the verbatim oracle fixture.
//!
//! The fixture `tests/data/config.default.yaml` is copied byte-for-byte from
//! `../synopsis/configs/config.default.yaml` (see `tests/data/README.md`). These
//! tests assert that every section of that file parses and that its values match,
//! which is acceptance criterion (a) for task 1.1.

// Test target: expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::{Config, load};
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
    assert_eq!(cfg.database.path, "./data/knowledge.db");
    assert!(cfg.database.cache_path.is_empty());
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
    assert!(cfg.embeddings.local.model_path.is_empty());
    assert_eq!(cfg.embeddings.local.vector_dim, 384);
    assert_eq!(cfg.embeddings.api.base_url, "http://localhost:1234/v1");
    assert_eq!(cfg.embeddings.api.model_name, "text-embedding-3-large");
    assert_eq!(cfg.embeddings.api.vector_dim, 3072);
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

    // ingestion.ner.prose ---------------------------------------------------
    let prose = &cfg.ingestion.ner.prose;
    assert!(prose.enable_pos && prose.enable_ner);
    assert!(prose.custom_patterns.is_empty());
    assert!(prose.entity_types.is_empty());
    approx(prose.min_confidence, 0.5);
    approx(prose.location_min_confidence, 0.75);

    // ingestion.batch_size / resolver ---------------------------------------
    assert_eq!(cfg.ingestion.batch_size, 100);
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
    let p = &cfg.paths;
    assert_eq!(p.data_dir, "data");
    assert_eq!(p.documents_dir, "documents");
    assert_eq!(p.migrations_dir, "migrations");
    assert_eq!(p.global_config_path, "data/ontology");
    assert_eq!(p.prompts_path, "configs/prompts");
    assert!(p.onnx_config.is_empty()); // absent in the fixture

    // server ----------------------------------------------------------------
    let sv = &cfg.server;
    assert_eq!(sv.name, "synopsis");
    assert_eq!(sv.version, "0.1.0-dev");
    assert_eq!(sv.host, "0.0.0.0");
    assert_eq!(sv.port, 8080);

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
