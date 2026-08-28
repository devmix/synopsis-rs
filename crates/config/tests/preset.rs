//! Integration tests for [`crate::preset`] against the verbatim oracle fixture.
//!
//! The fixture `tests/data/config.default.yaml` is copied byte-for-byte from
//! `../synopsis/configs/config.default.yaml` (see `tests/data/README.md`). These
//! tests assert that every section of that file parses and that its values match,
//! which is acceptance criterion (a) for task 1.1; the defaulting test covers
//! criterion (b) of task 1.2.

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
    // The verbatim oracle fixture still carries `database.path`, but the
    // field is gone (revision 1.1: the DB path is derived from
    // workspace_dir + dataset.name, not configurable) — the key is now
    // ignored like any unknown key (Go parity).
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
    // The verbatim oracle fixture predates the storage-layout-restructure: its
    // `data_dir` / `documents_dir` / `global_config_path` keys are now ignored
    // (unknown-key tolerance, Go parity), so the fields resolve to their new
    // defaults.
    let p = &cfg.paths;
    assert_eq!(p.workspace_dir, "workspace");
    assert_eq!(p.migrations_dir, "migrations");
    // Explicit fixture value survives: `prompts_path` still exists in the new
    // schema, so the oracle's "configs/prompts" is respected.
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
    // The verbatim oracle fixture predates the section: it parses fine and the
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

    // Explicit values that differ from the oracle defaults (survival checks).
    // (The fixture's `database.path` no longer exists in the schema —
    // revision 1.1 — so it has nothing to survive.)
    assert_eq!(cfg.embeddings.mode, EmbeddingsMode::Local);
    assert_eq!(
        cfg.embeddings.local.model_name,
        "bge-small-en-v1.5" // not replaced by bge-m3-int8
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
