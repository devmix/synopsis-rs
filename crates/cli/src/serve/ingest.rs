//! Ingestion run wrapper (design D4): the call sites that still ingest
//! directly (the one-off `sync` subcommand, the serve forced-rebuild
//! recovery) invoke the ingestion [`Runner`] through this facade instead of
//! the runner directly — one logging site per operation (oracle
//! `serve.go` / `sync.go` call sites).
//!
//! The serve producer path no longer goes through here: since
//! document-jobs-queue task 1.5 the file watcher and the startup reconcile
//! enqueue `document_jobs` rows via [`ingestion::DocumentJobQueue`], and the
//! background worker runs the pipeline (state flows through the queue table
//! only).
//!
//! The wrapper is thin: it adds logging only. Failure semantics are
//! unchanged — a full run collects per-source errors in
//! [`SummaryStats::errors`].

use ingestion::{Runner, SummaryStats};

/// Runs a full multi-source ingestion (oracle `IngestAll`) and logs the
/// outcome.
///
/// Per-source failures are collected in [`SummaryStats::errors`], never
/// returned (oracle parity).
pub fn ingest_all(runner: &Runner<'_>, rebuild: bool) -> SummaryStats {
    let stats = runner.ingest_all(rebuild);
    tracing::info!(
        rebuild,
        sources = stats.sources_processed,
        documents_created = stats.documents_created,
        documents_updated = stats.documents_updated,
        documents_skipped = stats.documents_skipped,
        errors = stats.errors.len(),
        "ingestion run finished"
    );
    stats
}
