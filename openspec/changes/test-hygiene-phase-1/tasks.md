# Tasks: test-hygiene-phase-1

## Change header (read before any task)

Phase 1 of the post-migration roadmap: test hygiene. Relocate inline `#[cfg(test)]` test
modules into integration test files (`crates/<crate>/tests/`) to shrink application-code
files; remove the one true duplicate helper. Read `proposal.md` and `design.md` (decisions
D1–D8 + Appendix A audit data) for this change.

Conventions for every task in this change:
- **Oracle reference: N/A** — pure Rust-side test hygiene; no behavior is ported.
  `../synopsis` is read-only and must not appear in any diff.
- **Gates (all tasks):** `cargo fmt --all --check` clean;
  `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace`
  green; `cargo check --workspace` clean.
- **Test-count invariant:** extraction relocates tests, it does not delete them — the
  workspace total stays **1,474** unless a task documents a true-duplicate removal
  (design D4). Report the before/after count per crate in the task report.
- **Pure moves are exempt from the ~500-line diff cap** (design D2, user decision
  2026-09-01); the cap still bounds any new/changed logic (test_support modules,
  visibility edits).
- **Moving a test means moving it with its private test-module helpers** that it
  exclusively uses. If a private helper is shared between a moved test and a stay-inline
  test, leave the helper inline and give the moved test a local copy in the integration
  file (note it in the task report).
- Integration test files start with `#![allow(clippy::unwrap_used)]` (workspace gate runs
  with `-D warnings`; test modules may opt out locally — existing convention).
- Moved tests keep their names and assertions verbatim (import paths change from
  `crate::…` to the crate name). No assertion rewrites; if a moved test cannot compile
  against the public API as classified, STOP and report `needs_input` with the item name.
- All produced content in English. No new dependencies (frozen stack).

## Task checklist

- [x] 1.1 Dedup: eliminate the `test_platform_key` duplicate
- [x] 1.2 Extract `db/src/fact.rs` tests → `db/tests/fact.rs`
- [x] 1.3 Extract `cli/src/serve/bootstrap.rs` tests → `cli/tests/serve_bootstrap.rs`
- [x] 1.4 Extract `mcp/src/tools/documents.rs` tests → `mcp/tests/documents.rs`
- [x] 1.5 Extract `mcp/src/tools/graph_tools.rs` tests → `mcp/tests/graph_tools.rs`
- [x] 1.6 Extract `graph/src/cel.rs` tests → `graph/tests/cel.rs` (no dev-dep — D6 corrected)
- [x] 1.7 Extract `search/src/hybrid.rs` tests → `search/tests/hybrid_units.rs` (12 moved / 3 inline)
- [x] 1.8 Extract `graph/src/linker.rs` tests → `graph/tests/linker_units.rs` (7 moved / 9 inline)
- [x] 1.9 Extract `llm/src/client.rs` tests → `llm/tests/client.rs` (+ `llm` test_support, new `tests/` dir) (32 moved / 2 inline)
- [x] 1.10 Extract `mcp/src/transport/sse.rs` tests → `mcp/tests/sse_units.rs` (+ `mcp` test_support) (18 moved / 5 inline)
- [x] 1.11 Extract `ingestion/src/ingester/mod.rs` tests → `ingestion/tests/ingester.rs` (+ `ingestion` test_support) (23 moved)
- [ ] 1.12 Extract `ingestion/src/runner/mod.rs` tests → `ingestion/tests/runner.rs` (reuses ingestion test_support)
- [ ] 1.13 Final verification: before/after report + full gates

---

### Task 1.1 — Dedup: eliminate the `test_platform_key` duplicate

**Goal.** Remove the single true duplicate in the workspace: the `test_platform_key`
helper, byte-identical in two crates.

**File scope.**
- `crates/cli/src/onnx_runtime.rs` (helper at :212)
- `crates/embedding/src/lib.rs` (helper at :326)
- `crates/embedding/src/library.rs` (only if `current_platform_key()` needs a visibility
  bump so `cli` can reach it — check first)

**Dependencies.** None.

**Approach (design D8).** The production function `current_platform_key()` exists in
`embedding` (see `crates/embedding/src/library.rs:411`, whose same-named test helper
already delegates to it). Make both byte-identical `test_platform_key` helpers delegate
to the production function instead of re-implementing the inline OS/ARCH match — exactly
the `library.rs:411` pattern. If `current_platform_key()` is not reachable from the `cli`
crate, widen it minimally (`pub`) and document it in the task report. Do NOT touch the
`library.rs:411` helper (already correct).

**Acceptance criteria.**
1. `rg -n "test_platform_key" crates/` shows no remaining inline OS/ARCH match
   re-implementation; both helpers are one-line delegations to `current_platform_key()`.
2. All gates green. `cargo test -p cli -p embedding` green; workspace test count
   unchanged (1,474).
3. No files outside the scope touched; `../synopsis` untouched; no dependency changes.

---

### Task 1.2 — Extract `db/src/fact.rs` tests → `db/tests/fact.rs`

**Goal.** Shrink `crates/db/src/fact.rs` (1,823 lines, 70% test) by moving its entire
inline test module into a new integration test file.

**File scope.**
- `crates/db/src/fact.rs` (source; test module to be emptied out)
- `crates/db/tests/fact.rs` (new integration test file)

**Dependencies.** None.

**Approach.** All 30 tests in the `fact.rs` test module are classified MOVABLE (public
API only; design Appendix A). Move the whole test module to `crates/db/tests/fact.rs`:
rewrite `use crate::…` imports to `use db::…`, carry over the module's private helpers,
keep names/assertions verbatim, add `#![allow(clippy::unwrap_used)]` at the top. Remove
the now-empty `#[cfg(test)] mod tests` block from `fact.rs` (and any imports used only by
it). `db::test_util` (public, `crates/db/src/test_util.rs`) is already usable from
integration tests — use it as-is.

**Acceptance criteria.**
1. `crates/db/src/fact.rs` no longer contains a `#[cfg(test)]` block; line count drops by
   the moved test lines (target: ≤ ~600 lines).
2. `cargo test -p db` reports the same test count as before the move (30 fact tests now
   run from `tests/fact.rs`); all gates green.
3. Moved tests keep names/assertions verbatim. No production logic changed. Scope-only
   diff; `../synopsis` untouched.

---

### Task 1.3 — Extract `cli/src/serve/bootstrap.rs` tests → `cli/tests/serve_bootstrap.rs`

**Goal.** Shrink `crates/cli/src/serve/bootstrap.rs` (1,403 lines, 57% test) by moving its
inline test module to a new integration test file.

**File scope.**
- `crates/cli/src/serve/bootstrap.rs` (source)
- `crates/cli/tests/serve_bootstrap.rs` (new; `cli/tests/` already exists with
  `cli.rs`, `db_cli.rs`, `queue_cli.rs` — do not modify those)

**Dependencies.** None.

**Approach.** All 22 tests are classified MOVABLE (public API only). Move the test module
to `crates/cli/tests/serve_bootstrap.rs`, rewrite `use crate::…`/`use super::*` →
`use cli::…` (the lib target is named `cli` — the default = package name; the `[[bin]]`
is `synopsis`. `serve` and `serve::bootstrap` are both `pub`, so
`cli::serve::bootstrap::…` is importable), carry private helpers, keep
names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]`.

**Acceptance criteria.**
1. `bootstrap.rs` has no `#[cfg(test)]` block; line count drops by the moved test lines.
2. `cargo test -p cli` test count unchanged (22 relocated); all gates green.
3. Names/assertions verbatim; scope-only diff; `../synopsis` untouched.

---

### Task 1.4 — Extract `mcp/src/tools/documents.rs` tests → `mcp/tests/documents.rs`

**Goal.** Shrink `crates/mcp/src/tools/documents.rs` (1,139 lines, 56% test) by moving its
inline test module to a new integration test file.

**File scope.**
- `crates/mcp/src/tools/documents.rs` (source)
- `crates/mcp/tests/documents.rs` (new; `mcp/tests/` already exists — do not modify
  `server_integration.rs`)

**Dependencies.** None. (Task 1.5 touches a different file in the same crate — no
conflict.)

**Approach.** All 19 tests are MOVABLE (public API only). Move the module to
`crates/mcp/tests/documents.rs`, rewrite `use crate::…`/`use super::*` → `use mcp::…`
(the lib target is named `mcp`; `tools` and `tools::documents` are both `pub`, so
`mcp::tools::documents::…` is importable), carry private helpers, keep
names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]`
(the source module has both and uses `.expect()`).

**Acceptance criteria.**
1. `documents.rs` has no `#[cfg(test)]` block; line count drops by the moved test lines.
2. `cargo test -p mcp` test count unchanged (19 relocated); all gates green.
3. Names/assertions verbatim; scope-only diff; `../synopsis` untouched.

**Revision 1 (2026-09-01, human "revise").** The new file's module doc comment must not
carry a relative `../synopsis/…` path (for consistency with the other relocated test
files, 1.2/1.3, which do not reference the oracle by path). Replace the
`../synopsis/internal/mcp/handlers/…` reference with a path-free description (e.g.
"oracle: Go `internal/mcp/handlers/documents.go`"). Doc comment only — no code change.

---

### Task 1.5 — Extract `mcp/src/tools/graph_tools.rs` tests → `mcp/tests/graph_tools.rs`

**Goal.** Shrink `crates/mcp/src/tools/graph_tools.rs` (1,181 lines, 61% test) by moving
its inline test module to a new integration test file.

**File scope.**
- `crates/mcp/src/tools/graph_tools.rs` (source)
- `crates/mcp/tests/graph_tools.rs` (new; do not modify other `mcp/tests/` files)

**Dependencies.** None.

**Approach.** All 17 tests are MOVABLE (public API only). Move the module to
`crates/mcp/tests/graph_tools.rs`, rewrite `use crate::…`/`use super::*` → `use mcp::…`
(the lib target is named `mcp`; `tools` and `tools::graph_tools` are both `pub`, so
`mcp::tools::graph_tools::…` is importable), carry private helpers, keep
names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]`
(the source module has both and uses `.expect()`).

**Acceptance criteria.**
1. `graph_tools.rs` has no `#[cfg(test)]` block; line count drops by the moved test lines.
2. `cargo test -p mcp` test count unchanged (17 relocated); all gates green.
3. Names/assertions verbatim; scope-only diff; `../synopsis` untouched.

---

### Task 1.6 — Extract `graph/src/cel.rs` tests → `graph/tests/cel.rs` (+ `cel` dev-dep)

**Goal.** Shrink `crates/graph/src/cel.rs` (1,740 lines, 52% test) by moving its inline
test module to a new integration test file.

**File scope.**
- `crates/graph/src/cel.rs` (source)
- `crates/graph/tests/cel.rs` (new; `graph/tests/` already exists — do not modify
  `linker_pipeline.rs`, `llm_linker_pipeline.rs`)
- `crates/graph/Cargo.toml` (add `cel` to `[dev-dependencies]` only)

**Dependencies.** None. (Task 1.8 touches a different graph file — no conflict.)

**Approach.** All 29 tests are MOVABLE, but the test module references the `cel` crate
directly (e.g. `cel::Program`, `cel::ExecutionError::function_error`), which integration
tests cannot reach through `graph`'s `[dependencies]` (design D6). Add
`cel = { workspace = true }` to `graph` `[dev-dependencies]` at the SAME version already
used in `[dependencies]` (no new package, no Cargo.lock change — verify with
`git diff --stat -- Cargo.lock` being empty). Then move the test module to
`crates/graph/tests/cel.rs`, rewrite `use crate::…`/`use super::*` → `use graph::…`
(the lib target is named `graph`; `cel` is `pub` in `graph`'s lib — confirm the exact
public path of the items the tests use), carry private helpers, keep
names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]`
(the source module has both and uses `.expect()`).

**Acceptance criteria.**
1. `cel.rs` has no `#[cfg(test)]` block; line count drops by the moved test lines.
2. `cargo test -p graph` test count unchanged (29 relocated); all gates green.
3. `git diff --stat -- Cargo.lock` is empty (no package added). Names/assertions
   verbatim; scope-only diff; `../synopsis` untouched.

**Revision 1 (2026-09-01, human chose option B).** The `cel` dev-dep added by the
original approach is REDUNDANT: Cargo makes a crate's `[dependencies]` available to all
targets including integration tests, and the sibling `graph/tests/{linker_pipeline,
llm_linker_pipeline}.rs` already use `db::`/`config::` (both `[dependencies]`) directly.
So `tests/cel.rs` reaches `cel::Value`/`cel::Program`/`cel::ExecutionError` without a
dev-dep. REMOVE the `cel = { workspace = true }` line (and its comment) from
`crates/graph/Cargo.toml` `[dev-dependencies]`; re-verify `tests/cel.rs` still compiles
and all 29 tests pass without it. Design D6's premise ("integration tests cannot reach
`[dependencies]`") is corrected accordingly.

---

### Task 1.7 — Extract `search/src/hybrid.rs` tests → `search/tests/hybrid_units.rs`

**Goal.** Shrink `crates/search/src/hybrid.rs` (1,142 lines, 68% test) by moving 12 of its
15 tests to a new integration test file; 3 stay inline.

**File scope.**
- `crates/search/src/hybrid.rs` (source)
- `crates/search/tests/hybrid_units.rs` (new; `search/tests/` already exists — do not
  modify `hybrid_integration.rs`)

**Dependencies.** None.

**Approach.** Move the 12 MOVABLE tests (all except the three below) to
`crates/search/tests/hybrid_units.rs`, rewriting `use crate::…`/`use super::*` →
`use search::…` (the lib target is named `search`; `hybrid` is `pub mod hybrid;`, so
`search::hybrid::…` is importable), carrying their EXCLUSIVE private helpers (helpers
used only by a moved test move with it; a helper shared with a stay-inline test stays
inline), keeping names/assertions verbatim, `#![allow(clippy::unwrap_used,
clippy::expect_used)]` at the top of the new file (carry whichever of the two the moved
tests actually use; the source module has both and uses `.expect()`).
**Stay inline (do NOT move)** — they use private items (design D7):
- `invert_score_reciprocal` (uses private `invert_score`, `hybrid.rs:361`)
- `standalone_results_maps_hits` (uses private `standalone_results`, `hybrid.rs:336`)
- `hybrid_fusion_pool_is_max_of_leg_tops` (uses private `HybridSearcher::fusion_pool`,
  `hybrid.rs:203` — Appendix A misclassified it MOVABLE; an integration test cannot reach
  the private method, E0624)
Leave those three tests (and the private fns they need, which are production items that
stay in `hybrid.rs`) in `hybrid.rs` in a trimmed `#[cfg(test)] mod tests` (keep that
module's `#![allow(clippy::unwrap_used, clippy::expect_used)]`). Overlap note (design D4): moved
tests overlap `hybrid_integration.rs` coverage — MOVE them (do not remove); only if a
moved test is a true name+body duplicate of an existing integration test, remove the
inline copy and note it.

**Acceptance criteria.**
1. `hybrid.rs` retains exactly the 3 named stay-inline tests in its `#[cfg(test)]` block;
   line count drops by the 12 moved tests' lines.
2. `cargo test -p search` test count unchanged (12 relocated); all gates green.
3. Names/assertions verbatim; scope-only diff; `../synopsis` untouched.

**Revision 1 (2026-09-01, human chose option A).** Appendix A misclassified
`hybrid_fusion_pool_is_max_of_leg_tops` as MOVABLE; it calls the private
`HybridSearcher::fusion_pool` (`hybrid.rs:203`), which an integration test cannot reach
(E0624, verified by the implementer). It STAYS inline with the other two, so the move is
12 (not 13). No production-code change (no API widening, per D7). Design Appendix A
corrected: 13→12 MOVABLE, 2→3 STAY_INLINE, Total 173→172 / 33→34.

---

### Task 1.8 — Extract `graph/src/linker.rs` tests → `graph/tests/linker_units.rs`

**Goal.** Shrink `crates/graph/src/linker.rs` (1,809 lines, 53% test) by moving 7 of its
16 tests to a new integration test file; 9 stay inline.

**File scope.**
- `crates/graph/src/linker.rs` (source)
- `crates/graph/tests/linker_units.rs` (new; do not modify other `graph/tests/` files)

**Dependencies.** None.

**Approach.** Move the 7 MOVABLE tests to `crates/graph/tests/linker_units.rs`:
`equals_links_matching_names_and_skips_short_or_different_names`,
`equals_respects_configured_min_words`,
`expression_rule_creates_link_with_rule_attributes`,
`expression_priority_order_and_first_true_wins`,
`expression_errors_are_recorded_not_fatal`, `method_order_from_config_is_respected`,
`self_link_never_created` (all use public `build_entity_links` (`linker.rs:819`) + public
config + local helpers). Rewrite `use crate::…`/`use super::*` → `use graph::…` (the lib
target is named `graph`; `linker` is `pub mod linker;`, so `graph::linker::…` is
importable), carry their EXCLUSIVE private helpers (a helper used only by a moved test
moves; one shared with a stay-inline test stays inline), keep names/assertions verbatim,
`#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of the new file (carry
whichever the moved tests actually use; the source module has both).
**Stay inline (do NOT move)** — 9 tests using private items (design D7): the two
`cross_domain_pairs` tests (private fn) and the seven `llm_*` tests (private `MockLlm`
struct, defined at `linker.rs:1283`). Identify them by grepping the test module for
`MockLlm` and `cross_domain_pairs`. Leave them (plus `MockLlm` and the private fn) in a
trimmed `#[cfg(test)] mod tests`. Overlap note (design D4): equals/expression tests
overlap `linker_pipeline.rs` — MOVE them; remove only on true name+body duplicate (note
it).

**Acceptance criteria.**
1. `linker.rs` retains exactly the 9 stay-inline tests (grep-verified: every remaining
   test references `MockLlm` or `cross_domain_pairs`); line count drops by the 7 moved
   tests' lines.
2. `cargo test -p graph` test count unchanged (7 relocated); all gates green.
3. Names/assertions verbatim; scope-only diff; `../synopsis` untouched.

---

### Task 1.9 — Extract `llm/src/client.rs` tests → `llm/tests/client.rs` (+ `llm` test_support, new `tests/` dir)

**Goal.** Shrink `crates/llm/src/client.rs` (1,401 lines, **84% test** — the worst file in
the workspace) by moving 32 of its 34 tests to a new integration test file; 2 stay inline.
This crate has no `tests/` directory yet — create it.

**File scope.**
- `crates/llm/src/client.rs` (source)
- `crates/llm/src/lib.rs` (declare `#[doc(hidden)] pub mod test_support;`)
- `crates/llm/src/test_support.rs` (new, design D3)
- `crates/llm/tests/client.rs` (new; `crates/llm/tests/` directory is new)

**Dependencies.** None.

**Approach.**
1. **test_support (design D3):** in `client.rs`, drop the `#[cfg(test)]` gate from
   `with_sleeper` (keep `pub(crate)`) and widen private `backoff_delay` to `pub(crate)`.
   Create `crates/llm/src/test_support.rs` exposing exactly `with_sleeper` +
   `backoff_delay` and declare it in `lib.rs` as `#[doc(hidden)] pub mod test_support;`.
   **Both items are inherent methods on `LlmClient`**, so `pub use` cannot re-export them
   (E0432 — verified in a scratch crate); instead define two thin `pub fn` wrappers that
   delegate to the `pub(crate)` methods: `with_sleeper(client, sleeper) -> LlmClient` and
   `backoff_delay(&client, retry) -> Duration`. (The wrappers are public and call the
   `pub(crate)` methods, so no `dead_code` warnings in production builds; verify with
   clippy.)
2. **Move 32 tests** (all except the 2 below) to `crates/llm/tests/client.rs`: the lib
   target is `llm`; `mod client;` is PRIVATE, so reference the lib-root re-exports —
   `llm::LlmClient`, `llm::LlmError` (NOT `llm::client::…`, which won't resolve) — plus
   `config::preset::{LlmConfig, ResponseFormat}`. Carry exclusive private helpers, keep
   names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the
   top of the new file (the source module has both). The seam-based tests now call
   `llm::test_support::with_sleeper(…)`; the backoff test calls
   `llm::test_support::backoff_delay(…)`.
3. **Stay inline (do NOT move)** — read the private field `LlmClient.config` (design D7):
   `new_accepts_valid_config` (:651), `new_accepts_zero_max_retries` (:739). Leave them in
   a trimmed `#[cfg(test)] mod tests`.

**Acceptance criteria.**
1. `client.rs` retains exactly the 2 named stay-inline tests; line count drops from 1,401
   toward ≤ ~500.
2. `cargo test -p llm` test count unchanged (32 relocated); all gates green.
3. `test_support` exposes exactly `with_sleeper` + `backoff_delay`, both `#[doc(hidden)]`
   at module level; no other visibility changes. Names/assertions verbatim; scope-only
   diff; `../synopsis` untouched.

**Revision 1 (2026-09-01, implementer correction, reviewer-approved).** The original
step 1 prescribed `#[doc(hidden)] pub use crate::client::{backoff_delay, with_sleeper};`,
but both items are inherent methods on `LlmClient`, and `pub use` cannot re-export
inherent methods (E0432 — the implementer verified this in a scratch crate before
editing). Step 1 now prescribes two thin `pub fn` wrappers delegating to the
`pub(crate)` methods; the moved tests call `llm::test_support::with_sleeper(…)` /
`llm::test_support::backoff_delay(…)` exactly as step 3 already prescribed. No production
logic changed. `client.rs` lands at 642 lines (the "≤ ~500" target is a soft "toward";
the ~592-line production body is the floor and cannot shrink without production changes).

---

### Task 1.10 — Extract `mcp/src/transport/sse.rs` tests → `mcp/tests/sse_units.rs` (+ `mcp` test_support)

**Goal.** Shrink `crates/mcp/src/transport/sse.rs` (1,312 lines, 57% test) by moving 18 of
its 23 tests to a new integration test file; 5 stay inline.

**File scope.**
- `crates/mcp/src/transport/sse.rs` (source)
- `crates/mcp/src/lib.rs` (declare `#[doc(hidden)] pub mod test_support;`)
- `crates/mcp/src/test_support.rs` (new, design D3)
- `crates/mcp/tests/sse_units.rs` (new; do not modify `server_integration.rs`)

**Dependencies.** None. (Tasks 1.4/1.5 already added other `mcp/tests/` files — no
conflict; none of them touch `lib.rs`.)

**Approach.**
1. **test_support (design D3):** widen private `endpoint_url`, `encode_sse_frame` (fns)
   and `CHANNEL_CAPACITY` (const) in `sse.rs` to `pub(crate)`. Create
   `crates/mcp/src/test_support.rs` exposing the three and declare the module in `lib.rs`
   as `#[doc(hidden)] pub mod test_support;`. **A `pub use` cannot re-export a
   `pub(crate)` item so it is visible to integration tests (E0364 — verified in a scratch
   crate)**, so instead define thin delegates: `pub fn endpoint_url(…)` /
   `pub fn encode_sse_frame(…)` calling the `pub(crate)` fns, and
   `pub const CHANNEL_CAPACITY: usize = crate::transport::sse::CHANNEL_CAPACITY;` (a
   compile-time REFERENCE to the production const, so it stays in sync — not a literal
   copy). (The delegates are public and call `pub(crate)` items in-crate, so no
   `dead_code` warnings in production builds; verify with clippy.)
2. **Move 18 tests** (all except the 5 stay-inline below) to
   `crates/mcp/tests/sse_units.rs`. **11 use only the public `SseSessionMap` /
   `handle_sse`:** `session_map_create_get_remove_touch_round_trip`,
   `session_map_unknown_id_get_none_touch_false`, `session_ids_are_unique_uuid_v4`,
   `send_to_dropped_receiver_removes_session_and_errors`,
   `sse_endpoint_serves_endpoint_event_and_cleans_up_on_drop`,
   `channel_payload_streams_as_message_frame`, `reaper_reaps_idle_session_after_threshold`,
   `reaper_keeps_touched_session`, `reaper_keeps_fresh_session`,
   `spawn_reaper_twice_is_harmless`, `idle_sse_stream_ends_after_threshold`.
   **7 use the `test_support` items** (rewrite the call to `mcp::test_support::…`):
   `endpoint_url_no_proxy_headers_uses_http_and_host`,
   `endpoint_url_uses_forwarded_proto_and_host`,
   `endpoint_url_comma_list_proto_first_value_wins` (→ `endpoint_url`);
   `endpoint_frame_bytes_match_wire_contract`, `message_frame_bytes_match_wire_contract`,
   `multiline_data_is_split_into_data_fields` (→ `encode_sse_frame`);
    `backpressure_channel_is_bounded` (→ `CHANNEL_CAPACITY`).
    Rewrite imports (`crate::…` → `mcp::…`; the lib target is `mcp` and `sse` is
    `pub mod sse`), carry exclusive private helpers, keep names/assertions verbatim,
    `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the top of the new file
    (the source module has both).
3. **Stay inline (do NOT move)** — need `test_server`, which is `#[cfg(test)]` inside the
   PRIVATE module `crate::transport::test_util` (`transport/mod.rs:65`), so it is not
   reachable from an integration test (which links the non-test lib build); `handle_message`
   takes `State<SseState { sessions, server: Arc<Server> }>` (design D7):
   `message_without_session_id`, `message_with_unknown_session_id`,
   `message_with_malformed_body`, `message_round_trip`, `touched_sse_stream`. Leave them
   (plus `test_server`) in a trimmed `#[cfg(test)] mod tests`. Overlap note (design D4):
   moved tests overlap `server_integration.rs` / `sse_parity.rs` — MOVE them; remove only
   on true name+body duplicate (note it).

**Acceptance criteria.**
1. `sse.rs` retains exactly the 5 named stay-inline tests; line count drops by the 18
   moved tests' lines.
2. `cargo test -p mcp` test count unchanged (18 relocated); all gates green.
3. `test_support` exposes exactly the 3 named items, `#[doc(hidden)]`; no other
   visibility changes. Names/assertions verbatim; scope-only diff; `../synopsis`
   untouched.

**Revision 1 (2026-09-01, implementer correction, reviewer-approved).** The original
step 1 prescribed `#[doc(hidden)] pub use crate::transport::sse::{…}`, but a `pub use`
cannot re-export a `pub(crate)` item so it is visible to integration tests (E0364 — the
implementer verified this in a scratch crate before editing). Step 1 now prescribes thin
delegates: two `pub fn` wrappers calling the `pub(crate)` fns, and a `pub const
CHANNEL_CAPACITY` that is a compile-time REFERENCE to the production const (stays in
sync, not a copy). The moved tests call `mcp::test_support::…` exactly as step 2
prescribed. Separately, the 3 moved endpoint_url tests reference the private production
consts `X_FORWARDED_PROTO`/`X_FORWARDED_HOST`; since only the 3 D3 items may be widened,
the integration file defines local `const`s with the identical `HeaderName::from_static`
values (byte-identical, reviewer-verified) and the test bodies remain verbatim. No
production logic changed.

---

### Task 1.11 — Extract `ingestion/src/ingester/mod.rs` tests → `ingestion/tests/ingester.rs` (+ `ingestion` test_support)

**Goal.** Shrink `crates/ingestion/src/ingester/mod.rs` (1,526 lines, 70% test) by moving
ALL 23 of its tests to a new integration test file.

**File scope.**
- `crates/ingestion/src/ingester/mod.rs` (source)
- `crates/ingestion/src/lib.rs` (declare `#[doc(hidden)] pub mod test_support;`)
- `crates/ingestion/src/test_support.rs` (new, design D3)
- `crates/ingestion/tests/ingester.rs` (new; `ingestion/tests/` already exists — do not
  modify `parity_fixtures.rs`, `pipeline_e2e.rs`)

**Dependencies.** None. (Task 1.12 reuses this task's test_support — keep 1.11 before
1.12.)

**Approach.**
1. **test_support (design D3):** `walk_matched_files` is already `pub(crate)` in
   `crates/ingestion/src/parsers/mod.rs:54` — no visibility change. Create
   `crates/ingestion/src/test_support.rs` exposing it and declare it in `lib.rs` as
   `#[doc(hidden)] pub mod test_support;`. **A `pub use` cannot re-export a `pub(crate)`
   item so it is visible to integration tests (E0364 — the same issue task 1.10 hit)**, so
   define a thin `pub fn walk_matched_files(source_path, matches, visit, errors)` delegate
   that calls `crate::parsers::walk_matched_files(…)` (same 4-arg signature). (The delegate
   is public and calls a `pub(crate)` item in-crate, so no `dead_code` warnings in
   production builds; verify with clippy.)
2. **Move all 23 tests** to `crates/ingestion/tests/ingester.rs`: they reach
   `walk_matched_files` through the test-local `TestSource`/`Harness` fixtures — carry
   those fixtures (and any other exclusive private helpers) into the integration file and
   rewrite their one call path to `ingestion::test_support::walk_matched_files`. Keep
   names/assertions verbatim, `#![allow(clippy::unwrap_used, clippy::expect_used)]` at the
   top of the new file (the source module has both).
3. Overlap note (design D4): overlaps `pipeline_e2e.rs`/`parity_fixtures.rs` — MOVE;
   remove only on true name+body duplicate (note it).

**Acceptance criteria.**
1. `ingester/mod.rs` has no `#[cfg(test)]` block; line count drops by the 23 moved tests'
   lines.
2. `cargo test -p ingestion` test count unchanged (23 relocated); all gates green.
3. `test_support` exposes exactly `walk_matched_files`, `#[doc(hidden)]`; no visibility
   changes anywhere. Names/assertions verbatim; scope-only diff; `../synopsis`
   untouched.

---

### Task 1.12 — Extract `ingestion/src/runner/mod.rs` tests → `ingestion/tests/runner.rs`

**Goal.** Shrink `crates/ingestion/src/runner/mod.rs` (1,454 lines, 58% test) by moving
ALL 13 of its tests to a new integration test file.

**File scope.**
- `crates/ingestion/src/runner/mod.rs` (source)
- `crates/ingestion/tests/runner.rs` (new; do not modify other `ingestion/tests/` files)

**Dependencies.** **Task 1.11 must be complete** — it created
`crates/ingestion/src/test_support.rs` exposing `walk_matched_files`. Reuse it; do NOT
create or modify the test_support module or `ingestion/src/lib.rs`.

**Approach.** Move all 13 tests to `crates/ingestion/tests/runner.rs`: 12 reach
`walk_matched_files` through test-local fixtures (carry them, rewrite the call path to
`ingestion::test_support::walk_matched_files`); the 13th,
`detect_source_type_matches_the_oracle_cases` (:1058), uses a public fn directly. Keep
names/assertions verbatim, `#![allow(clippy::unwrap_used)]`. Overlap note (design D4):
overlaps `pipeline_e2e.rs`/`parity_fixtures.rs` — MOVE; remove only on true name+body
duplicate (note it).

**Acceptance criteria.**
1. `runner/mod.rs` has no `#[cfg(test)]` block; line count drops by the 13 moved tests'
   lines.
2. `cargo test -p ingestion` test count unchanged (13 relocated on top of task 1.11's 23);
   all gates green.
3. No changes to `test_support.rs` or `lib.rs` (reuse only). Names/assertions verbatim;
   scope-only diff; `../synopsis` untouched.

---

### Task 1.13 — Final verification: before/after report + full gates

**Goal.** Machine-verify the whole change: file shrinkage, test-count invariance, gates.

**File scope.**
- `openspec/changes/test-hygiene-phase-1/results.md` (new report)
- Read-only: all `crates/**` (measurement only — no code edits)

**Dependencies.** Tasks 1.1–1.12 all complete.

**Approach.** Produce `results.md` containing:
1. **Before/after table** for the 11 extracted files (1.2–1.12): total lines before
   (from design.md Appendix A / `git show` of pre-change blobs), after, and reduction %.
   Also `cli/src/serve/watcher.rs` listed as "not extracted (all private)" with its line
   count.
2. **Test-count reconciliation:** workspace total before (1,474, Appendix A) vs after,
   per crate; every delta explained (expected: 0 — relocations only; any true-duplicate
   removals per design D4 named with file:line).
3. **Residual inline test inventory:** for each file that kept a `#[cfg(test)]` block
   (`search/hybrid.rs`, `graph/linker.rs`, `llm/client.rs`, `mcp/transport/sse.rs`,
   `cli/serve/watcher.rs`, plus any others): test count + the private-item reason.
4. Gate outputs (commands + pass/fail).

**Acceptance criteria.**
1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`, `cargo check --workspace` all green.
2. Workspace test count == 1,474 (or every delta D4-documented in results.md).
3. Every one of the 11 extracted files is smaller than before; `results.md` committed
   with the change. No source files modified by this task; `../synopsis` untouched.
