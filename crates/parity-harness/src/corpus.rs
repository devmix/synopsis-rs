//! Deterministic content corpus for the content-parity tests (design D2).
//!
//! [`write_content_corpus`] writes the expanded corpus the content-parity
//! fixtures are recorded against: **8 markdown documents across 3 domains**
//! (`hr/`, `product/`, `eng/`). Every document is a compile-time string
//! literal — no randomness, no `chrono`/`SystemTime` timestamps — so
//! ingestion is byte-for-byte deterministic on both the Go oracle (one-time
//! recording) and the Rust product (verify).
//!
//! The corpus carries a **consistent entity vocabulary** that recurs across
//! domains, so multi-domain listing and cross-domain search are exercised:
//!
//! - people: Dana Kovac (HR), Marcus Webb (product), Priya Sharma (engineering);
//! - systems: Atlas (the product), Portal (HR portal), Ledger (data
//!   pipeline), Beacon (monitoring);
//! - policies: Vacation Policy, Remote Work Policy, API v2 deprecation.
//!
//! Eight documents is enough that `catalog_documents` pagination with a page
//! size below the total is exercised. NER is disabled in both the Go
//! recording config and the Rust harness, so only the chunk text matters —
//! the corpus needs no NER-relevant ambiguity.
//!
//! This is a distinct corpus from the latency test's 2-doc `write_corpus` in
//! `tests/parity_test.rs` (design D2): the latency p50/p95 baseline was
//! recorded against that fixed corpus and must not change.

use std::path::Path;

/// The 8 corpus documents as `(relative path, static markdown)` pairs.
///
/// Layout: `hr/` (3 docs), `product/` (3 docs), `eng/` (2 docs). The
/// markdown is static: no random values, no timestamps.
const CONTENT_CORPUS: [(&str, &str); 8] = [
    (
        "hr/vacation-policy.md",
        r#"# Vacation and Leave Policy

The Vacation Policy is owned by Dana Kovac, HR Director, and applies to all
full-time employees. Requests are submitted through the Portal at least two
weeks in advance.

## Annual Entitlement

Employees are entitled to twenty days of paid vacation per calendar year.
Vacation days accrue on the first day of each month and must be taken within
the calendar year, with a carry-over of up to five days into the next year.
Team leads approve requests in the Portal; cross-team coverage must be
arranged before approval.

## Sick Leave

Sick leave is unlimited and does not reduce the vacation balance. Absences
longer than three consecutive days require a medical certificate uploaded to
the Portal. Recurring medical appointments count as half-days.

## Parental Leave

New parents receive sixteen weeks of paid parental leave, extendable to
twenty-four weeks with a written request approved by Dana Kovac. The leave
may be taken in two blocks within the first year after the birth or
adoption date.
"#,
    ),
    (
        "hr/remote-work-policy.md",
        r#"# Remote Work Policy

The Remote Work Policy, owned by Dana Kovac, defines how employees work
off-site. Eligibility starts six months after the start date, and the
request is filed in the Portal.

## Eligibility and Schedule

Employees may work remotely up to three days per week. The two on-site days
are fixed per team and published in the Portal calendar. Engineering teams
aligned with the Atlas release cycle keep Tuesday and Thursday on-site.

## Equipment and Security

Remote workstations receive a quarterly hardware stipend. A company-managed
laptop, a VPN account, and the Beacon desktop agent for security monitoring
are required before the first remote day. Personal devices are not approved
for production access.

## Review

Dana Kovac reviews every remote work agreement twice a year, together with
the team lead. Violations of the on-site schedule are logged in the Portal
and discussed at the next one-on-one.
"#,
    ),
    (
        "hr/onboarding-guide.md",
        r#"# New Hire Onboarding Guide

Onboarding runs for the first ninety days. Dana Kovac coordinates the HR
track, Marcus Webb runs the product tour, and Priya Sharma leads the
engineering orientation.

## Day One

Day one starts with an HR check-in with Dana Kovac: contract, benefits, and
Portal account provisioning. The Portal account unlocks the internal wiki,
the vacation request form, and the equipment request form.

## Weeks One to Four

Week one covers the product tour with Marcus Webb: the Atlas platform, the
customer-facing dashboard, and the API surface. Weeks two and four alternate
between team pair programming and the engineering orientation led by Priya
Sharma, which covers the on-call rotation, the Beacon alerting setup, and
the Ledger data pipeline.

## Ninety-Day Review

The ninety-day review is scheduled in the Portal before the end of month
three. It covers goal progress, Remote Work Policy eligibility, and the
Vacation Policy entitlement that accrues from the start date. Dana Kovac
signs off the review together with the team lead.
"#,
    ),
    (
        "product/atlas-release-notes.md",
        r#"# Atlas 3.0 Release Notes

Atlas 3.0, owned by Marcus Webb, ships the dashboard builder, real-time
collaboration, and the redesigned reporting engine. The release is tracked
in the Beacon release channel.

## Dashboard Builder

The new dashboard builder supports drag-and-drop widgets, saved layouts per
workspace, and shared views. Widgets bind to live Atlas data sources and
refresh on a per-widget schedule.

## Real-Time Collaboration

Workspaces now support concurrent editing with presence indicators.
Conflict resolution follows last-write-wins per widget, and every change is
recorded in the workspace audit log.

## Reporting Engine

The reporting engine was rewritten for the Atlas 3.0 release. Reports run
as scheduled jobs and export to CSV, PDF, and the API. The legacy report
format is read-only and is covered by the API v2 deprecation plan.

## Known Issues

Offline mode may show a stale cache badge on dashboards that poll more than
once per minute. The Beacon status page lists the affected workspace ids.
"#,
    ),
    (
        "product/atlas-api-migration.md",
        r#"# Atlas API v1 to v2 Migration Guide

Atlas API v2 replaces the legacy REST API v1. Marcus Webb owns the
deprecation plan; the v1 endpoints stop working on 2026-06-30.

## Breaking Changes

v2 changes the authentication scheme to scoped tokens, replaces the
/reports polling endpoints with webhooks, and moves pagination from offset
to cursor-based. Request bodies are validated against published JSON
schemas, and unknown fields are rejected instead of ignored.

## Compatibility Mode

Between now and the deprecation date, the Atlas gateway runs compatibility
mode: v1 requests are translated to v2 and answered with a Deprecation
header. Compatibility mode is off by default for new workspaces and is
reported per workspace in the Beacon dashboard.

## Migration Steps

1. Inventory the v1 endpoints used by your integrations.
2. Switch authentication to scoped tokens in a staging workspace.
3. Replace polling with the v2 webhooks and verify delivery with the
   Beacon webhook inspector.
4. Cut over in production and disable compatibility mode for your
   workspace.
"#,
    ),
    (
        "product/roadmap-2026.md",
        r#"# Atlas Product Roadmap 2026

The 2026 roadmap is owned by Marcus Webb and reviewed quarterly with
Priya Sharma, the engineering lead.

## Q1 — Mobile Offline

The Atlas mobile app gains full offline mode: dashboards sync in the
background, and edits merge with the workspace on reconnect. Offline mode
reuses the Atlas 3.0 conflict resolution rules.

## Q2 — Data Platform

The Ledger data platform becomes the single ingestion path for customer
events. Atlas workspaces stream their audit logs to Ledger, and the
reporting engine reads from Ledger instead of the local store.

## Q3 — Copilot

An AI copilot lands in the dashboard builder. It suggests widget layouts
from natural-language prompts and drafts report summaries. Copilot calls are
metered per workspace and reported in the Beacon cost dashboard.

## Q4 — Hybrid Work Tooling

Following the Remote Work Policy review with Dana Kovac, Atlas adds a
shared on-site calendar so hybrid teams coordinate fixed on-site days. The
calendar integrates with the Portal and shows team-level presence.
"#,
    ),
    (
        "eng/oncall-runbook.md",
        r#"# Atlas Service On-Call Runbook

Priya Sharma, the on-call manager, owns this runbook. The Atlas service
team rotates weekly; the primary on-call engineer is paged through Beacon.

## Escalation Ladder

1. The primary on-call acknowledges Beacon pages within fifteen minutes.
2. Unacknowledged pages escalate to the secondary on-call after twenty
   minutes.
3. Severity-1 incidents escalate to Priya Sharma and, for customer-facing
   Atlas outages, to Marcus Webb for the customer communication.

## Common Incidents

### Dashboard builder 5xx spike

Check the Beacon dashboard for the affected release. If the spike started
with a deploy, roll back to the previous Atlas build and open a post
incident review.

### Ledger ingestion lag

When Ledger lags beyond the SLA, pause the non-critical Atlas audit log
stream first, then scale the Ledger workers. See the data pipeline runbook
for the scaling procedure.

### API v2 webhook delivery failures

Verify the customer webhook endpoint in the Beacon webhook inspector.
Expired scoped tokens are the most frequent cause; ask the customer to
rotate the token and resend the backlog.
"#,
    ),
    (
        "eng/data-pipeline.md",
        r#"# Ledger Data Pipeline

Ledger is the Atlas data platform: the ingestion path for customer events
and audit logs. Priya Sharma owns the pipeline, and Marcus Webb defines the
data product requirements.

## Architecture

Ledger has three stages. The ingest stage consumes Atlas workspace streams
and normalizes events. The transform stage enriches events with workspace
metadata and writes to the columnar store. The serve stage exposes the
store to the Atlas reporting engine and the Beacon dashboards.

## SLAs and Alerts

Ledger lags beyond fifteen minutes page the on-call engineer through
Beacon. The on-call runbook defines the triage order: pause non-critical
streams first, then scale the ingest workers.

## Backfills

Backfills are run from the Ledger console with an explicit time range and
workspace list. A backfill larger than one billion events requires a sign
off from Priya Sharma. Completed backfills are recorded in the Beacon
backfill dashboard together with the row counts.
"#,
    ),
];

/// Write the content corpus (design D2): the 8 markdown documents of
/// [`CONTENT_CORPUS`] under the `hr/`, `product/`, and `eng/`
/// sub-directories of `corpus`, creating the directories as needed.
///
/// The output is byte-for-byte deterministic: the same `corpus` root always
/// receives the same bytes, so the Go oracle (one-time recording) and the
/// Rust product (verify) ingest identical input. I/O failures are returned
/// as `std::io::Error`; the function never panics.
pub fn write_content_corpus(corpus: &Path) -> Result<(), std::io::Error> {
    for (rel, content) in CONTENT_CORPUS {
        let path = corpus.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique scratch directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "synopsis-parity-corpus-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    /// The written corpus as `relative path -> bytes` in deterministic
    /// (`CONTENT_CORPUS`) order.
    fn snapshot(base: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        CONTENT_CORPUS
            .iter()
            .map(|(rel, _)| {
                (
                    PathBuf::from(*rel),
                    std::fs::read(base.join(rel)).expect("corpus file present"),
                )
            })
            .collect()
    }

    #[test]
    fn writes_eight_markdown_docs_across_three_domains() {
        let dir = TempDir::new();
        write_content_corpus(&dir).expect("write corpus");

        // Eight files, each a non-empty markdown document with a title.
        assert_eq!(CONTENT_CORPUS.len(), 8, "corpus must hold 8 docs");
        let mut domains = std::collections::BTreeSet::new();
        for (rel, content) in CONTENT_CORPUS {
            let path = dir.join(rel);
            assert!(path.is_file(), "{rel} must be written");
            let text = std::fs::read_to_string(&path).expect("read back");
            assert!(!text.trim().is_empty(), "{rel} must be non-empty");
            assert!(
                text.starts_with("# "),
                "{rel} must be markdown with a title"
            );
            assert_eq!(
                text, *content,
                "{rel} must contain the static literal unchanged"
            );
            let domain = rel.split('/').next().expect("domain segment");
            domains.insert(domain);
        }
        assert_eq!(
            domains,
            ["eng", "hr", "product"].into(),
            "corpus must span exactly the three domains"
        );

        // Each domain carries 2..=3 docs with distinct topics.
        for domain in ["hr", "product", "eng"] {
            let count = CONTENT_CORPUS
                .iter()
                .filter(|(rel, _)| rel.starts_with(&format!("{domain}/")))
                .count();
            assert!(
                (2..=3).contains(&count),
                "{domain}: expected 2..=3 docs, got {count}"
            );
        }
    }

    #[test]
    fn bytes_are_identical_across_calls() {
        // Static content: no rand, no chrono, no SystemTime — two calls
        // into two roots must produce byte-for-byte identical files.
        let a = TempDir::new();
        let b = TempDir::new();
        write_content_corpus(&a).expect("write corpus A");
        write_content_corpus(&b).expect("write corpus B");
        assert_eq!(
            snapshot(&a),
            snapshot(&b),
            "corpus must be byte-for-byte deterministic"
        );
    }

    #[test]
    fn entity_vocabulary_recurs_across_domains() {
        // A few named people, systems, and policies must recur across at
        // least two domains, so multi-domain listing and cross-domain
        // search are exercised (design D2).
        for name in [
            "Dana Kovac",
            "Marcus Webb",
            "Priya Sharma",
            "Atlas",
            "Portal",
            "Ledger",
            "Beacon",
        ] {
            let mut domains = std::collections::BTreeSet::new();
            for (rel, content) in CONTENT_CORPUS {
                if content.contains(name) {
                    domains.insert(rel.split('/').next().expect("domain segment"));
                }
            }
            assert!(
                domains.len() >= 2,
                "`{name}` must recur across at least two domains, found in {domains:?}"
            );
        }
    }
}
