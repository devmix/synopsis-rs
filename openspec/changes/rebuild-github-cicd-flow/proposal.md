# Proposal: rebuild-github-cicd-flow

## Why

The first GitHub release attempt (tag `v0.1.0`, run 34092788497, 2026-09-07) failed,
and the review of the whole pipeline surfaced three structural problems:

1. **`release.yml` is broken**: `zig objcopy --strip-all <bin>` fails with
   `error: expected output parameter` — verified against the Zig source
   (`lib/compiler/objcopy.zig`): unlike GNU `objcopy`, `zig objcopy` requires BOTH
   an input and an output file (no in-place mode). The release pipeline has never
   produced a single archive.
2. **`ci.yml` compiles the workspace twice per push**: two independent parallel jobs
   (`checks` = fmt + clippy + test, `coverage` = llvm-cov) each on their own runner,
   with their own toolchain install and cargo cache. For a personal project on the
   GitHub free tier, runner-minutes matter more than a few minutes of wall time.
3. **The Gitea-compatibility constraint is dead**: `make-ci-gitea-compatible`
   (archived 2026-09-04) collapsed the release pipeline into a single sequential job
   and swapped in Gitea-fork actions because CI ran on Gitea (`git@git.lan`). CI now
   runs on **GitHub Actions** (`.github/workflows/`, the `github` remote); there is no
   `.gitea/` directory, so no Gitea runner executes these workflows. The single-job
   design (sequential 5-target loop, ~25–30 min wall time) and the fork actions no
   longer serve a purpose.

The user approved (2026-09-07) a full restructure of the GitHub CI/CD flow.

## What

- **Fix `release.yml` strip step**: `zig objcopy --strip-all "${bin}" "${bin}.stripped"
  && mv "${bin}.stripped" "${bin}"`.
- **Rewrite `ci.yml`**: merge `checks` + `coverage` into ONE sequential job on one
  runner — one checkout, one toolchain install (components: `clippy`, `rustfmt`,
  `llvm-tools-preview`), one cargo cache. Steps: fmt → clippy → test →
  cargo-llvm-cov (pinned 0.9.0) → lcov upload. The coverage step keeps the
  measure-first intent via `continue-on-error: true`.
- **Rewrite `release.yml`**: three jobs — `gate` (fmt + clippy + test on the tagged
  commit: a release is never built from unverified code) → `build` (5-way matrix,
  each leg builds one target in parallel, strips, packages, uploads the archive as an
  artifact) → `publish` (`needs: build`; downloads the five archives, writes
  `SHA256SUMS.txt`, generates a categorized changelog from conventional commits,
  publishes).
- **Standard GitHub actions again**: `actions/upload-artifact@v4`,
  `actions/download-artifact@v4`, `softprops/action-gh-release@v3` replace the Gitea
  forks (`ChristopherHX/gitea-upload-artifact@v4`, `akkuman/gitea-release-action@v1`).
- **Refresh the cross-build target matrix** (user decision 2026-09-07: the service
  is used by other people on different platforms): drop both `*-musl` targets
  (slow to compile; fully-static benefit irrelevant for end-user machines), replace
  `x86_64-pc-windows-gnu` with the standard `x86_64-pc-windows-msvc` (cargo-zigbuild
  supports it; the "Zig cannot link MSVC ABI" note is stale), add
  `x86_64-apple-darwin` (Intel Macs). Final matrix: `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`,
  `x86_64-apple-darwin`.
- **`AGENTS.md` gotcha update**: the "CI is Gitea-compatible on purpose" bullet is
  replaced with the new GitHub-native description.

**Frozen contracts:** NO behavior change. This change touches only
`.github/workflows/{ci.yml,release.yml}` and the CI gotcha in `AGENTS.md`; it does not
touch the MCP tools, CLI surface, data schema, or config format, and no Rust source or
dependency changes. Parity is confirmed by the gates themselves (fmt/clippy/test stay
green in the new pipeline) and by the release pipeline producing the same archive
layout/naming as before (`synopsis_<version>_<name>.<ext>` + `SHA256SUMS.txt`).

## Non-goals

- **NOT a change to any frozen contract** (MCP tools / CLI / data schema / config
  format) — CI/CD pipeline + its AGENTS.md gotcha only.
- **NOT a behavior or dependency change** — no new crates, no Rust code change.
- **NOT release-plz / auto semver / auto version bumps** — manual `v*` tags remain the
  release trigger (design D3 of `rebuild-ci-cd` is preserved); only the release BODY
  becomes machine-generated from conventional commits.
- **NOT restoring Gitea CI** — the Gitea remote (`origin`, `git@git.lan`) stays a git
  mirror without CI; no `.gitea/workflows/` is created.
- **NOT the ONNX runtime `.so` or model files in the archive** (gitignored runtime
  artifacts downloaded per `onnx.yaml`), and **NOT the demo corpus** — archive
  CONTENTS are unchanged from `make-ci-gitea-compatible` D4 (only the `<name>`
  values follow the refreshed matrix).
- **NOT signing** of release artifacts (personal project; no cosign/sigstore).
- **NOT `workflow_run`-triggered releases** — the explicit `gate` job is the chosen
  mechanism (see design D3).
