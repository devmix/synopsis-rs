//! `sync` subcommand body (design D8): a one-shot full re-index of all
//! configured sources, then exit.
//!
//! Oracle mapping: `../synopsis/cmd/app/sync.go` (`runSync`): bootstrap →
//! vector dimension-mismatch handling → runner → `IngestAll(rebuild)` →
//! stderr summary block. Exit 0 on success, non-zero on error (oracle
//! `log.Fatal` → exit 1).
//!
//! Reuses the serve-side collaborators (D3 bootstrap, the rebuild-clear
//! ingest wrappers, the serve engine-recreate helper) — no ingestion logic
//! lives here.
//!
//! # Dimension-mismatch handling
//!
//! The oracle's `rebuildVectorsIfNeeded` runs at migrate time (a vec0
//! virtual-table error) and re-embeds in place. Here the mismatch surfaces
//! from the ANN engine at open time, so the Rust form is: recreate the
//! engine (drop + create with the configured dimension) and force a full
//! re-ingest — the same helper the serve wiring uses. `--rebuild` takes the
//! same path: the oracle skips the check under `--rebuild` because the full
//! reset covers it, and the recreate is a strict subset of that reset.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ingestion::SummaryStats;
use vectors::VectorsError;

use crate::error::CliError;
use crate::serve::bootstrap::{self, Bootstrap};
use crate::serve::ingest;
use crate::serve::server;

/// One `sync` invocation: the effective config path plus the per-command
/// flags (clap `sync` subcommand).
pub struct SyncRequest {
    /// Resolved configuration file path.
    pub cfg_path: PathBuf,
    /// `--dataset` dataset name override (wins over `config.dataset.name`).
    pub dataset: Option<String>,
    /// `--rebuild`: clear all existing data before re-indexing.
    pub rebuild: bool,
    /// `--auto-rebuild-vectors` (OR-ed with the config flag).
    pub auto_rebuild_vectors: bool,
}

/// The `sync` subcommand entry point (design D8).
///
/// Bootstraps the application state, runs the sync flow, and maps the
/// outcome to the process exit code: 0 on success, non-zero on error
/// (oracle parity — every fatal error exits 1). Per-source ingestion
/// failures are reported in the summary, never in the exit code (oracle
/// parity: `runSync` returns nothing).
pub fn run_sync(req: &SyncRequest) -> ExitCode {
    let start = Instant::now();
    match sync_flow(req, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "sync failed");
            err.exit_code()
        }
    }
}

/// Bootstrap + sync flow (the production path of [`run_sync`]).
fn sync_flow(req: &SyncRequest, start: Instant) -> Result<(), CliError> {
    let mut boot = bootstrap::bootstrap(&req.cfg_path, req.dataset.as_deref())?;
    sync(&mut boot, req, start, &mut std::io::stderr())?;
    Ok(())
}

/// The sync flow (design D8) over an already-bootstrapped application
/// state — the seam the integration tests drive with a fake embedding
/// provider and a temp db + source (no ONNX runtime required).
///
/// `summary_out` receives the summary block (production: stderr).
///
/// # Errors
///
/// [`CliError`] from the vector engine, the runner assembly, the summary
/// write, or the fatal dimension-mismatch path (see module docs).
pub fn sync(
    boot: &mut Bootstrap,
    req: &SyncRequest,
    start: Instant,
    summary_out: &mut dyn Write,
) -> Result<SummaryStats, CliError> {
    // No-data semantics (design D2): without an active dataset there is no
    // ontology and no content to index — report an empty summary and exit 0
    // (the bootstrap already logged the warning).
    if !bootstrap::has_active_dataset(&boot.config) {
        let stats = SummaryStats::default();
        print_summary(summary_out, &stats, start.elapsed())?;
        return Ok(stats);
    }

    // CLI flag overrides config (oracle: autoRebuildVectorsCLI || cfg).
    let auto_rebuild = req.auto_rebuild_vectors || boot.config.embeddings.auto_rebuild_vectors;

    // Vector dimension-mismatch handling (oracle `rebuildVectorsIfNeeded`):
    // the mismatch surfaces from the ANN engine at open time; the fix is to
    // recreate the engine with the configured dimension and force a full
    // re-ingest below (the Rust form of the oracle's `ReEmbedChunks`).
    // Without auto-rebuild (and without `--rebuild`) it is fatal.
    let mut force_rebuild = false;
    loop {
        match bootstrap::open_vectors_engine(boot) {
            Ok(()) => break,
            Err(CliError::Vectors(VectorsError::DimensionMismatch { .. }))
                if auto_rebuild || req.rebuild =>
            {
                force_rebuild = true;
                server::recreate_vectors_engine(boot)?;
            }
            Err(CliError::Vectors(VectorsError::DimensionMismatch { .. })) => {
                return Err(CliError::Unsupported(
                    "vector dimension mismatch detected; run with --auto-rebuild-vectors \
                     (or set embeddings.auto_rebuild_vectors in config) to rebuild the \
                     vectors automatically, or delete the stored vector index"
                        .to_string(),
                ));
            }
            Err(err) => return Err(err),
        }
    }
    if force_rebuild {
        tracing::info!("vectors rebuilt due to dimension mismatch");
    }

    let runner = bootstrap::build_runner(boot)?;

    tracing::info!(rebuild = req.rebuild, "starting full sync");
    let stats = ingest::ingest_all(&runner, req.rebuild || force_rebuild);

    let duration = start.elapsed();
    print_summary(summary_out, &stats, duration)?;
    tracing::info!(
        sources = stats.sources_processed,
        documents_created = stats.documents_created,
        documents_updated = stats.documents_updated,
        documents_skipped = stats.documents_skipped,
        errors = stats.errors.len(),
        duration = %format_duration(duration),
        "sync finished"
    );
    Ok(stats)
}

/// Renders the sync summary block to `out` (oracle `runSync` stderr block:
/// sources processed, documents created/updated/skipped, errors — only when
/// non-empty — and the duration rounded to the nearest second).
///
/// # Errors
///
/// [`std::io::Error`] when `out` cannot be written.
pub(crate) fn print_summary(
    out: &mut dyn Write,
    stats: &SummaryStats,
    duration: Duration,
) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "=== Sync Summary ===")?;
    writeln!(out, "Sources processed: {}", stats.sources_processed)?;
    writeln!(out, "Documents created:  {}", stats.documents_created)?;
    writeln!(out, "Documents updated:  {}", stats.documents_updated)?;
    writeln!(out, "Documents skipped:  {}", stats.documents_skipped)?;
    if !stats.errors.is_empty() {
        writeln!(out, "Errors:            {}", stats.errors.len())?;
    }
    writeln!(out, "Duration:          {}", format_duration(duration))?;
    writeln!(out, "==========================")?;
    Ok(())
}

/// Formats an elapsed duration the way the oracle prints it: rounded to the
/// nearest whole second (Go `time.Duration.Round(time.Second)`, half away
/// from zero) and rendered in Go's `time.Duration` units (`1h2m3s`, `45s`,
/// `0s`).
fn format_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs() + u64::from(elapsed.subsec_nanos() >= 500_000_000);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    if out.is_empty() {
        out.push_str("0s");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use config::{Config, GlobalConfig};
    use db::{ChunkDao, ConnectionOrTx, DocumentDao, DocumentFilter};
    use embedding::{EmbeddingError, EmbeddingProvider};
    use vectors::{LanceEngine, VectorIndexConfig};

    use super::*;
    use crate::serve::bootstrap::{Bootstrap, open_db};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-sync-{tag}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    /// A deterministic embedding provider (fixed vectors of `dim`).
    struct FakeEmbed {
        dim: usize,
    }

    impl EmbeddingProvider for FakeEmbed {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(texts.iter().map(|_| vec![0.5f32; self.dim]).collect())
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            "fake"
        }
    }

    /// A 4-dim local-mode config (matching [`FakeEmbed`]), NER disabled.
    fn test_config(workspace_dir: &Path) -> Config {
        let mut config = Config {
            embeddings: config::preset::EmbeddingsConfig {
                mode: config::preset::EmbeddingsMode::Local,
                local: config::preset::LocalEmbedding {
                    model_name: "bge-m3-int8".to_string(),
                    model_path: String::new(),
                    tokenizer_path: String::new(),
                    vector_dim: 4,
                },
                api: Default::default(),
                auto_rebuild_vectors: false,
            },
            paths: config::preset::PathsConfig {
                workspace_dir: workspace_dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.ingestion.ner.disabled = true;
        config.apply_defaults();
        config
    }

    /// A minimal global config with one enabled markdown source at `src`.
    fn one_markdown_source(src: &Path) -> GlobalConfig {
        GlobalConfig {
            sources: vec![config::ontology::SourceConfig {
                path: src.to_string_lossy().into_owned(),
                source_type: config::ontology::SourceType::Markdown,
                disabled: false,
                space: String::new(),
                domains: vec!["default".to_string()],
                dataset: String::new(),
            }],
            cross_domain_links: None,
            ner: config::ontology::GlobalNerConfig {
                methods: Vec::new(),
            },
            entities: Vec::new(),
            relations: Vec::new(),
            extraction: Default::default(),
        }
    }

    /// A Bootstrap with a fake provider and a temp-file db; no assembled
    /// pipeline collaborators (the state right before `build_runner`).
    fn test_bootstrap(config: Config, global: Option<GlobalConfig>, db: db::Db) -> Bootstrap {
        Bootstrap {
            config,
            global,
            domains: HashMap::new(),
            db,
            cache: None,
            embed: Arc::new(FakeEmbed { dim: 4 }),
            onnx: config::OnnxConfig::default(),
            registry: None,
            prompts: None,
            vectors: None,
            dimension_mismatch: None,
        }
    }

    /// Sets up a temp dir with one markdown source document and a
    /// bootstrapped state ready for `sync` (no flags set).
    fn sync_fixture(tag: &str) -> (TempDir, Bootstrap, SyncRequest) {
        let dir = TempDir::new(tag);
        let src = dir.as_ref().join("src");
        std::fs::create_dir_all(&src).expect("create source dir");
        std::fs::write(
            src.join("doc.md"),
            "# Title\n\nBody text of the document.\n",
        )
        .expect("write source document");
        let mut config = test_config(&dir.as_ref().join("workspace"));
        // An active dataset (design D2): named + directory present, so the
        // sync flow ingests instead of short-circuiting with no data.
        config.dataset.name = "edtech".to_string();
        std::fs::create_dir_all(config.dataset.state_path(&config.paths.workspace_dir))
            .expect("create dataset state dir");
        let global = one_markdown_source(&src);
        let db = open_db(dir.as_ref().join("knowledge.db").as_path()).expect("open db");
        let boot = test_bootstrap(config, Some(global), db);
        let req = SyncRequest {
            cfg_path: dir.as_ref().join("unused.yaml").to_path_buf(),
            dataset: None,
            rebuild: false,
            auto_rebuild_vectors: false,
        };
        (dir, boot, req)
    }

    /// The persisted document count (the task's count check).
    fn doc_count(boot: &Bootstrap) -> i64 {
        boot.db
            .with_conn(|conn| {
                DocumentDao::new(ConnectionOrTx::Connection(conn)).count(&DocumentFilter::default())
            })
            .expect("with_conn documents")
            .expect("count documents")
    }

    /// The persisted chunk count.
    fn chunk_count(boot: &Bootstrap) -> i64 {
        boot.db
            .with_conn(|conn| ChunkDao::new(ConnectionOrTx::Connection(conn)).count())
            .expect("with_conn chunks")
            .expect("count chunks")
    }

    // --- sync: initial run ---------------------------------------------------

    #[test]
    fn sync_without_rebuild_creates_documents() {
        let (_dir, mut boot, req) = sync_fixture("initial");
        let mut out = Vec::new();
        let stats = sync(&mut boot, &req, Instant::now(), &mut out).expect("sync succeeds");

        assert_eq!(stats.sources_processed, 1, "one source processed");
        assert_eq!(stats.documents_created, 1, "one document created");
        assert_eq!(stats.documents_updated, 0);
        assert!(
            stats.errors.is_empty(),
            "unexpected errors: {:?}",
            stats.errors
        );

        // Persisted rows (the task's count check).
        assert_eq!(doc_count(&boot), 1, "one document row");
        assert!(chunk_count(&boot) >= 1, "at least one chunk row");
        let vectors = boot.vectors.as_deref().expect("engine opened");
        assert_eq!(
            vectors.count().expect("engine count"),
            1,
            "chunk vector stored"
        );

        // The summary block is printed (oracle fields).
        let printed = String::from_utf8(out).expect("utf-8 summary");
        assert!(printed.contains("=== Sync Summary ==="), "got: {printed}");
        assert!(printed.contains("Sources processed: 1"), "got: {printed}");
        assert!(printed.contains("Documents created:  1"), "got: {printed}");
        assert!(printed.contains("Duration:"), "got: {printed}");
    }

    // --- sync: --rebuild -----------------------------------------------------

    #[test]
    fn sync_rebuild_reingests_and_converges() {
        let (dir, mut boot, mut req) = sync_fixture("rebuild");
        let mut out = Vec::new();

        let first = sync(&mut boot, &req, Instant::now(), &mut out).expect("first sync");
        assert_eq!(first.documents_created, 1, "first run creates the document");

        // Change the document content, then rebuild: the old document is
        // cleared and the new content re-ingested.
        let src = dir.as_ref().join("src");
        std::fs::write(src.join("doc.md"), "# Title\n\nCompletely new body.\n")
            .expect("rewrite document");
        req.rebuild = true;
        let second = sync(&mut boot, &req, Instant::now(), &mut out).expect("rebuild sync");

        assert_eq!(
            second.documents_created, 1,
            "rebuild re-creates the document"
        );
        assert_eq!(
            second.documents_updated, 0,
            "no update path on a cleared db"
        );
        assert!(
            second.errors.is_empty(),
            "unexpected errors: {:?}",
            second.errors
        );

        assert_eq!(doc_count(&boot), 1, "rebuild converges to one document");
        let vectors = boot.vectors.as_deref().expect("engine opened");
        assert_eq!(
            vectors.count().expect("engine count"),
            1,
            "re-embedded once"
        );
    }

    // --- sync: dimension mismatch ---------------------------------------------

    /// Pre-creates a stored ANN index with a different dimension (8 vs the
    /// 4-dim test config) so `open_vectors_engine` reports a mismatch. The
    /// index lands at the fixture's dataset vectors path (dataset `edtech`).
    fn precreate_mismatched_index(dir: &TempDir) {
        let vectors_path = dir
            .as_ref()
            .join("workspace")
            .join("datasets")
            .join("edtech")
            .join("state")
            .join("vectors");
        let stored = VectorIndexConfig::new(8, 16, 100, 256, 32, 200).expect("index config");
        LanceEngine::create(&vectors_path, stored).expect("create stored index");
    }

    #[test]
    fn sync_mismatch_auto_rebuild_recreates_engine() {
        let (dir, mut boot, mut req) = sync_fixture("dim-auto");
        precreate_mismatched_index(&dir);
        req.auto_rebuild_vectors = true;
        let mut out = Vec::new();

        let stats = sync(&mut boot, &req, Instant::now(), &mut out).expect("auto-rebuild sync");

        assert_eq!(
            stats.documents_created, 1,
            "forced re-ingest creates the document"
        );
        assert!(boot.dimension_mismatch.is_none(), "mismatch flag cleared");
        let vectors = boot.vectors.as_deref().expect("engine recreated");
        assert_eq!(
            vectors.count().expect("engine count"),
            1,
            "fresh index holds the vector"
        );
    }

    #[test]
    fn sync_mismatch_with_rebuild_recreates_engine() {
        // `--rebuild` resets everything anyway, so the mismatch takes the
        // recreate path even without auto-rebuild (oracle: the check is
        // skipped under `--rebuild`).
        let (dir, mut boot, mut req) = sync_fixture("dim-rebuild");
        precreate_mismatched_index(&dir);
        req.rebuild = true;
        let mut out = Vec::new();

        let stats = sync(&mut boot, &req, Instant::now(), &mut out).expect("rebuild sync");

        assert_eq!(
            stats.documents_created, 1,
            "full reset re-creates the document"
        );
        assert!(boot.dimension_mismatch.is_none(), "mismatch flag cleared");
    }

    #[test]
    fn sync_mismatch_without_auto_rebuild_is_fatal() {
        let (dir, mut boot, req) = sync_fixture("dim-fatal");
        precreate_mismatched_index(&dir);
        let mut out = Vec::new();

        let err = sync(&mut boot, &req, Instant::now(), &mut out)
            .expect_err("the mismatch must be fatal without auto-rebuild");
        assert!(
            matches!(
                err,
                CliError::Unsupported(ref msg) if msg.contains("dimension mismatch")
            ),
            "got: {err:?}"
        );
        assert!(boot.vectors.is_none(), "no engine on the fatal path");
    }

    // --- format_duration --------------------------------------------------------

    #[test]
    fn format_duration_matches_go_rounding() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(400)), "0s");
        assert_eq!(format_duration(Duration::from_millis(2400)), "2s");
        // Half away from zero, like Go's Round.
        assert_eq!(format_duration(Duration::from_millis(2500)), "3s");
        assert_eq!(format_duration(Duration::from_secs(61)), "1m1s");
        assert_eq!(format_duration(Duration::from_secs(75)), "1m15s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h1m1s");
    }

    // --- print_summary ------------------------------------------------------------

    #[test]
    fn summary_block_matches_oracle_fields() {
        let stats = SummaryStats {
            sources_processed: 2,
            documents_created: 3,
            documents_updated: 4,
            documents_skipped: 5,
            errors: vec!["src-a: boom".to_string()],
        };
        let mut out = Vec::new();
        print_summary(&mut out, &stats, Duration::from_secs(75)).expect("write");
        let text = String::from_utf8(out).expect("utf-8");
        let expected = "\n=== Sync Summary ===\n\
Sources processed: 2\n\
Documents created:  3\n\
Documents updated:  4\n\
Documents skipped:  5\n\
Errors:            1\n\
Duration:          1m15s\n\
==========================\n";
        assert_eq!(text, expected);

        // No errors: the Errors line is omitted (oracle parity).
        let clean = SummaryStats {
            sources_processed: 1,
            ..Default::default()
        };
        let mut out2 = Vec::new();
        print_summary(&mut out2, &clean, Duration::ZERO).expect("write");
        let text2 = String::from_utf8(out2).expect("utf-8");
        assert!(
            !text2.contains("Errors:"),
            "Errors line must be omitted: {text2}"
        );
        assert!(text2.contains("Duration:          0s"), "got: {text2}");
    }
}
