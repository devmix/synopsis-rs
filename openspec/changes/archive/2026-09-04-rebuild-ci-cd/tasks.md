# Tasks: rebuild-ci-cd

Read first: `proposal.md`, `design.md`, and `openspec/config.yaml`. This change is
**CI/CD pipeline only** — it restructures `ci.yml` and adds `release.yml`; it touches
no Rust source, no dependency, and no frozen contract. Every task is self-contained for
a fresh agent (~100k context): goal, exact file scope, dependencies, and
machine-checkable acceptance all fit in the body.

## Rule (summary — full detail in design.md)

- **D1 (fast dev CI):** `ci.yml` keeps `checks` + `coverage`; the 5-target cross-build
  moves out of `ci.yml` and lives only in `release.yml` (tag time).
- **D2 (tooling):** build with `cargo zigbuild`, package with shell, publish with
  `softprops/action-gh-release@v3` (v2 is deprecated — do NOT use v2).
- **D3 (trigger/versioning):** `release.yml` triggers on `push: tags: 'v*'`; manual/empty
  body; `prerelease: auto`.
- **D4 (archives):** `synopsis_<version>_<name>.<ext>` bundling binary + `README.md` +
  `workspace/configs/**` + `workspace/datasets/edtech/ontology/**`, plus a top-level
  `SHA256SUMS.txt`. ONNX runtime `.so` + model files are NOT shipped (downloaded at
  runtime per `onnx.yaml`). No LICENSE (none exists at root).
- **Cross-build matrix → archive mapping (5 targets):**
  | target | `<name>` | `<ext>` |
  |---|---|---|
  | `x86_64-unknown-linux-musl` | `linux_amd64` | `tar.gz` |
  | `aarch64-unknown-linux-gnu` | `linux_arm64_gnu` | `tar.gz` |
  | `aarch64-unknown-linux-musl` | `linux_arm64_musl` | `tar.gz` |
  | `x86_64-pc-windows-gnu` | `windows_amd64` | `zip` |
  | `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |
- **Action versions (reuse the working `ci.yml` pins; verified online 2026-09-04):**
  `actions/checkout@v7`, `dtolnay/rust-toolchain@1.96.0`, `Swatinem/rust-cache@v2`,
  `mlugg/setup-zig@v2` (Zig `0.16.0`), `softprops/action-gh-release@v3`.
- **Reference (read-only, do NOT modify):** legacy Go CI/CD at
  `/mnt/local/secure/projects/devmix/synopsis/.github/workflows/{ci.yml,release.yml}` and
  `/mnt/local/secure/projects/devmix/synopsis/.goreleaser.yaml`.
- **Gates:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace` stay green (workflow-only change; these must
  not regress).

## 1 — Pipeline files

- [x] 1.1 Restructure `ci.yml` — drop `cross-builds`, keep `checks` + `coverage`

**Goal.** Remove the `cross-builds` job from `.github/workflows/ci.yml` so the dev CI is
fast (D1). Keep the `checks` job (fmt + clippy + test — the native build check) and the
`coverage` job (llvm-cov → lcov artifact) exactly as they are. Update the file's header
comment (lines 1–10) so it no longer describes a 3-job pipeline with a cross-build leg;
it should now say the pipeline is `checks` + `coverage`, and that the 5-target
cross-build + packaging now happens in `release.yml` at tag time (mirroring the legacy
Go structure — "Keeping dev runs fast").

**Scope (exact file).** `.github/workflows/ci.yml` only.

**Dependencies.** None.

**Acceptance.**
- `grep -c 'cross-builds' .github/workflows/ci.yml` → **0** (the job and its matrix are gone).
- `grep -c 'cargo zigbuild' .github/workflows/ci.yml` → **0** (no cross-build command left).
- `checks` and `coverage` jobs still present: `grep -cE '^  checks:|^  coverage:' .github/workflows/ci.yml` → **2**.
- The three gates still referenced: `grep -c 'cargo fmt --check' .github/workflows/ci.yml` → 1, `grep -c 'cargo clippy' .github/workflows/ci.yml` → 1, `grep -c 'cargo test' .github/workflows/ci.yml` → 1.
- YAML still parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"` → no error.
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

- [x] 1.2 Add `release.yml` — 5-target build + package + publish on `v*` tag

**Goal.** Create `.github/workflows/release.yml` — the CD pipeline (D1–D4). On `push:
tags: 'v*'` it cross-builds the 5 targets, packages per-platform archives, generates
`SHA256SUMS.txt`, and publishes a GitHub Release. Two jobs: `build` (5-way matrix) and
`release` (`needs: build`).

**Scope (exact file).** `.github/workflows/release.yml` (new file). Do NOT modify
`ci.yml` in this task.

**Dependencies.** Task 1.1 (so the cross-build matrix has a single home — `release.yml`).

**Requirements (all must hold):**
1. `name: Release`. Trigger: `on: push: tags: ['v*']`. Top-level `permissions:
   contents: write`. `fetch-depth: 0` on the checkout (so the tag is available).
2. **`build` job** — `runs-on: ubuntu-latest`, `strategy: fail-fast: false` with
   `matrix.include` = the 5 rows from the table above (each row: `target`, `name`,
   `ext`). Steps per leg:
   - checkout (`actions/checkout@v7`, `fetch-depth: 0`);
   - install toolchain (`dtolnay/rust-toolchain@1.96.0` with `targets: ${{ matrix.target }}`);
   - `Swatinem/rust-cache@v2`;
   - `cargo install --locked cargo-zigbuild`;
   - `mlugg/setup-zig@v2` with `version: "0.16.0"`;
   - `cargo zigbuild --release --target ${{ matrix.target }}`;
   - strip symbols from the built binary;
   - package `synopsis_${{ github.ref_name#v }}_${{ matrix.name }}.${{ matrix.ext }}`
     bundling: the binary, `README.md`, `workspace/configs/**`,
     `workspace/datasets/edtech/ontology/**` (use `tar czf` for `tar.gz`, `zip` for
     `zip`); output under `dist/`;
   - upload the archive as an Actions artifact (`actions/upload-artifact@v4`).
3. **`release` job** — `needs: [build]`, `runs-on: ubuntu-latest`. Steps:
   - checkout (`actions/checkout@v7`);
   - download all 5 build artifacts into `dist/` (`actions/download-artifact@v4`);
   - generate `dist/SHA256SUMS.txt` (`sha256sum` over the archives);
   - publish via `softprops/action-gh-release@v3` with `files: dist/*` (or
     `dist/*.tar.gz, dist/*.zip, dist/SHA256SUMS.txt`), `tag_name: ${{ github.ref_name }}`,
     `prerelease: auto`, empty body.
4. **NOT shipped:** the ONNX runtime `.so` (`workspace/onnxruntime/**`) and model files
   (`workspace/models/**`) and the demo corpus (`workspace/datasets/edtech/content/**`)
   must NOT appear in any archive file list. No `LICENSE` (none exists at root).

**Acceptance.**
- File exists and parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"` → no error.
- `grep -c "tags:" .github/workflows/release.yml` ≥ 1 and `grep -c "'v\*'" .github/workflows/release.yml` → 1 (tag trigger).
- `grep -c 'contents: write' .github/workflows/release.yml` → 1.
- All 5 targets present: for each of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin` → `grep -c <target> .github/workflows/release.yml` ≥ 1.
- `grep -c 'action-gh-release@v3' .github/workflows/release.yml` → 1 (and `grep -c 'action-gh-release@v2' .github/workflows/release.yml` → 0).
- `grep -c 'SHA256SUMS' .github/workflows/release.yml` → 1.
- No excluded data shipped: `grep -cE 'onnxruntime|workspace/models|datasets/edtech/content' .github/workflows/release.yml` → 0.
- No oracle/Go references: `grep -ciE 'goreleaser|golangci|go test|setup-go|\.goreleaser' .github/workflows/release.yml` → 0.
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

## 2 — Verification

- [x] 2.1 Whole-pipeline verification

**Goal.** Confirm both workflow files are valid, the split is correct (D1), the release
is complete (D2–D4), and no oracle/Go narrative leaked in.

**Scope (read-only).** `.github/workflows/ci.yml`, `.github/workflows/release.yml`.

**Dependencies.** Tasks 1.1 and 1.2.

**Acceptance.**
- Both parse: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); yaml.safe_load(open('.github/workflows/release.yml'))"` → no error.
- D1 split: `grep -c 'cross-builds\|cargo zigbuild' .github/workflows/ci.yml` → 0 (ci.yml no longer cross-builds); `grep -c 'cargo zigbuild' .github/workflows/release.yml` ≥ 1 (release.yml does).
- D2 tooling: `release.yml` uses `cargo zigbuild` + `softprops/action-gh-release@v3` (both `grep -c` ≥ 1); no `@v2` action-gh-release.
- D4 completeness: `release.yml` references `README.md`, `workspace/configs`, and `workspace/datasets/edtech/ontology` (each `grep -c` ≥ 1) and `SHA256SUMS` (≥ 1); and does NOT reference `onnxruntime`, `workspace/models`, or `datasets/edtech/content`.
- No Go *tooling* in the whole pipeline: `grep -rciE 'goreleaser|golangci|setup-go|CGO_CFLAGS|CGO_ENABLED' .github/workflows/` → 0 across both files. (The `ci.yml` header may mention the legacy Go *structure* as a design rationale — intentional per the approved plan; only Go *tooling* references are forbidden, and `\bgo\b`-style words like "cargo" must not false-positive.)
- Gates green: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- **Dev report:** list the exact archive names the 5 matrix legs produce for a sample tag (e.g. `v0.13.0`), and confirm each bundles the 4 file groups (binary, README, configs, ontology) + the single `SHA256SUMS.txt`.
