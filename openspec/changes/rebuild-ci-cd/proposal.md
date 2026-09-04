# Proposal: rebuild-ci-cd

## Why

The current Rust CI (`.github/workflows/ci.yml`) diverges from the structure of the
legacy Go project it replaced. It runs the 5-target cross-build on **every** push/PR,
and it has **no release/CD pipeline** — there is no way to publish the cross-built
binaries. The legacy Go project's CI/CD was deliberately structured as:

- **(a) a fast dev CI** (`ci.yml`): test + lint + coverage, with the cross-build
  deliberately kept out — *"the full zig cross-build runs in release.yml at tag time,
  when it actually matters. Keeping dev runs fast."*
- **(b) a release pipeline** (`release.yml`): on `v*` tags, cross-built all platforms,
  packaged per-platform archives + `SHA256SUMS.txt`, and published a GitHub Release.

To restore that structure for the Rust workspace ahead of the legacy Go project's
deletion, the CI/CD is rebuilt to mirror it (user decision 2026-09-04).

## What

- **Restructure `ci.yml`:** remove the `cross-builds` job; keep `checks`
  (fmt + clippy + test — the native build check) and `coverage` (llvm-cov → lcov
  artifact). Update the header comment to state the new philosophy (cross-builds now
  live in `release.yml` at tag time).
- **Add `release.yml`:** on `push: tags: v*`, cross-build the 5 targets via
  `cargo zigbuild` (reusing the pinned toolchain 1.96.0 + cargo-zigbuild + Zig
  0.16.0), strip symbols, package each as `synopsis_<version>_<os>_<arch>.tar.gz`
  (`.zip` for Windows) bundling the binary + `README.md` + `workspace/configs/**` +
  `workspace/datasets/edtech/ontology/**`, generate `SHA256SUMS.txt`, and publish via
  `softprops/action-gh-release@v3` with a manual/empty body (just the tag).

**Frozen contracts:** NO behavior change. This change touches only CI/CD pipeline
files (`.github/workflows/`); it does not touch the MCP tools, CLI surface, data
schema, or config format. Parity is confirmed by the gates (fmt/clippy/test stay
green) and by the fact that no Rust source, dependency, or contract spec changes.

## Non-goals

- **NOT a change to any frozen contract** (MCP tools / CLI / data schema / config
  format) — CI/CD pipeline only.
- **NOT a behavior or dependency change** — no new crates, no code change; workflow
  YAML only.
- **NOT the ONNX runtime `.so` or model files in the archive** — those are gitignored
  runtime artifacts downloaded by the binary at runtime per `onnx.yaml` (frozen-stack
  design); the archive ships only the binary + README + `workspace/configs/**` +
  `workspace/datasets/edtech/ontology/**`.
- **NOT the demo corpus** (`workspace/datasets/edtech/content/**`) in the archive —
  the legacy archive did not ship a demo corpus.
- **NOT signing** of release artifacts (personal project; no cosign/sigstore).
- **NOT release-plz / auto semver / auto CHANGELOG** — manual `v*` tags, manual/empty
  release body.
- **NOT a `justfile`/Makefile** for local builds — the plain-cargo commands in
  `AGENTS.md` remain the build system.
- **NOT the `.archive/spikes/` historical archive** or the legacy Go project
  (read-only reference only).
