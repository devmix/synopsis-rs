# Design: test-hygiene-phase-1

## Deployment / context note

Pure Rust-side test hygiene. No behavior, contract, or dependency change. The Go oracle
is **not** the reference for this change (nothing is being ported); `../synopsis` must
remain untouched. All decisions below were confirmed by the user on 2026-09-01.

## Decisions

### D1 — Audit findings are baked into the task bodies (no runtime audit task)

The full audit (duplicates, test-only code, per-file movable/stay-inline/needs-pub
classification, baseline counts — Appendix A) was performed during change preparation and
its concrete results are written directly into each task body (exact stay-inline test
names, exact `test_support` items, exact file scope).

*Why not a first "run the audit" implementation task:* task bodies must be
self-contained for a fresh agent with no memory; an audit-doc dependency would make every
later task depend on an artifact that doesn't exist yet and force agents to re-derive
classifications. Baking the measured results in keeps each task deterministic.

### D2 — One extraction task per file, ordered; pure moves exempt from the ~500-line cap

Each extraction task handles exactly one source file (its test module → one integration
test file). Extracting a 1,200-line test module is a pure move (delete + add, zero new
logic) whose diff exceeds the ~500-line "new code + tests" cap on a literal reading.

*Why the exemption (user decision, 2026-09-01):* the cap exists to bound new logic per
fresh agent (~100k context); a mechanical move of one file's test module is well within
one agent's context and adds no logic. The strict alternative (splitting one test module
across 2–3 subtasks) was rejected as choppier to review with no context-budget benefit.
The cap still applies to any *new/changed* logic inside an extraction task (e.g. the
`test_support` module, visibility edits).

### D3 — `#[doc(hidden)] pub mod test_support` for the 79 unlockable tests (user decision A)

Tests that need a handful of private items are unlocked by a per-crate
`#[doc(hidden)] pub mod test_support` (always compiled, hidden from docs) that re-exports
exactly the needed items; those items are widened to `pub(crate)` where they were private.

Items exposed:
- `llm::test_support`: `with_sleeper` (drop the `#[cfg(test)]` gate, keep `pub(crate)`),
  `backoff_delay` (private → `pub(crate)`)
- `mcp::test_support`: `endpoint_url`, `encode_sse_frame` (private → `pub(crate)`),
  `CHANNEL_CAPACITY` (private → `pub(crate)`)
- `ingestion::test_support`: `walk_matched_files` (already `pub(crate)` in
  `parsers/mod.rs` — re-export only, no visibility change)

*Why not plain `pub`:* pollutes the documented API and adds `missing_docs` burden.
*Why not `#[cfg(test)]` helpers:* `cfg(test)` items are invisible to integration tests
(the test crate links the lib compiled without `cfg(test)`).
*Why not leave the 79 tests inline:* the user's goal is shrinking the largest files;
`walk_matched_files` alone unlocks 35 tests across the two biggest ingestion files.
`pub(crate)` widening is crate-internal — no public API surface change; re-export counts
as a use, so no `dead_code` warnings in production builds.

### D4 — Overlap with existing integration tests: move, don't remove (user decision B)

Some movable tests overlap coverage already present in `tests/` (e.g. `search/hybrid.rs`
↔ `search/tests/hybrid_integration.rs`; `graph/linker.rs` equals/expression ↔
`graph/tests/linker_pipeline.rs`; `mcp/sse.rs` ↔ `mcp/tests/server_integration.rs` +
`parity-harness/tests/sse_parity.rs`; ingestion ↔ `pipeline_e2e.rs`/`parity_fixtures.rs`).
Policy: **move** the inline tests (shrinks the source file, preserves their specific
assertions). If an implementer finds a *true* duplicate (same name + body) of an existing
integration test, it removes the inline copy instead and records it in the task revision
history.

*Why not remove on overlap:* overlap ≠ duplication; the inline tests carry distinct
assertions and removing them would silently shrink coverage.

### D5 — Dead test-only code removal is a no-op (user decision C)

The audit found no production item referenced only from tests: `with_sleeper`,
`provider_names`, the `test_util` blocks, and `TempTree` are all deliberate
`#[cfg(test)]` infrastructure; `db::test_util` is public and used by integration tests.
No task deletes production code for this reason.

### D6 — `cel` dev-dependency for `graph` (not a new dependency)

The 29 extracted `cel.rs` tests assert on `cel::Value`, which `graph` does not re-export
and integration tests cannot reach through `graph`'s `[dependencies]`. Adding `cel` to
`graph` `[dev-dependencies]` (same version, already in the tree) is the minimal fix.

*Why not re-export `Value` from `graph`:* would expand the public API for a test need;
the dev-dep touches no production surface and changes Cargo.lock in no way.

### D7 — Stay-inline tests stay; no widening beyond D3 items

The 33 tests that need private internals (`Debouncer`, `debounce_loop`, `MockLlm`,
`test_server`/`SseState`, `LlmClient.config`, `Watcher.task`, `invert_score`,
`standalone_results`, `cross_domain_pairs`) remain in their source files. Their private
items are not widened.

### D8 — Dedup by delegation to the production function

`test_platform_key` (byte-identical in `cli` and `embedding`) is deduplicated by
delegating both helpers to the production `current_platform_key()` — the pattern
`embedding/src/library.rs:411` already uses.

*Why not delete one helper and call across crates:* `cli` and `embedding` are separate
crates; the `#[cfg(test)]` helper is invisible across the crate boundary. Delegation to
the production fn also makes the tests exercise the real code path (strict improvement),
and removes the duplicated logic entirely.

## Oracle reference

None. No `../synopsis` file is a reference for this change (no behavior is ported or
compared). The machine gates are the Rust workspace's own: fmt / clippy / test.

## Appendix A — Audit results (measured 2026-09-01, exact file:line)

**Baseline:** 1,474 tests workspace-wide (cli 134, config 120, db 187, embedding 101,
graph 95, ingestion 377, llm 42, mcp 183, parity-harness 42, search 102, utils 8,
vectors 83). *Corrected 2026-09-01 during task 1.1:* the initial static audit
undercounted db (+11), embedding (+2), graph (+1); the authoritative total is from
`cargo test --workspace -- --list`. The per-file counts below were re-verified against
the tree and are unchanged. Note: six `#[tokio::test(start_paused = true)]` in
`mcp/src/transport/sse.rs` (1114/1130/1154/1169/1187/1233) and one in
`mcp/src/server.rs:1368` are easily missed by naive `#[test]` greps — and a comment at
`sse.rs:1097` contains the literal attribute text, so attribute-grep counts must exclude
comment lines (sse.rs has 23 tests, not 24).

**Duplicates:** 0 true duplicate `#[test]` fns (43 duplicate *names* are coincidental
parallel suites — different DAOs/parsers/types, bodies verified). One true duplicate
helper: `test_platform_key` — `crates/cli/src/onnx_runtime.rs:212` ≡
`crates/embedding/src/lib.rs:326` (byte-identical inline OS/ARCH match);
`crates/embedding/src/library.rs:411` is a coincidental name (delegates to
`current_platform_key()` — keep).

**Per-file classification (256 tests across the 12 files):**

| File (tests) | MOVABLE | STAY_INLINE (names / blocker) | NEEDS_PUB (via test_support) |
|---|---|---|---|
| `llm/src/client.rs` (34) | 24 | 2: `new_accepts_valid_config` (:651), `new_accepts_zero_max_retries` (:739) — read private `LlmClient.config` | 8: 7 via `with_sleeper` seam; `backoff_delay_stays_within_jitter_band_and_varies` (:1381) via private `backoff_delay` |
| `db/src/fact.rs` (30) | 30 | — | — |
| `ingestion/src/ingester/mod.rs` (23) | 0 | — | 23: all via `pub(crate) walk_matched_files` (`parsers/mod.rs:54`) reached through `TestSource`/`Harness` |
| `search/src/hybrid.rs` (15) | 13 | 2: `invert_score_reciprocal` (:1067) → private `invert_score` (:361); `standalone_results_maps_hits` (:1076) → private `standalone_results` (:336) | — |
| `ingestion/src/runner/mod.rs` (13) | 1: `detect_source_type_matches_the_oracle_cases` (:1058) | — | 12: via `walk_matched_files` |
| `cli/src/serve/bootstrap.rs` (22) | 22 | — | — |
| `cli/src/serve/watcher.rs` (15) | 0 | 15: private `Debouncer` (3), `debounce_loop` (2), `relevant_kind`, `wanted_extension`/`normalize_extensions`, `watchable_sources` (3), `IngestChangeHandler::handle_changes` (4), `Watcher.task` (1) — **not extractable, out of scope** | — |
| `mcp/src/transport/sse.rs` (23) | 11: `session_map_*` (3), `session_ids_are_unique`, `send_to_dropped_receiver`, `sse_endpoint_serves`, `channel_payload_streams`, reaper ×4, `idle_sse_stream_ends` | 5: `message_without_session_id`, `message_with_unknown_session_id`, `message_with_malformed_body`, `message_round_trip`, `touched_sse_stream` — need `#[cfg(test)] test_server()` (`handle_message` takes `State<SseState{sessions, server}>`) | 7: `endpoint_url` ×3, `encode_sse_frame` ×3, `CHANNEL_CAPACITY` ×1 (private) |
| `mcp/src/tools/documents.rs` (19) | 19 | — | — |
| `mcp/src/tools/graph_tools.rs` (17) | 17 | — | — |
| `graph/src/cel.rs` (29) | 29 (need `cel` dev-dep for `cel::Value` assertions) | — | — |
| `graph/src/linker.rs` (16) | 7: equals ×2, expression ×3, `method_order_from_config`, `self_link_never_created` | 9: `cross_domain_pairs` ×2 (private fn); `llm_*` ×7 (private `MockLlm` at :1283) | — |
| **Total** | **173** | **33** | **50** (walk_matched_files 35, llm 8, sse 7) |

**Overlap warnings (move, per D4):** `graph/linker.rs` ↔ `graph/tests/linker_pipeline.rs`;
`search/hybrid.rs` ↔ `search/tests/hybrid_integration.rs`; `mcp/sse.rs` ↔
`mcp/tests/server_integration.rs` + `parity-harness/tests/sse_parity.rs`;
ingestion ingester/runner ↔ `ingestion/tests/pipeline_e2e.rs` + `parity_fixtures.rs`.

**Existing integration tests (coverage map):** cli: `cli.rs`, `db_cli.rs`, `queue_cli.rs`;
config: `domain.rs`, `onnx.rs`, `ontology.rs`, `preset.rs`; db: `fts5_parity.rs`;
graph: `linker_pipeline.rs`, `llm_linker_pipeline.rs`; ingestion: `parity_fixtures.rs`,
`pipeline_e2e.rs`; mcp: `server_integration.rs`; parity-harness: `parity_test.rs`,
`sse_parity.rs`; search: `hybrid_integration.rs`; vectors: `persistence_integration.rs`.
(`embedding`, `llm`, `utils` have no `tests/` dir; `llm` gets one in task 1.9.)
