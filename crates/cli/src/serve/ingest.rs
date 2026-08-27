//! Ingestion run wrappers (design D4): the serve/sync call sites (initial
//! sync, file watcher, periodic jobs) invoke the ingestion [`Runner`] through
//! these facades instead of the runner directly — one logging site per
//! operation (oracle `serve.go` / `sync.go` call sites).
//!
//! The wrappers are thin: they add logging only. Failure semantics are
//! unchanged — a full run collects per-source errors in
//! [`SummaryStats::errors`], the single-source operations return
//! [`IngestionError`].

use ingestion::{IngestionError, OrphanCleanupStats, ProgressStats, Runner, SummaryStats};

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

/// Re-ingests the configured source containing `path` (oracle
/// `IngestSourceByPath` — the file-watcher entry point, design D5).
///
/// # Errors
///
/// [`IngestionError`] when no configured source owns `path` or the source
/// run fails.
pub fn ingest_source_by_path(
    runner: &Runner<'_>,
    path: &str,
) -> Result<ProgressStats, IngestionError> {
    let result = runner.ingest_source_by_path(path);
    match &result {
        Ok(stats) => {
            tracing::info!(
                path,
                documents_created = stats.documents_created,
                documents_updated = stats.documents_updated,
                documents_skipped = stats.documents_skipped,
                "source sync finished"
            );
        }
        Err(err) => tracing::warn!(path, error = %err, "source sync failed"),
    }
    result
}

/// Removes indexed documents whose source file no longer exists on disk
/// (oracle `PruneDeleted`).
///
/// # Errors
///
/// [`IngestionError`] on a database failure.
pub fn prune_deleted(runner: &Runner<'_>) -> Result<usize, IngestionError> {
    let removed = runner.prune_deleted()?;
    tracing::info!(removed, "prune deleted finished");
    Ok(removed)
}

/// Removes orphaned entities / facts / documents / vectors (oracle
/// `CleanupOrphanedData`).
///
/// # Errors
///
/// [`IngestionError`] on a database failure.
pub fn cleanup_orphaned_data(runner: &Runner<'_>) -> Result<OrphanCleanupStats, IngestionError> {
    let stats = runner.cleanup_orphaned_data()?;
    tracing::info!(
        entities_deleted = stats.entities_deleted,
        facts_deleted = stats.facts_deleted,
        documents_deleted = stats.documents_deleted,
        vectors_deleted = stats.vectors_deleted,
        "orphan cleanup finished"
    );
    Ok(stats)
}
