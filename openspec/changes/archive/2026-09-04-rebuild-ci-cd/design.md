# Design: rebuild-ci-cd

## Context

The Rust workspace replaced a legacy Go project. That project's CI/CD
(`/mnt/local/secure/projects/devmix/synopsis/.github/workflows/{ci.yml,release.yml}` +
`.goreleaser.yaml`, read-only reference) was split into a **fast dev CI** (test + lint +
coverage, no cross-build) and a **tag-time release** (cross-build + package + publish).
The Rust `ci.yml` currently does the cross-build in dev CI and has no release pipeline.
This change restores the two-file structure. Frozen stack + constraints: `openspec/config.yaml`.

## Decisions

### D1 — Cross-builds move out of CI into release.yml (fast dev CI)

- **Decision:** remove the `cross-builds` job from `ci.yml`; the 5-target cross-build
  lives only in `release.yml` (tag time). `ci.yml` keeps `checks` + `coverage`.
- **Why:** mirrors the legacy structure, which kept cross-builds out of dev CI
  ("Keeping dev runs fast"). `cargo test` (the `checks` job) already compiles + links
  the whole workspace natively, so it is the build check — the same role as the legacy
  `test` job.
- **Tradeoff (accepted):** the Rust cross-compile path (`zig cc` + bundled SQLite for
  musl/aarch64/windows/darwin) is *not* exercised by native `cargo test`, so a
  cross-compile break now surfaces only at tag time. Accepted: releases are infrequent
  for a personal project, and the legacy project made the same tradeoff.
- **Alternative rejected:** keeping cross-builds in CI (earlier breakage detection) —
  rejected to match the reference structure and keep dev runs fast.

### D2 — Release tooling: cargo zigbuild + softprops/action-gh-release@v3

- **Decision:** build with `cargo zigbuild --release` (the CI's existing cross-build
  tool), package with shell (`tar`/`zip` + `sha256sum`), publish with
  `softprops/action-gh-release@v3`.
- **Why:** there is no GoReleaser equivalent for Rust without adding a tool/dependency
  (frozen stack). `cargo zigbuild` + a release action is transparent and adds no
  dependency to `Cargo.toml`/`Cargo.lock`.
- **Version note (verified online 2026-09-04):** `softprops/action-gh-release`
  **v2.6.2 is the final v2 and is deprecated** (Node 20 runtime, no longer maintained);
  **v3** (v3.0.3, Node 24) is current. Pin `@v3`.
- **Alternative rejected:** release-plz (rejected per D3); goreleaser (Go-only, N/A).

### D3 — Trigger + versioning: manual v* tags, manual/empty body

- **Decision:** `release.yml` triggers on `push: tags: 'v*'`. No release-plz; the
  release body is manual/empty (just the tag). `prerelease` is `auto` (a
  `vX.Y.Z-rc*`/`vX.Y.Z-beta*` tag is marked a prerelease).
- **Why:** lightest, no extra tooling, fits "no external services". The legacy used
  GoReleaser's auto-changelog, but there is no GoReleaser for Rust; auto-changelog
  would require release-plz (rejected).
- **Alternative rejected:** release-plz auto semver + changelog — rejected (adds a
  tool; D3 = manual).

### D4 — Archive contents + naming: mirror the legacy layout

- **Contents (per archive):** the stripped binary, `README.md`, `workspace/configs/**`
  (onnx.yaml + presets + prompts), `workspace/datasets/edtech/ontology/**`. Plus one
  top-level `SHA256SUMS.txt` covering all archives.
- **Why:** mirrors the legacy `.goreleaser.yaml` archives (binary + README + configs +
  ontology data + `SHA256SUMS.txt`). The ONNX runtime `.so` and model files are
  gitignored runtime artifacts downloaded by the binary per `onnx.yaml` (frozen stack)
  — **not** shipped. The demo corpus is **not** shipped (legacy didn't ship one).
- **Naming:** `synopsis_<version>_<name>.<ext>`, where `<version>` is the tag with the
  leading `v` removed (tag `v0.13.0` → `synopsis_0.13.0_linux_amd64.tar.gz`). This
  mirrors the legacy `name_template: "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}"`.
- **Note:** there is **no `LICENSE` file** at the repo root (the legacy archive bundled
  one); the Rust archive omits it. If a LICENSE is added later, add it to the file list.

### Cross-build matrix → archive mapping (reused from ci.yml; 5 targets)

| zigbuild target | `<name>` | `<ext>` |
|---|---|---|
| `x86_64-unknown-linux-musl` | `linux_amd64` | `tar.gz` |
| `aarch64-unknown-linux-gnu` | `linux_arm64_gnu` | `tar.gz` |
| `aarch64-unknown-linux-musl` | `linux_arm64_musl` | `tar.gz` |
| `x86_64-pc-windows-gnu` | `windows_amd64` | `zip` |
| `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |

The two `aarch64` Linux targets are disambiguated by a `_gnu`/`_musl` suffix (the legacy
had only one `linux_arm64`). Windows uses `.zip` (GoReleaser convention); the others
use `.tar.gz`.

## Pipeline shape (release.yml)

Two jobs:
1. **`build`** — `strategy.matrix.include` of the 5 rows above (each row carries
   `target`, `name`, `ext`). Each leg: checkout → toolchain 1.96.0 (+ target std) →
   rust-cache → cargo-zigbuild → Zig 0.16.0 → `cargo zigbuild --release --target` →
   strip symbols → package `synopsis_<version>_<name>.<ext>` (binary + README +
   `workspace/configs/**` + `workspace/datasets/edtech/ontology/**`) → upload the
   archive as an Actions artifact.
2. **`release`** (`needs: build`) — checkout → download all 5 archives → generate
   `SHA256SUMS.txt` → publish via `softprops/action-gh-release@v3`
   (`files: dist/*`, `tag_name`, `prerelease: auto`, empty body). `permissions:
   contents: write`.

## Reference

- Legacy Go CI/CD (read-only): `/mnt/local/secure/projects/devmix/synopsis/.github/workflows/{ci.yml,release.yml}` + `.goreleaser.yaml`.
- Current Rust CI: `.github/workflows/ci.yml` — the `cross-builds` job is the source of the 5-target matrix + toolchain/zig setup reused in `release.yml`.
- No recorded fixtures / contract specs are the reference (no behavior change).
