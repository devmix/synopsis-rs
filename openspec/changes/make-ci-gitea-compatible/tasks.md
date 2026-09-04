# Tasks: make-ci-gitea-compatible

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. This change is
**CI/CD pipeline only** — it edits two workflow files to be Gitea-compatible; it touches
no Rust source, no dependency, and no frozen contract. Every task is self-contained for
a fresh agent (~100k context): goal, exact file scope, dependencies, and
machine-checkable acceptance all fit in the body.

## Rule (summary — full detail in design.md)

- **D1 (single job):** `release.yml` becomes ONE `release` job (no matrix, no
  `upload/download-artifact`). It builds all 5 targets sequentially, packages each
  archive, writes `SHA256SUMS.txt`, and publishes on the same runner.
- **D2 (publish action):** publish via `akkuman/gitea-release-action@v1` (NOT
  `softprops/action-gh-release`). `tag_name: ${{ github.ref_name }}`, `files:` = the
  three globs, `prerelease: ${{ contains(github.ref_name, '-') }}`, empty body, and
  `env: NODE_OPTIONS: '--experimental-fetch'`.
- **D3 (coverage action):** `ci.yml` `coverage` job uses
  `ChristopherHX/gitea-upload-artifact@v4` (NOT `actions/upload-artifact@v4`), same
  `name`/`path`.
- **Cross-build matrix → archive mapping (5 targets):**
  | target | `<name>` | `<ext>` |
  |---|---|---|
  | `x86_64-unknown-linux-musl` | `linux_amd64` | `tar.gz` |
  | `aarch64-unknown-linux-gnu` | `linux_arm64_gnu` | `tar.gz` |
  | `aarch64-unknown-linux-musl` | `linux_arm64_musl` | `tar.gz` |
  | `x86_64-pc-windows-gnu` | `windows_amd64` | `zip` |
  | `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |
- **Action pins (verified 2026-09-04):** `actions/checkout@v7`,
  `dtolnay/rust-toolchain@1.96.0`, `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2`,
  `mlugg/setup-zig@v2` (Zig `0.16.0`), `ChristopherHX/gitea-upload-artifact@v4`,
  `akkuman/gitea-release-action@v1`.
- **Reference (read-only, do NOT modify):** legacy Go CI/CD at
  `/mnt/local/secure/projects/devmix/synopsis/.github/workflows/{ci.yml,release.yml}`.
- **Gates:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` stay green (workflow-only change; these must
  not regress).

## 1 — Pipeline files

- [ ] 1.1 Make `ci.yml` `coverage` job Gitea-compatible (swap the artifact action)

**Goal.** In `.github/workflows/ci.yml`, the `coverage` job's final step "Upload lcov
artifact" currently uses `actions/upload-artifact@v4`, which aborts on Gitea with
`GHESNotSupportedError`. Replace it with `ChristopherHX/gitea-upload-artifact@v4` (a
fork of `upload-artifact@v4` that does not abort on Gitea). Keep the step's `name:`
(`coverage-lcov`) and `path:` (`lcov.info`) exactly the same. Update the step's comment
so it states this is the Gitea-compatible fork (because `actions/upload-artifact@v4`
is not supported on the self-hosted Gitea runner). Do NOT change any other job or step
(`checks` stays as-is; the coverage measurement step `cargo llvm-cov …` stays as-is).

**Scope (exact file).** `.github/workflows/ci.yml` only.

**Dependencies.** None.

**Acceptance.**
- `grep -c 'ChristopherHX/gitea-upload-artifact@v4' .github/workflows/ci.yml` → **1**.
- `grep -c 'actions/upload-artifact' .github/workflows/ci.yml` → **0** (standard action gone).
- `grep -c 'actions/download-artifact' .github/workflows/ci.yml` → **0**.
- The lcov step is intact: `grep -c 'name: coverage-lcov' .github/workflows/ci.yml` → 1 and `grep -c 'path: lcov.info' .github/workflows/ci.yml` → 1.
- `checks` job still present: `grep -cE '^  checks:' .github/workflows/ci.yml` → 1.
- YAML still parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"` → no error.
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

- [ ] 1.2 Rewrite `release.yml` as a single Gitea-compatible job

**Goal.** Replace `.github/workflows/release.yml` (currently a 2-job pipeline: a 5-way
`build` matrix that `upload-artifact@v4`s each archive + a `release` job that
`download-artifact@v4`s them) with a **single `release` job** that does everything on one
runner (Gitea has no artifact service, so files cannot be passed between jobs — see
design D1). The job:
1. `name: Release`. Trigger `on: push: tags: ['v*']`. Top-level `permissions:
   contents: write`.
2. Steps, in order:
   - checkout (`actions/checkout@v7`, `fetch-depth: 0`);
   - install toolchain (`dtolnay/rust-toolchain@1.96.0`) with **all five** targets in the
     `targets:` input (comma-separated): `x86_64-unknown-linux-musl, aarch64-unknown-linux-gnu, aarch64-unknown-linux-musl, x86_64-pc-windows-gnu, aarch64-apple-darwin`;
   - `Swatinem/rust-cache@v2`;
   - `cargo install --locked cargo-zigbuild`;
   - `mlugg/setup-zig@v2` with `version: "0.16.0"`;
   - **one shell step** (`set -euo pipefail`) that loops over the five targets and, for
     each: `cargo zigbuild --release --target <target>`; set `bin=target/<target>/release/synopsis`
     (append `.exe` and use bin name `synopsis.exe` for `*-pc-windows-*`, else `synopsis`);
     `zig objcopy --strip-all "$bin"`; rebuild a clean `stage/` dir and copy the binary as
     `stage/<binname>`, `README.md`, `workspace/configs` → `stage/workspace/configs`,
     `workspace/datasets/edtech/ontology` → `stage/workspace/datasets/edtech/ontology`;
     package `dist/synopsis_${GITHUB_REF_NAME#v}_<name>.<ext>` (use `zip` when `<ext>` is
     `zip`, else `tar czf`). The target→name→ext rows are exactly the table in the Rule.
   - a **Generate checksums** step: `(cd dist && sha256sum synopsis_*.tar.gz synopsis_*.zip > SHA256SUMS.txt)`;
   - **Publish** via `akkuman/gitea-release-action@v1` with:
     `tag_name: ${{ github.ref_name }}`; `files:` = the three newline-delimited globs
     `dist/synopsis_*.tar.gz`, `dist/synopsis_*.zip`, `dist/SHA256SUMS.txt`;
     `prerelease: ${{ contains(github.ref_name, '-') }}`; NO `body` (empty); and step
     `env: NODE_OPTIONS: '--experimental-fetch'`.

**Scope (exact file).** `.github/workflows/release.yml` (overwrite). Do NOT modify
`ci.yml` in this task.

**Dependencies.** None (independent of task 1.1, but both must land for the pipeline to
work; run after 1.1 for a coherent review).

**Acceptance.**
- File parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"` → no error.
- Single job, no matrix: `grep -cE '^  [a-z-]+:' .github/workflows/release.yml` → **1** (exactly one top-level job under `jobs:`); `grep -c 'matrix' .github/workflows/release.yml` → **0**; `grep -c 'needs:' .github/workflows/release.yml` → **0**.
- No standard artifact actions and no softprops: `grep -cE 'actions/upload-artifact|actions/download-artifact|softprops/action-gh-release' .github/workflows/release.yml` → **0**.
- Publish action present: `grep -c 'akkuman/gitea-release-action@v1' .github/workflows/release.yml` → **1**.
- All five targets present: for each of the five targets in the table, `grep -c <target> .github/workflows/release.yml` ≥ 1.
- Build + package present: `grep -c 'cargo zigbuild' .github/workflows/release.yml` ≥ 1; `grep -c 'SHA256SUMS' .github/workflows/release.yml` ≥ 1.
- Tag trigger + permissions: `grep -c "'v\*'" .github/workflows/release.yml` → 1; `grep -c 'contents: write' .github/workflows/release.yml` → 1.
- No excluded data shipped: `grep -cE 'onnxruntime|workspace/models|datasets/edtech/content' .github/workflows/release.yml` → 0.
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

## 2 — Verification

- [ ] 2.1 Whole-pipeline Gitea-compatibility verification

**Goal.** Confirm both workflow files are Gitea-compatible: no standard artifact action
remains anywhere, the two Gitea forks are pinned correctly, the release is a single
job, and the archive contents are unchanged from `rebuild-ci-cd`.

**Scope (read-only).** `.github/workflows/ci.yml`, `.github/workflows/release.yml`.

**Dependencies.** Tasks 1.1 and 1.2.

**Acceptance.**
- Both parse: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); yaml.safe_load(open('.github/workflows/release.yml'))"` → no error.
- No standard artifact action anywhere: `grep -rcE 'actions/upload-artifact|actions/download-artifact' .github/workflows/` → **0** across both files.
- Gitea forks pinned: `grep -rc 'ChristopherHX/gitea-upload-artifact@v4' .github/workflows/` → **1** (ci.yml); `grep -rc 'akkuman/gitea-release-action@v1' .github/workflows/` → **1** (release.yml).
- Release is a single job: `grep -cE '^  [a-z-]+:' .github/workflows/release.yml` → 1; `grep -c 'matrix' .github/workflows/release.yml` → 0; `grep -c 'needs:' .github/workflows/release.yml` → 0.
- D4 completeness (unchanged): `release.yml` references `README.md`, `workspace/configs`, `workspace/datasets/edtech/ontology` (each `grep -c` ≥ 1) and `SHA256SUMS` (≥ 1); and does NOT reference `onnxruntime`, `workspace/models`, or `datasets/edtech/content` (`grep -cE … → 0`).
- No Go tooling: `grep -rciE 'goreleaser|golangci|setup-go|CGO_CFLAGS|CGO_ENABLED' .github/workflows/` → 0 across both files.
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- **Dev report:** list the exact archive names the single job produces for a sample tag (e.g. `v0.13.0`), and confirm each bundles the 4 file groups (binary, README, configs, ontology) + the single `SHA256SUMS.txt`, and that the publish step uploads exactly those 6 assets (5 archives + checksum).
