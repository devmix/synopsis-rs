# Design: make-ci-gitea-compatible

## Context

`rebuild-ci-cd` (archived 2026-09-04) produced a two-file pipeline designed for GitHub
Actions:

- `ci.yml`: `checks` (fmt + clippy + test) + `coverage` (cargo-llvm-cov → lcov artifact
  via `actions/upload-artifact@v4`).
- `release.yml`: `build` (5-way matrix, each leg `cargo zigbuild`s one target and
  `upload-artifact@v4`s the archive) + `release` (`needs: build`, `download-artifact@v4`
  collects all five, `SHA256SUMS.txt`, `softprops/action-gh-release@v3` publishes).

The repo is hosted on **Gitea** (`origin` = `git@git.lan`). Gitea's Actions runner
(`act_runner`) is GitHub-compatible but has **no artifact service**: the
`@actions/artifact` backend that powers `upload-artifact@v4` / `download-artifact@v4`
is unavailable, so those actions abort with `GHESNotSupportedError`. Consequences:

1. `ci.yml` `coverage` job's final step fails (observed).
2. `release.yml` cannot pass the five archives from the matrix legs to the `release`
   job — the standard mechanism does not exist on Gitea.

The legacy Go project's CI (read-only reference) sidesteps this: its `release.yml` runs
**one job** (GoReleaser builds every platform, packages, and publishes on the same
runner), so it never passes files between jobs.

## Decisions

### D1 — `release.yml` is a single job (no matrix, no cross-job passing)

Collapse `build` (5-way matrix) + `release` into **one `release` job**. The job
installs the toolchain with all five targets, then loops over the five targets:
`cargo zigbuild` → `zig objcopy --strip-all` → package `synopsis_<version>_<name>.<ext>`
(tar.gz, or zip for Windows) → next. After the loop it writes `SHA256SUMS.txt` and
publishes. All work happens on one runner, so no artifact passing is needed.

- **Why:** Gitea has no artifact service; a single job is the only way to share files
  between the build and the publish without an external store. It mirrors the legacy
  Go project's single-job shape.
- **Tradeoff (accepted):** the five targets build **sequentially** on one runner instead
  of in parallel. For an occasional `v*` release on a personal project this is fine;
  the dev CI (push/PR) is unaffected and stays fast.

### D2 — Publish via `akkuman/gitea-release-action@v1`

Replace `softprops/action-gh-release@v3` with `akkuman/gitea-release-action@v1`, a
Gitea-compatible fork of `softprops/action-gh-release`. Inputs used:

- `tag_name: ${{ github.ref_name }}`
- `files:` — newline-delimited globs `dist/synopsis_*.tar.gz`, `dist/synopsis_*.zip`,
  `dist/SHA256SUMS.txt` (the action uploads each as a release asset).
- `prerelease: ${{ contains(github.ref_name, '-') }}` — a `vX.Y.Z-rc*` (any dash-
  suffixed) tag is a prerelease; a plain `vX.Y.Z` is not. (The original `prerelease:
  auto` is a softprops-specific feature; the fork takes a boolean, so it is computed.)
- `body` / `name` omitted → empty body, name defaults to the tag (design D3 of
  `rebuild-ci-cd`: manual/empty body).
- `env: NODE_OPTIONS: '--experimental-fetch'` — the action uses `fetch`; the README
  sets this for Node < 18. It is a harmless no-op on Node ≥ 18, so it is set to cover
  both.

`SHA256SUMS.txt` is still generated in-shell (`sha256sum synopsis_*.tar.gz
synopsis_*.zip`), exactly as before, and shipped as an asset — the action's own
`sha256sum` input is NOT used (to keep the file name/format identical to the
`rebuild-ci-cd` contract).

### D3 — `ci.yml` `coverage` uses `ChristopherHX/gitea-upload-artifact@v4`

Replace `actions/upload-artifact@v4` with `ChristopherHX/gitea-upload-artifact@v4` —
a fork of `upload-artifact@v4` that removes the GHES/Gitea abort. The `name:
coverage-lcov` / `path: lcov.info` interface is unchanged, so the lcov artifact is
preserved (the user chose to keep it rather than drop it).

## Cross-build matrix → archive mapping (unchanged from `rebuild-ci-cd`)

| target | `<name>` | `<ext>` |
|---|---|---|
| `x86_64-unknown-linux-musl` | `linux_amd64` | `tar.gz` |
| `aarch64-unknown-linux-gnu` | `linux_arm64_gnu` | `tar.gz` |
| `aarch64-unknown-linux-musl` | `linux_arm64_musl` | `tar.gz` |
| `x86_64-pc-windows-gnu` | `windows_amd64` | `zip` |
| `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |

## Action pins (verified 2026-09-04)

- `actions/checkout@v7`, `dtolnay/rust-toolchain@1.96.0`, `Swatinem/rust-cache@v2`,
  `taiki-e/install-action@v2` (cargo-llvm-cov), `mlugg/setup-zig@v2` (Zig `0.16.0`) —
  unchanged, already fetchable by the Gitea runner.
- **New:** `ChristopherHX/gitea-upload-artifact@v4` (ci.yml), `akkuman/gitea-release-action@v1`
  (release.yml). Both are GitHub-hosted; the Gitea runner already fetches GitHub-hosted
  actions (checkout, rust-toolchain, …), so these are fetchable the same way.

## Risks / prerequisites (outside this change's file scope)

- **Runner server URL:** `gitea-upload-artifact` (per its README) requires the
  `act_runner` to be configured with the **external public Gitea URL**; otherwise the
  returned artifact URLs are wrong. This is an admin config, not a workflow edit.
- **`zip` availability:** the Windows leg uses `zip`. It is present on GitHub's
  `ubuntu-latest`; if the Gitea runner image lacks it, only the Windows leg fails
  (the other four still build/package). Same assumption as `rebuild-ci-cd`.
- **Node version:** `gitea-release-action` needs `fetch`; covered by the
  `NODE_OPTIONS: '--experimental-fetch'` env (D2).
- **Third-party actions:** both new actions are third-party forks (12★ / 16★). They are
  the user's chosen option (B + B); the alternative (Gitea API via `curl`) was
  declined for this change.
