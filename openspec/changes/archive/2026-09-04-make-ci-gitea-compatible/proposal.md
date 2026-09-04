# Proposal: make-ci-gitea-compatible

## Why

The `rebuild-ci-cd` change (archived 2026-09-04) rebuilt the CI/CD to mirror the
legacy Go structure, but it was designed for **GitHub Actions**. The repository is
actually hosted on a **self-hosted Gitea** (`origin` = `git@git.lan`), and the
`rebuild-ci-cd` pipeline fails there:

- **`ci.yml` → `coverage` job** ends with `actions/upload-artifact@v4`. On Gitea the
  `@actions/artifact` backend is unavailable, so the step aborts with
  `GHESNotSupportedError: @actions/artifact v2.0.0+, upload-artifact@v4+ and
  download-artifact@v4+ are not currently supported on GHES`. This is the failing
  step observed in the Actions run ("Upload lcov artifact").
- **`release.yml`** (added by `rebuild-ci-cd`) uses a 5-way matrix where each leg
  `upload-artifact@v4`s an archive and a second `release` job `download-artifact@v4`s
  all five to collect them. Gitea has no artifact service, so **cross-runner file
  passing is impossible** with the standard actions — the whole release pipeline would
  fail on a `v*` tag.

The legacy Go project avoided this because **GoReleaser did everything in a single
job** (build all platforms → package → publish on one runner); it never passed files
between jobs.

## What

Make the pipeline Gitea-compatible (user decision 2026-09-04, option **B + B** — both
Gitea-compatible third-party actions):

- **`ci.yml` → `coverage` job:** replace `actions/upload-artifact@v4` with
  `ChristopherHX/gitea-upload-artifact@v4` (a fork of `upload-artifact@v4` that does
  not abort on Gitea). Same `name` / `path` interface; the lcov artifact is preserved.
- **`release.yml`:** collapse the two jobs (5-way `build` matrix + `release`) into a
  **single `release` job** that builds all 5 targets sequentially on one runner,
  packages each archive, generates `SHA256SUMS.txt`, and publishes via
  `akkuman/gitea-release-action@v1` (a Gitea-compatible fork of
  `softprops/action-gh-release`). No matrix, no `upload/download-artifact`.

**Frozen contracts:** NO behavior change. This change touches only CI/CD pipeline files
(`.github/workflows/`); it does not touch the MCP tools, CLI surface, data schema, or
config format. Parity is confirmed by the gates (fmt/clippy/test stay green) and by the
fact that no Rust source, dependency, or contract spec changes.

## Non-goals

- **NOT a change to any frozen contract** (MCP tools / CLI / data schema / config
  format) — CI/CD pipeline only.
- **NOT a behavior or dependency change** — no new crates, no code change; workflow
  YAML only.
- **NOT a switch to the Gitea API via `curl`** (option ①A) — the user chose the
  purpose-built actions (B + B) instead.
- **NOT the ONNX runtime `.so` or model files in the archive** — gitignored runtime
  artifacts downloaded at runtime per `onnx.yaml`; the archive ships only the binary +
  README + `workspace/configs/**` + `workspace/datasets/edtech/ontology/**`.
- **NOT the demo corpus** (`workspace/datasets/edtech/content/**`) in the archive.
- **NOT signing** of release artifacts (personal project; no cosign/sigstore).
- **NOT changing the Gitea runner's server URL** — the `act_runner` must be configured
  with the external public Gitea URL for `gitea-upload-artifact` to work (an admin
  prerequisite, outside this change's file scope).
