# Proposal: Unified document-jobs queue (single ingestion control plane)

## Problem

Document operations are scattered across five modules: `ingester` (per-document
pipeline), `runner` (multi-source orchestrator with `Mutex<()>` serialization),
`serve/watcher` (`IngestChangeHandler` calls `ingest_source_by_path` + `prune_deleted`
directly), `serve/ingest` (thin wrappers), `serve/server` (owner loop). There is **no
single place** that owns document state, **no retry policy**, and **no visibility** into
what is indexed / failing. A document that fails (e.g. a truncated LLM-NER JSON) is simply
skipped for the run and only re-processed if the whole source is re-ingested — there is no
automatic background retry, and the failure is only an `eprintln!("warning: ...")`.

The user wants ONE queue/state-machine shared by the watcher, the startup scan, and a
background worker; failed documents auto-retry (configurable counter, default 3) and flip
to `error` after the cap; and two CLI commands to inspect state and reset retry counters.
This also unlocks future parallelism (parse/NER off-thread, linking converged) by
centralizing the pipeline.

## Decision

Introduce a `document_jobs` table in the **knowledge** DB as a state machine. All document
events (watcher, startup scan, CLI) become **producers** that enqueue/reconcile jobs; a
single **background worker** (on the serve owner thread, because `Runner` is `!Send`) is
the only **consumer** that runs the pipeline and updates state. GC (orphan cleanup) moves
into the worker as the final phase of each cycle.

**Phase 1 is sequential** (parse → NER → link), per user direction "на первом этапе можно
не параллелить". Parallelism (embed/NER in `spawn_blocking`) is explicitly deferred to a
future change.

## Non-goals (Phase 1)

- No parallelism of parse/embed/NER (deferred; collaborators `EmbeddingProvider`,
  `VectorIndex`, `NerProvider` are already `Send + Sync`, so it is straightforward later).
- No change to the existing 12 MCP tools or their responses.
- No change to the chunking/embedding/NER algorithms themselves — only the orchestration
  and state tracking around them.
- No new source types.

## Why Option A (queue table + owner-thread worker)

`problem-analyst` evaluated three options:
- **A (chosen):** queue table + owner-thread worker. Minimal change to the existing
  `Runner` API (queue wraps it); respects `!Send`; retry/backoff live in SQL; CLI reads
  the table directly. Incremental, each phase independently testable.
- **B (rejected):** dedicated `jobs` crate + async worker pool. Requires splitting `Runner`
  into `Send`/`!Send` parts — a separate large refactor, high parity risk, unnecessary on a
  single-user laptop.
- **C (rejected):** in-memory channel queue. No persistence (loses jobs/counters on
  restart), no CLI visibility — violates the explicit requirement for `index status` and
  `reset-retries`.

## Frozen-contract decisions (explicit, per openspec/config.yaml)

- **CLI surface:** new `index` subcommand (`status`, `reset-retries`) — additive; the
  oracle has no equivalent, so no parity requirement. Human-approved 2026-08-29.
- **Config format:** new `ingestion.max_retries` (default 3) and
  `auto_update.retry_failed.{enabled, poll_interval_seconds}` — additive, backward
  compatible (serde defaults). Human-approved 2026-08-29.
- **Data schema:** new `document_jobs` table via forward-only migration `2-document-jobs`
  — additive, low risk.
