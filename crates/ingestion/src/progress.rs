//! Ingestion run progress: counters and a stderr progress bar.
//!
//! Functional port of the oracle's `internal/ingestion/progress.go`:
//! [`ProgressStats`] is a plain counter snapshot for one ingestion run, and
//! [`ProgressTracker`] accumulates it while an [`indicatif::ProgressBar`]
//! renders per-file progress.
//!
//! Deliberate deviations from the oracle (Rust re-architecture):
//!
//! - The oracle guards its counters with a mutex for cross-goroutine
//!   sharing. A run is single-threaded (the Runner serializes runs behind
//!   one mutex, design D4), so the tracker takes `&mut self` — no interior
//!   mutability needed.
//! - The oracle's progress bar was commented out; here the bar is real but
//!   auto-hidden when stderr is not a terminal, so headless runs (tests,
//!   cron, piped output) never emit ANSI traffic and no separate no-op
//!   tracker type is required.
//! - Counters are `u64`: they only ever increase.
//! - The field the oracle calls `embeddings_generated` is
//!   [`ProgressStats::embeddings_created`] — internal naming only, no
//!   compatibility obligation.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::ProgressBar;

/// Counters for one ingestion run.
///
/// A cheap, immutable snapshot type: the tracker owns the mutable state and
/// hands out copies via [`ProgressTracker::stats`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProgressStats {
    /// Files successfully processed.
    pub files_processed: u64,
    /// Chunks created across all documents.
    pub chunks_created: u64,
    /// Embeddings generated for chunks.
    pub embeddings_created: u64,
    /// Entities extracted by NER.
    pub entities_extracted: u64,
    /// Facts created.
    pub facts_created: u64,
    /// Fact sources (chunk-level provenance rows) created.
    pub fact_sources_created: u64,
    /// Documents inserted as new.
    pub documents_created: u64,
    /// Existing documents updated (content hash changed).
    pub documents_updated: u64,
    /// Documents skipped (unchanged content hash or empty chunk list).
    pub documents_skipped: u64,
    /// Non-fatal errors encountered; the run continues.
    pub errors: u64,
}

/// Accumulates a run's [`ProgressStats`] and renders file progress.
///
/// The bar writes to stderr (stdout is reserved for MCP protocol traffic)
/// and is auto-hidden when stderr is not a terminal.
pub struct ProgressTracker {
    stats: ProgressStats,
    bar: Option<ProgressBar>,
    started: Instant,
    label: String,
}

impl ProgressTracker {
    /// Creates a tracker for a run over `total_files` files.
    ///
    /// `total_files == 0` means the count is unknown and suppresses the bar
    /// entirely; the counters keep working either way.
    pub fn new(total_files: u64, label: &str) -> Self {
        let bar = (total_files > 0).then(|| {
            if std::io::stderr().is_terminal() {
                ProgressBar::new(total_files).with_message(label.to_owned())
            } else {
                // Headless run: a hidden bar keeps the same call sites but
                // never emits ANSI traffic.
                ProgressBar::hidden()
            }
        });
        Self {
            stats: ProgressStats::default(),
            bar,
            started: Instant::now(),
            label: label.to_owned(),
        }
    }

    /// Marks one file as processed and advances the bar.
    pub fn increment_files(&mut self) {
        self.stats.files_processed += 1;
        if let Some(bar) = &self.bar {
            bar.inc(1);
        }
    }

    /// Records chunks created for a document.
    pub fn add_chunks(&mut self, n: u64) {
        self.stats.chunks_created += n;
    }

    /// Records embeddings generated for a document's chunks.
    pub fn add_embeddings(&mut self, n: u64) {
        self.stats.embeddings_created += n;
    }

    /// Records entities extracted by NER.
    pub fn add_entities(&mut self, n: u64) {
        self.stats.entities_extracted += n;
    }

    /// Records facts created for a document.
    pub fn add_facts(&mut self, n: u64) {
        self.stats.facts_created += n;
    }

    /// Records fact sources created for a document.
    pub fn add_fact_sources(&mut self, n: u64) {
        self.stats.fact_sources_created += n;
    }

    /// Records a non-fatal error; the run continues.
    pub fn increment_errors(&mut self) {
        self.stats.errors += 1;
    }

    /// Records a document inserted as new.
    pub fn increment_documents_created(&mut self) {
        self.stats.documents_created += 1;
    }

    /// Records an existing document updated.
    pub fn increment_documents_updated(&mut self) {
        self.stats.documents_updated += 1;
    }

    /// Records a document skipped (unchanged content hash or empty chunk list).
    pub fn increment_documents_skipped(&mut self) {
        self.stats.documents_skipped += 1;
    }

    /// Returns a snapshot of the current counters.
    pub fn stats(&self) -> ProgressStats {
        self.stats
    }

    /// Time since the tracker was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Finishes the bar with a short summary (no-op when the bar is absent).
    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_with_message(format!(
                "{}: {} files, {} chunks in {:.1}s",
                self.label,
                self.stats.files_processed,
                self.stats.chunks_created,
                self.elapsed().as_secs_f64(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn counters_accumulate_per_kind() {
        let mut tracker = ProgressTracker::new(10, "test");
        tracker.increment_files();
        tracker.increment_files();
        tracker.increment_files();
        tracker.add_chunks(5);
        tracker.add_embeddings(50);
        tracker.add_entities(7);
        tracker.add_facts(3);
        tracker.add_fact_sources(4);
        tracker.increment_documents_created();
        tracker.increment_documents_updated();
        tracker.increment_documents_skipped();
        tracker.increment_errors();

        let stats = tracker.stats();
        assert_eq!(
            stats,
            ProgressStats {
                files_processed: 3,
                chunks_created: 5,
                embeddings_created: 50,
                entities_extracted: 7,
                facts_created: 3,
                fact_sources_created: 4,
                documents_created: 1,
                documents_updated: 1,
                documents_skipped: 1,
                errors: 1,
            }
        );
    }

    #[test]
    fn snapshot_is_isolated_from_later_mutation() {
        let mut tracker = ProgressTracker::new(0, "test");
        tracker.add_chunks(1);
        let before = tracker.stats();
        tracker.add_chunks(9);

        assert_eq!(before.chunks_created, 1);
        assert_eq!(tracker.stats().chunks_created, 10);
    }

    #[test]
    fn elapsed_advances() {
        let tracker = ProgressTracker::new(0, "test");
        std::thread::sleep(Duration::from_millis(5));
        assert!(tracker.elapsed() > Duration::ZERO);
    }

    #[test]
    fn finish_is_safe_with_and_without_a_bar() {
        let no_bar = ProgressTracker::new(0, "test");
        no_bar.finish();
        let with_bar = ProgressTracker::new(100, "test");
        with_bar.finish();
    }
}
