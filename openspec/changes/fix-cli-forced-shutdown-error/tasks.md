# Tasks: a forced shutdown is not a failure

Change: `fix-cli-forced-shutdown-error`

## Change header (read first)

- **Goal:** make `serve_with_stop` return `Ok(())` (not `Err`) when the
  graceful-shutdown path exceeds its hard bound (a **forced shutdown**), so the
  CLI exit code matches the existing `warn!("forced shutdown after timeout")`.
  A real failure (serve task panic, setup/bind error) is still an error.
- **File scope (only this file):** `crates/cli/src/serve/server.rs` — the
  `serve_with_stop` function's shutdown tail (the `serve_result` match,
  currently `:643–650`). Do NOT touch other crates, the Go oracle
  (`../synopsis` is read-only), `SHUTDOWN_TIMEOUT`, the `Some(result)` arm, the
  log lines, or any test.
- **Context (read these first):**
  - This file, `server.rs` — `serve_with_stop` (`:302`), the shutdown tail
    (`:638–654`), `reap_serve` (`:677`), `SHUTDOWN_TIMEOUT` (`:106`), and the
    three `serve_with_stop` tests that assert `result.is_ok()`
    (`serve_starts_serves_health_and_stops` `:1041`,
    `serve_startup_reconcile_jobs_are_processed_by_the_worker` `:1134`,
    `serve_rebuilds_vectors_on_dimension_mismatch` `:1315`) plus the setup-error
    test `serve_mismatch_without_auto_rebuild_is_fatal` (`:1401`, expects
    `CliError::Unsupported` — a different path, unchanged).
  - `openspec/changes/fix-cli-forced-shutdown-error/proposal.md`.
  - `openspec/changes/fix-cli-forced-shutdown-error/design.md` (D1, the
    `serve_result` state analysis, the contract-impact note).
  - `openspec/config.yaml`, `AGENTS.md` (gates).
- **Gates (all must be green):** `cargo fmt --all --check`;
  `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo check --workspace`; `cargo test --workspace`.
- **Test-count invariant:** the workspace total stays **1,474** (no tests added
  or removed).

- [ ] **1.1** — Return `Ok(())` on a forced shutdown.

  **Goal:** close the `warn!`-then-`Err` inconsistency so a load-induced forced
  stop is not reported as a CLI failure.

  **File scope:** `crates/cli/src/serve/server.rs` only — the `serve_result`
  match in `serve_with_stop` (`:643–650`).

  **Change (exactly one arm):**

  Replace the `None` arm of the `serve_result` match:
  ```rust
  let serve_result = match serve_result {
      Some(result) => result,
      // The shutdown bound expired before the serve task drained; the
      // task is reaped when the runtime drops at the end of run_serve.
      None => Err(io::Error::other(
          "serve task did not finish within the shutdown bound",
      )),
  };
  ```
  with:
  ```rust
  let serve_result = match serve_result {
      Some(result) => result,
      // A forced shutdown (the bound expired before the serve task drained) is
      // not a failure: the process is about to exit, so the in-flight axum
      // drain is abandoned — that is the point of the forced bound. The warn!
      // above already logged it; returning Ok keeps the exit code clean (a
      // forced stop met the user's intent: the server stopped). The serve task
      // is reaped when the runtime drops at the end of run_serve.
      None => Ok(()),
  };
  ```

  **Notes / invariants:**
  - This is the ONLY change. The `Some(result)` arm is unchanged (a serve task
    that finished with an error — e.g. a panic folded by `reap_serve` — is still
    `Err`). `SHUTDOWN_TIMEOUT` (`:106`) is unchanged (oracle parity). The
    `info!`/`warn!`/`error!` log lines are unchanged. No other file, no test,
    no new import, no new dependency.
  - `io` (the `std::io` module) is still used elsewhere in the function (the
    bind `?`, `reap_serve`'s return type), so removing this one `io::Error` use
    does not make the import unused. If clippy/fmt report an unused import, the
    implementer must NOT add a workaround — report it instead (the import stays
    used by the bind + `reap_serve`).
  - Do NOT touch the three `result.is_ok()` tests or the
    `serve_mismatch_without_auto_rebuild_is_fatal` test (it expects
    `CliError::Unsupported`, a setup error on a different path).

  **Approach:** change the `None` arm to `Ok(())` with the explanatory comment,
  run the gates.

  **Acceptance (Критерии приёмки):**
  1. `cargo fmt --all --check` clean.
  2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
  3. `cargo check --workspace` clean.
  4. `cargo test -p cli` green (all cli tests, including the four
     `serve_with_stop` tests: `serve_starts_serves_health_and_stops`,
     `serve_startup_reconcile_jobs_are_processed_by_the_worker`,
     `serve_rebuilds_vectors_on_dimension_mismatch`, and
     `serve_mismatch_without_auto_rebuild_is_fatal`).
  5. `cargo test --workspace` green; **run it 15 consecutive times** and all
     fifteen must pass (the flake reproduced ~1-in-30 under load; the fix makes
     the `result.is_ok()` assertion robust to a forced stop).
  6. Workspace test count unchanged: 1,474 (report `cargo test --workspace --
     --list | grep -c ': test$'`).
  7. `git diff --name-status` shows only `crates/cli/src/serve/server.rs`
     modified (nothing under `../synopsis`, no new dependencies, no test
     changes, no `SHUTDOWN_TIMEOUT` change, no other file).

  **Oracle reference:** `../synopsis/cmd/app/serve.go` — the `shutdownCtx` 10 s
  bound (unchanged); the oracle's forced-stop exit semantics are not part of the
  frozen surface contract.
