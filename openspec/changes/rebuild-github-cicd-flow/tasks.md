# Tasks: rebuild-github-cicd-flow

Order: 1.1 and 1.2 are independent (either order); 1.3 last (docs follow the final
pipeline shape). All tasks touch YAML/docs only — NO Rust source, NO Cargo.toml, NO
Cargo.lock. `cargo fmt/clippy/test` gates are not applicable (workspace untouched);
verify via `git status --porcelain` that only the declared files changed.

---

## 1.1 Rewrite `ci.yml` — merge `checks` + `coverage` into one sequential job

- **Goal:** one job, one runner, one checkout, one toolchain install, one cargo cache;
  dev CI compiles the workspace per profile (clippy metadata, test codegen, llvm-cov
  instrumented) exactly once each instead of on two parallel runners.
- **File scope:** `.github/workflows/ci.yml` ONLY (full rewrite of the file).
- **Dependencies:** none.
- **References:** `design.md` D1/D2/D4 of this change; current `.github/workflows/ci.yml`
  (steps to preserve); `rust-toolchain.toml` (channel 1.96.0).

Target file structure (keep the existing comment style — explain each non-obvious
choice; update the header comment to the new single-job philosophy):

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true
permissions:
  contents: read
jobs:
  ci:
    name: Checks + Coverage (ubuntu-latest)
    runs-on: ubuntu-latest
    steps:
      - actions/checkout@v7
      - dtolnay/rust-toolchain@1.96.0   # components: clippy, rustfmt, llvm-tools-preview
      - Swatinem/rust-cache@v2          # after toolchain (keyed by rustc version)
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
      - taiki-e/install-action@v2       # tool: cargo-llvm-cov@0.9.0
      - name: Measure coverage
        if: always()
        continue-on-error: true
        run: cargo llvm-cov --workspace --lcov --output-path lcov.info
      - name: Upload lcov artifact
        if: always()
        continue-on-error: true
        uses: actions/upload-artifact@v4
        with: { name: coverage-lcov, path: lcov.info }
```

Requirements:
- Single job `ci`; NO `checks`/`coverage` jobs, NO Gitea-fork actions
  (`ChristopherHX/gitea-upload-artifact` must NOT appear).
- Toolchain step: `dtolnay/rust-toolchain@1.96.0` with
  `components: clippy, rustfmt, llvm-tools-preview` (the `@1.96.0` pin MUST match
  `rust-toolchain.toml`).
- `cargo-llvm-cov` pinned `0.9.0` via `taiki-e/install-action@v2`; NO `--fail-under-*`
  flags anywhere (measure-first).
- Coverage step + upload: `continue-on-error: true` (design D2 — coverage must not
  block the core gate). Use `if: always()` only if needed so the upload runs after a
  skipped/failed measurement; keep it minimal.
- Preserve `concurrency` (cancel-in-progress) and `permissions: contents: read`.
- Comments: header explains the single-job layout + why coverage is non-blocking;
  per-step comments as in the current file (toolchain pin rationale, cache-after-
  toolchain rule, llvm-cov separate target dir).

**Acceptance criteria (machine-checkable):**
1. `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` exits 0.
2. `grep -c 'runs-on' .github/workflows/ci.yml` == 1 (exactly one job).
3. `grep -q 'llvm-tools-preview' .github/workflows/ci.yml` and
   `grep -q 'continue-on-error: true' .github/workflows/ci.yml` both succeed.
4. `grep -cE 'ChristopherHX|gitea' .github/workflows/ci.yml` == 0.
5. `git status --porcelain` shows ONLY `.github/workflows/ci.yml` modified
   (plus this task's revision history if any).

---

## 1.2 Rewrite `release.yml` — `gate` → `build` (5-way matrix) → `publish` + objcopy fix

- **Goal:** tag-triggered release pipeline that (a) verifies the tagged commit before
  building, (b) builds the 5 targets in parallel, (c) fixes the `zig objcopy`
  in-place bug (run 34092788497: `error: expected output parameter`), (d) publishes
  via standard GitHub actions with a categorized changelog body.
- **File scope:** `.github/workflows/release.yml` ONLY (full rewrite of the file).
- **Dependencies:** none (independent of 1.1).
- **References:** `design.md` D3/D4/D5/D6 + matrix table + action pins of this
  change; current `.github/workflows/release.yml` (archive layout, naming, comments);
  `rust-toolchain.toml`.

Target file structure:

```yaml
name: Release
on: { push: { tags: ['v*'] } }
permissions: { contents: write }
jobs:
  gate:
    name: Gate (fmt + clippy + test)
    runs-on: ubuntu-latest
    steps:
      - actions/checkout@v7
      - dtolnay/rust-toolchain@1.96.0   # components: clippy, rustfmt
      - Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test

  build:
    name: Build ${{ matrix.name }}
    needs: gate
    runs-on: ubuntu-latest
    strategy:
      matrix:
        include:
          - { target: x86_64-unknown-linux-gnu,   name: linux_amd64,  ext: tar.gz }
          - { target: aarch64-unknown-linux-gnu,  name: linux_arm64,  ext: tar.gz }
          - { target: x86_64-pc-windows-msvc,     name: windows_amd64, ext: zip }
          - { target: aarch64-apple-darwin,       name: darwin_arm64, ext: tar.gz }
          - { target: x86_64-apple-darwin,        name: darwin_amd64, ext: tar.gz }
    steps:
      - actions/checkout@v7
      - dtolnay/rust-toolchain@1.96.0   # targets: ${{ matrix.target }}
      - Swatinem/rust-cache@v2
      - run: cargo install --locked cargo-zigbuild
      - mlugg/setup-zig@v2              # version: "0.16.0"
      - name: Build + strip + package
        run: |  (see script below)
      - actions/upload-artifact@v4
        with: { name: synopsis-${{ matrix.name }}, path: dist/synopsis_*_${{ matrix.name }}.${{ matrix.ext }} }

  publish:
    name: Publish release
    needs: build
    runs-on: ubuntu-latest
    steps:
      - actions/checkout@v7             # fetch-depth: 0 (changelog needs history + tags)
      - actions/download-artifact@v4    # merge-multiple: true, path: dist
      - name: Generate checksums
        run: (cd dist && sha256sum synopsis_*.tar.gz synopsis_*.zip > SHA256SUMS.txt)
      - name: Generate changelog body
        run: (python3 script below → RELEASE_BODY.md)
      - softprops/action-gh-release@v3
        with:
          tag_name: ${{ github.ref_name }}
          files: dist/*
          body_path: RELEASE_BODY.md
          prerelease: ${{ contains(github.ref_name, '-') }}
```

Build + strip + package script (per leg; keep the `set -euo pipefail` and the
existing per-target comments):

```bash
set -euo pipefail
target="${{ matrix.target }}"; name="${{ matrix.name }}"; ext="${{ matrix.ext }}"
cargo zigbuild --release --target "${target}"
bin="target/${target}/release/synopsis"
case "${target}" in
  *-pc-windows-*) bin="${bin}.exe"; binname="synopsis.exe" ;;
  *) binname="synopsis" ;;
esac
# zig objcopy requires an explicit output file (no in-place mode) —
# write to a temp file, then rename over the original.
zig objcopy --strip-all "${bin}" "${bin}.stripped"
mv "${bin}.stripped" "${bin}"
rm -rf stage
mkdir -p stage/workspace/datasets/edtech
cp "${bin}" "stage/${binname}"
cp README.md stage/
cp -r workspace/configs stage/workspace/configs
cp -r workspace/datasets/edtech/ontology stage/workspace/datasets/edtech/ontology
archive="dist/synopsis_${GITHUB_REF_NAME#v}_${name}.${ext}"
if [ "${ext}" = "zip" ]; then
  (cd stage && zip -qr "../${archive}" .)
else
  tar czf "${archive}" -C stage .
fi
```

Changelog script (python3, inline; env `RELEASE_TAG=${{ github.ref_name }}`):
- `prev` = highest `v*` tag by version sort excluding `RELEASE_TAG`
  (`git tag --list 'v*'` → filter → `sort -V` → last; none → empty).
- If no `prev`: write empty `RELEASE_BODY.md` and exit 0.
- Else: `git log --no-merges --pretty=%s ${prev}..HEAD`; parse each subject as
  `type(scope): subject` / `type: subject`; group into sections in this order:
  `feat`→**New features**, `fix`→**Bug fixes**, `perf`→**Performance**,
  `refactor`→**Refactoring**, `docs`→**Documentation**, `test`→**Tests**,
  `build`→**Build**, `ci`→**CI**, `chore`→**Chores**, unmatched→**Other**;
  entries as `- <original subject line>`; write `RELEASE_BODY.md` (sections with no
  entries omitted; trailing newline).

Requirements:
- NO Gitea-fork actions (`ChristopherHX`, `akkuman`, `gitea-release-action` must NOT
  appear); NO `NODE_OPTIONS: '--experimental-fetch'` env.
- `softprops/action-gh-release@v3` (NOT v2 — deprecated Node 20 line); `prerelease`
  is the computed `contains(github.ref_name, '-')` (NOT `auto`).
- `dtolnay/rust-toolchain@1.96.0` everywhere (must match `rust-toolchain.toml`);
  build legs install `targets: ${{ matrix.target }}` (one target per leg).
- `zig objcopy` MUST use the two-file form + `mv` (design D5).
- Archive contents/naming unchanged: stripped binary + `README.md` +
  `workspace/configs/**` + `workspace/datasets/edtech/ontology/**`;
  `synopsis_<version>_<name>.<ext>`; `SHA256SUMS.txt` at dist root.
- Header comment: explain the three-job shape (gate → parallel matrix → publish),
  why the gate exists, and that the Gitea single-job design is superseded
  (change `rebuild-github-cicd-flow`).

**Acceptance criteria (machine-checkable):**
1. `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"` exits 0.
2. `grep -c 'zig objcopy --strip-all' .github/workflows/release.yml` == 1 AND
   `grep -q 'bin.stripped' .github/workflows/release.yml`.
3. `grep -q 'softprops/action-gh-release@v3' .github/workflows/release.yml` AND
   `grep -q 'actions/download-artifact@v4' .github/workflows/release.yml`.
4. `grep -cE 'ChristopherHX|akkuman|gitea|NODE_OPTIONS' .github/workflows/release.yml` == 0.
5. `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); assert set(d['jobs']) == {'gate','build','publish'}; assert d['jobs']['build']['needs']=='gate'; assert d['jobs']['publish']['needs']=='build'"` exits 0.
6. `git status --porcelain` shows ONLY `.github/workflows/release.yml` modified
   (plus this task's revision history if any).

---

## 1.3 Update `AGENTS.md` — CI gotcha + cross-build target list (3 spots)

- **Goal:** three places in `AGENTS.md` describe the OLD pipeline / OLD target
  matrix and would instruct future agents against what this change shipped —
  replace them.
- **File scope:** `AGENTS.md` ONLY, exactly these three spots:
  1. The Commands-table row (line ~59): "Cross-build targets (the CI cross-build
     matrix): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`,
     `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`."
  2. The gotcha bullet "**CI is Gitea-compatible on purpose:** …" (line ~71).
  3. The gotcha bullet "**Cross-builds:** windows-gnu instead of msvc (Zig cannot
     link MSVC ABI from a Linux host). The x86_64 musl artifact is fully static."
     (line ~73).
  Do NOT touch any other line, bullet, or section.
- **Dependencies:** 1.1 and 1.2 (the new text must describe the shipped shape).
- **References:** `design.md` D4/D7 of this change; the new
  `.github/workflows/{ci.yml,release.yml}`.

Replacements (adapt wording, keep the bold-label style of the other bullets):

1. Commands-table row →
   `Cross-build targets (the CI cross-build matrix): `x86_64-unknown-linux-gnu`,
   `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`,
   `x86_64-apple-darwin`.`
2. Gitea gotcha bullet →
   > **CI is GitHub-native (change `rebuild-github-cicd-flow`, user decision
   > 2026-09-07):** `ci.yml` is ONE job — fmt + clippy + test + `cargo llvm-cov`
   > (coverage is `continue-on-error`, measure-first, no gates). `release.yml` on
   > `v*` tags: `gate` (fmt+clippy+test on the tagged commit) → `build` (5-way
   > parallel matrix, one zigbuild target per leg, `zig objcopy` strip with an
   > explicit output file — `zig objcopy` has no in-place mode) → `publish`
   > (standard `actions/*` + `softprops/action-gh-release@v3`, categorized
   > changelog from conventional commits, `SHA256SUMS.txt`). The earlier Gitea
   > compatibility (single job, Gitea-fork actions) is superseded — the Gitea
   > remote (`origin`) is a plain git mirror without CI. Cross-builds run only on
   > `v*` tags.
3. Cross-builds gotcha bullet →
   > **Cross-builds:** windows-msvc (the standard Rust Windows target) is
   > cross-built from the Linux host via cargo-zigbuild — zig's linker supports
   > the MSVC ABI (the old "cannot link MSVC ABI" note is stale). No musl
   > targets: musl is slow to compile and its fully-static benefit is irrelevant
   > for end-user machines; glibc is present on every supported distro.

**Acceptance criteria (machine-checkable):**
1. `grep -c 'Gitea-compatible on purpose' AGENTS.md` == 0.
2. `grep -c -iE 'musl|windows-gnu' AGENTS.md` == 0.
3. `grep -q 'rebuild-github-cicd-flow' AGENTS.md` AND
   `grep -q 'softprops/action-gh-release@v3' AGENTS.md`.
4. `grep -q 'x86_64-pc-windows-msvc' AGENTS.md` AND
   `grep -q 'x86_64-apple-darwin' AGENTS.md`.
5. `git diff --stat AGENTS.md` shows a bounded change (only the three declared
   regions).
