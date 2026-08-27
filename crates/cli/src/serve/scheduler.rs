//! Periodic job scheduler (design D6) — the port of the oracle's
//! `setupScheduler` (`../synopsis/cmd/app/serve.go`): a background scheduler
//! that runs the `orphan_cleanup` job on a fixed interval, removing orphaned
//! entities / facts / documents / vectors while the server is up.
//!
//! The job body is injected as a closure instead of the ingestion `Runner`
//! directly (design D6: "a Runner or a closure calling
//! `cleanup_orphaned_data`"). The production call site (the serve wiring, task
//! 1.6) wires `Runner::cleanup_orphaned_data`; unit tests inject a stub.
//!
//! The closure must be `Send + Sync + 'static` because the job runs on the
//! scheduler's tokio worker (tokio-cron-scheduler requires it). The ingestion
//! `Runner` is `!Send + !Sync` (it borrows a `!Send` source registry), so
//! the production closure must NOT capture it directly — it hands the work to
//! the owner thread via a channel, mirroring the file-watcher pattern (design
//! D5). That hand-off is the serve wiring's concern; this module only fires
//! the closure on schedule.
//!
//! The `orphan_cleanup` job is registered only when enabled in
//! `config.scheduler.jobs`; an inert scheduler (disabled job) owns no
//! background actors and its [`Scheduler::start`] / [`Scheduler::shutdown`]
//! are no-ops (oracle parity: `sched` stays `nil`).

use std::time::Duration;

use config::preset::SchedulerConfig;
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};

/// Name of the orphan-cleanup job (oracle `cfg.Scheduler.Jobs["orphan_cleanup"]`).
pub const ORPHAN_CLEANUP_JOB: &str = "orphan_cleanup";

/// A periodic background-job scheduler (design D6).
///
/// Holds the [`JobScheduler`] only when the `orphan_cleanup` job is enabled;
/// an inert scheduler (disabled job) owns no background actors.
pub struct Scheduler {
    scheduler: Option<JobScheduler>,
}

impl Scheduler {
    /// Builds the scheduler and registers the `orphan_cleanup` job when
    /// `config.jobs["orphan_cleanup"].enabled` is `true`.
    ///
    /// `cleanup` is the job body (production: `Runner::cleanup_orphaned_data`;
    /// tests: a stub). The run interval is
    /// `config.jobs["orphan_cleanup"].interval_seconds`, clamped to a minimum
    /// of one second — a non-positive interval would otherwise wrap (negative)
    /// or hot-loop (zero); the config default is ≥ 3600 s, so this only guards
    /// a misconfigured raw [`SchedulerConfig`].
    ///
    /// Must be called from within a tokio runtime (the job scheduler spawns
    /// actors).
    ///
    /// # Errors
    ///
    /// [`JobSchedulerError`] when the underlying scheduler cannot be created
    /// or the job cannot be registered.
    pub async fn new(
        config: &SchedulerConfig,
        cleanup: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, JobSchedulerError> {
        let Some(job) = config.jobs.get(ORPHAN_CLEANUP_JOB) else {
            return Ok(Self::inactive());
        };
        if !job.enabled {
            return Ok(Self::inactive());
        }

        let interval = Duration::from_secs(job.interval_seconds.max(1) as u64);
        let scheduler = JobScheduler::new().await?;
        let job = Job::new_repeated(interval, move |_uuid, _sched| cleanup())?;
        scheduler.add(job).await?;
        tracing::info!(
            job = ORPHAN_CLEANUP_JOB,
            interval_secs = interval.as_secs(),
            "orphan_cleanup job registered"
        );
        Ok(Self {
            scheduler: Some(scheduler),
        })
    }

    /// An inert scheduler (the `orphan_cleanup` job is not enabled).
    fn inactive() -> Self {
        Self { scheduler: None }
    }

    /// Whether the `orphan_cleanup` job is registered.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.scheduler.is_some()
    }

    /// Starts the scheduler (called after the initial sync). A no-op when the
    /// job is not registered.
    ///
    /// # Errors
    ///
    /// [`JobSchedulerError`] when the underlying scheduler cannot start.
    pub async fn start(&self) -> Result<(), JobSchedulerError> {
        match &self.scheduler {
            Some(scheduler) => {
                scheduler.start().await?;
                tracing::info!("scheduler started");
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Stops the scheduler (called on shutdown). A no-op when the job is not
    /// registered.
    ///
    /// # Errors
    ///
    /// [`JobSchedulerError`] when the underlying scheduler cannot stop.
    pub async fn shutdown(&mut self) -> Result<(), JobSchedulerError> {
        match &mut self.scheduler {
            Some(scheduler) => scheduler.shutdown().await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use config::preset::JobConfig;

    use super::*;

    /// A scheduler config with the `orphan_cleanup` job in the given state.
    fn config_with_orphan_cleanup(enabled: bool) -> SchedulerConfig {
        let mut jobs = HashMap::new();
        jobs.insert(
            ORPHAN_CLEANUP_JOB.to_owned(),
            JobConfig {
                enabled,
                interval_seconds: 3600,
            },
        );
        SchedulerConfig { jobs }
    }

    #[tokio::test]
    async fn job_registered_only_when_enabled() {
        let enabled = Scheduler::new(&config_with_orphan_cleanup(true), || {})
            .await
            .unwrap();
        assert!(enabled.is_registered(), "enabled job must be registered");

        let disabled = Scheduler::new(&config_with_orphan_cleanup(false), || {})
            .await
            .unwrap();
        assert!(
            !disabled.is_registered(),
            "disabled job must not be registered"
        );

        // An absent job entry is also unregistered (oracle `ok` guard).
        let absent = Scheduler::new(&SchedulerConfig::default(), || {})
            .await
            .unwrap();
        assert!(!absent.is_registered(), "absent job must not be registered");
    }

    #[tokio::test]
    async fn shutdown_does_not_panic() {
        // Enabled: a real start → stop lifecycle.
        let mut enabled = Scheduler::new(&config_with_orphan_cleanup(true), || {})
            .await
            .unwrap();
        enabled.start().await.unwrap();
        enabled.shutdown().await.unwrap();

        // Disabled: shutdown is a no-op and must not panic.
        let mut disabled = Scheduler::new(&config_with_orphan_cleanup(false), || {})
            .await
            .unwrap();
        disabled.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn start_is_noop_when_not_registered() {
        let scheduler = Scheduler::new(&config_with_orphan_cleanup(false), || {})
            .await
            .unwrap();
        assert!(scheduler.start().await.is_ok(), "no-op start must succeed");
    }
}
