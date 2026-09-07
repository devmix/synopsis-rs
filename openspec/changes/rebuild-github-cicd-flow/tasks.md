# Tasks: rebuild-github-cicd-flow

Order: 1.1 and 1.2 are independent (either order); 1.3 after them (docs follow the
final pipeline shape). REVISION 2026-09-07 (first release run 34100195267 failed):
1.4 fixes the build legs (drop the redundant/ELF-only strip step, revert
windows-msvc → windows-gnu, add the usearch `Windows.h` case shim) and stabilizes
the rust-cache keys (per-leg + shared host) — MUST land before the tag re-push.
1.5 (darwin legs need the macOS SDK — `SDKROOT`) also MUST land before the tag
re-push. 1.6 (project docs) and 1.7 (site docs) follow AFTER a green release run.
All tasks touch YAML/docs/comments only — NO Rust source, NO Cargo.toml deps, NO
Cargo.lock (1.6 touches comment lines in Cargo.toml files only). `cargo
fmt/clippy/test` gates are not applicable (workspace behavior untouched); verify
via `git status --porcelain` that only the declared files changed.

---

## 1.1 Rewrite `ci.yml` — merge `checks` + `coverage` into one sequential job

> **DONE 2026-09-07 (commit 5c21cd0).**

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

> **DONE 2026-09-07 (commit 5c21cd0).**

> **SUPERSEDED in part by 1.4 (revision 2026-09-07):** the `zig objcopy` strip step
> is REMOVED (D5 — the profile already strips; `zig objcopy` is ELF-only), and the
> Windows matrix row reverts `x86_64-pc-windows-msvc` → `x86_64-pc-windows-gnu` (D7 —
> Zig has no MSVC libc for C compilation). This task's body is kept as the
> as-committed history; 1.4 is authoritative for the build leg.

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

> **SUPERSEDED in part by 1.6 (revision 2026-09-07):** the msvc wording in the
> replacements above is reverted to windows-gnu (D7), and the "zig objcopy strip
> with an explicit output file" clause is replaced (D5 — no strip step). AC #2 and
> #4 above were correct for the as-committed state; 1.6 re-fixes these spots and is
> authoritative.

---

## 1.4 Fix `release.yml` build legs + stabilize rust-cache keys

- **Goal:** make the release pipeline actually build all 5 targets and stop
  rebuilding from scratch on every run. Three fixes, all evidence-backed
  (design D5/D7/D8 of this change; first run 34100195267):
  1. **Drop the strip step** — `[profile.release] strip = true` (root `Cargo.toml`)
     already strips at link time; `zig objcopy` is ELF-only (`InvalidElfMagic` on
     the built PE) and would have broken 4 of 5 legs.
  2. **Revert the Windows leg** `x86_64-pc-windows-msvc` → `x86_64-pc-windows-gnu`
     (Zig 0.16 has no libc/headers for the MSVC target: `ring` C code failed with
     `'assert.h' file not found`) AND **add the usearch `Windows.h` case shim**
     (`index.hpp:78` includes capital-W `Windows.h`; case-sensitive Linux hosts
     only have `windows.h`).
  3. **Stabilize rust-cache keys** — the action's default key embeds the job NAME
     (`v0-rust-{job}-{OS}-{envHash}`), so the job rename invalidated all caches
     (whole run cold: clippy 313s + test 298s + llvm-cov 551s) and all 5 matrix
     legs shared ONE key (last saver wins → 4 of 5 targets cold on every tag).
- **File scope:**
  - `.github/workflows/release.yml` (matrix row, build script, rust-cache steps,
    comments),
  - `.github/workflows/ci.yml` (the ONE rust-cache step),
  - NEW `third_party/windows-case-shim/Windows.h` (one-line shim header).
  Nothing else.
- **Dependencies:** 1.1, 1.2 (edits the committed files).
- **References:** `design.md` D3/D5/D7/D8 of this change; the committed
  `.github/workflows/{release.yml,ci.yml}`; root `Cargo.toml` `[profile.release]`.

Changes:

1. `release.yml` matrix row: `- { target: x86_64-pc-windows-gnu, name: windows_amd64, ext: zip }`
   (replace the msvc row; all other rows unchanged).
2. `release.yml` "Build + strip + package" step → rename to "Build + package" and
   REMOVE the two `zig objcopy`/`mv` lines (and their comment). New script body
   (keep `set -euo pipefail`, the `case` for `.exe`, staging, and archive logic
   exactly as committed):

   ```bash
   set -euo pipefail
   target="${{ matrix.target }}"; name="${{ matrix.name }}"; ext="${{ matrix.ext }}"
   # usearch includes <Windows.h> (capital W); case-sensitive Linux hosts only
   # have windows.h — put the one-line shim on the C++ include path (design D7).
   if [ "${target}" = "x86_64-pc-windows-gnu" ]; then
     export CXXFLAGS_x86_64_pc_windows_gnu="-I${GITHUB_WORKSPACE}/third_party/windows-case-shim"
   fi
   cargo zigbuild --release --target "${target}"
   bin="target/${target}/release/synopsis"
   case "${target}" in
     *-pc-windows-*) bin="${bin}.exe"; binname="synopsis.exe" ;;
     *) binname="synopsis" ;;
   esac
   # No strip step: [profile.release] strip = true already strips debug info and
   # symbols at link time (design D5). zig objcopy is ELF-only and would fail on
   # the PE/Mach-O legs.
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

3. `release.yml` rust-cache steps:
   - `gate` job: add `with: { shared-key: host }` to the `Swatinem/rust-cache@v2` step.
   - `build` job: add `with: { key: ${{ matrix.name }} }` to the `Swatinem/rust-cache@v2` step.
4. `ci.yml` rust-cache step: add `with: { shared-key: host }` to the
   `Swatinem/rust-cache@v2` step.
5. NEW file `third_party/windows-case-shim/Windows.h` — EXACTLY:

   ```c
   /* Case-sensitivity shim for cross-compiling from Linux.
    *
    * usearch (include/usearch/index.hpp) does `#include <Windows.h>` with a
    * capital W. Windows filesystems are case-insensitive, so it works there;
    * on a case-sensitive Linux host, Zig's mingw headers only provide
    * lowercase `windows.h` and the include fails. This header is placed on the
    * C++ include path for the windows-gnu CI leg (see .github/workflows/
    * release.yml) and forwards to the real header.
    */
   #include <windows.h>
   ```

6. Comments: update the header comment and per-step comments in `release.yml`
   that reference the strip step or the msvc target so they match the shipped
   behavior (no strip — profile strips; windows-gnu; case shim; per-leg cache
   keys). Keep the existing comment style. Do NOT change step order, action
   pins, the changelog script, or the publish job.

**Acceptance criteria (machine-checkable):**
1. `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"` exits 0.
2. `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` exits 0.
3. `grep -c 'zig objcopy' .github/workflows/release.yml` == 0 (strip step gone,
   including comments).
4. `grep -q 'x86_64-pc-windows-gnu' .github/workflows/release.yml` AND
   `grep -c 'windows-msvc' .github/workflows/release.yml` == 0.
5. `grep -q 'CXXFLAGS_x86_64_pc_windows_gnu' .github/workflows/release.yml` AND
   `grep -q 'third_party/windows-case-shim' .github/workflows/release.yml`.
6. `test -f third_party/windows-case-shim/Windows.h` AND
   `grep -q '#include <windows.h>' third_party/windows-case-shim/Windows.h`.
7. `grep -q 'shared-key: host' .github/workflows/ci.yml` AND
   `grep -c 'shared-key: host' .github/workflows/release.yml` == 1 (gate only) AND
   `grep -q 'key: ${{ matrix.name }}' .github/workflows/release.yml`.
8. `grep -q 'softprops/action-gh-release@v3' .github/workflows/release.yml` AND
   `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); assert set(d['jobs']) == {'gate','build','publish'}"` exits 0
   (structure unchanged).
9. `git status --porcelain` shows ONLY the three declared files
   (`.github/workflows/release.yml`, `.github/workflows/ci.yml`,
   `third_party/windows-case-shim/Windows.h`) plus this change's tasks.md revision
   note if any.

---

## 1.5 Fix darwin legs — macOS SDK (`SDKROOT`)

- **Goal:** the two darwin build legs fail at the final link on a Linux host:
  rustc's linker driver locates the macOS SDK via `xcrun --sdk macosx --show-sdk-path`,
  which does not exist on Linux → `error: linking with zigcc-<target> wrapper failed`
  (reproduced locally: every dependency compiled, then the `cli` binary link failed
  for BOTH darwin legs). Fix: give the darwin legs Apple's `MacOSX11.3.sdk` and
  export `SDKROOT` — the exact mechanism + SDK the official cargo-zigbuild
  Dockerfile uses (design D9 of this change).
- **File scope:** `.github/workflows/release.yml` ONLY (the `build` job).
  Nothing else.
- **Dependencies:** 1.4 (edits the committed build job).
- **References:** `design.md` D9 of this change; the committed
  `.github/workflows/release.yml`; cargo-zigbuild README (Environment Variables →
  `SDKROOT`) and its official Dockerfile (`MacOSX11.3.sdk` + `ENV SDKROOT`).

Changes (all in the `build` job of `release.yml`):

1. Add TWO steps AFTER the `mlugg/setup-zig@v2` step and BEFORE the
   "Build + package" step, both guarded by
   `if: contains(matrix.target, 'apple-darwin')`:

   a. **Cache macOS SDK** — `actions/cache@v4` with
      `path: ${{ env.HOME }}/macosx-sdk/MacOSX11.3.sdk` and
      `key: macosx-sdk-11.3`. (Restores the 575 MB extracted SDK on a cache hit;
      saves it at the end of a successful leg.)
   b. **Download macOS SDK** — `run: |` with `set -euo pipefail`, creating
      `$HOME/macosx-sdk` and downloading + extracting ONLY if the SDK is not
      already present (cache miss):
      `curl -L --fail
      "https://github.com/phracker/MacOSX-SDKs/releases/download/11.3/MacOSX11.3.sdk.tar.xz"
      | tar -J -x -C "$HOME/macosx-sdk"`.

2. In the "Build + package" `run:` script, NEXT TO the existing windows-gnu
   `CXXFLAGS` guard, add a darwin `SDKROOT` guard so the value is set only for the
   darwin legs:
   ```bash
   case "${target}" in
     *-apple-darwin) export SDKROOT="${HOME}/macosx-sdk/MacOSX11.3.sdk" ;;
   esac
   ```
   (Place it after the `target/name/ext` assignment and the windows-gnu guard,
   before `cargo zigbuild`.)

3. Comments: update the header / per-step comments in the `build` job so they
   match the shipped behavior (darwin legs need the macOS SDK + `SDKROOT`, design
   D9). Keep the existing comment style. Do NOT change step order, action pins,
   the matrix, the changelog script, or the publish job.

**Acceptance criteria (machine-checkable):**
1. `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"` exits 0.
2. `grep -q 'MacOSX11.3.sdk' .github/workflows/release.yml` AND
   `grep -q 'phracker/MacOSX-SDKs' .github/workflows/release.yml`.
3. `grep -q 'SDKROOT' .github/workflows/release.yml` AND
   `grep -q 'macosx-sdk-11.3' .github/workflows/release.yml` (the cache key).
4. `grep -q 'actions/cache@v4' .github/workflows/release.yml`.
5. Both new steps are guarded: `grep -c "contains(matrix.target, 'apple-darwin')"
   .github/workflows/release.yml` == 2 (the two `if:` guards).
6. `python3 -c "import yaml; d=yaml.safe_load(open('.github/workflows/release.yml')); assert set(d['jobs']) == {'gate','build','publish'}"` exits 0
   (job structure unchanged) AND
   `grep -q 'softprops/action-gh-release@v3' .github/workflows/release.yml`.
7. `grep -c 'zig objcopy' .github/workflows/release.yml` == 0 (1.4's fix intact)
   AND `grep -q 'x86_64-pc-windows-gnu' .github/workflows/release.yml`.
8. `git status --porcelain` shows ONLY `.github/workflows/release.yml` (plus this
   change's tasks.md/design.md/proposal.md revision notes if any).

---

## 1.6 Fix project docs — AGENTS.md, README.md, Cargo.toml comments

- **Goal:** bring the repo-root project docs in line with the shipped pipeline
  (design D5/D7/D8 of this change). Three classes of stale text: (a) the msvc
  wording task 1.3 committed is reverted to windows-gnu; (b) the "zig objcopy
  strip" clause is replaced (no strip step — the profile strips); (c) musl
  references and old target lists in README.md and three Cargo.toml comments.
- **File scope:** `AGENTS.md`, `README.md`, `Cargo.toml` (root, line ~235),
  `crates/llm/Cargo.toml` (line ~16), `crates/graph/Cargo.toml` (line ~13).
  Comment lines only in the Cargo.toml files — NO dependency or feature changes.
- **Dependencies:** 1.4 (the pipeline it documents must be committed first).
- **References:** `design.md` D5/D7/D8 of this change; the committed
  `.github/workflows/{release.yml,ci.yml}`.

Changes:

1. `AGENTS.md` (3 spots, as committed by task 1.3):
   - **Commands table**, `cargo zigbuild` row: target list
     `x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, x86_64-pc-windows-gnu,
     aarch64-apple-darwin, x86_64-apple-darwin` (msvc → gnu).
   - **CI GitHub-native gotcha bullet**: replace the clause
     "`zig objcopy` strip with an explicit output file — `zig objcopy` has no
     in-place mode" with "no strip step — `[profile.release] strip = true`
     strips at link time". Keep the rest of the bullet (gate → build → publish,
     softprops, changelog, SHA256SUMS.txt).
   - **Cross-builds gotcha bullet**: replace the msvc sentence with the new
     rationale — Zig 0.16 has no libc/headers for the MSVC target (C code in
     ring/libsqlite3-sys/usearch cannot compile for msvc via zig cc), so Windows
     stays `x86_64-pc-windows-gnu`; one-line note that the Windows leg carries a
     case-sensitivity shim for usearch's `#include <Windows.h>`
     (`third_party/windows-case-shim/`); keep the "fully static targets dropped"
     sentence (still true for musl).
2. `README.md` (2 spots):
   - Line ~35: "published as Gitea Releases on `v*` tags" → "published as
     GitHub Releases on `v*` tags".
   - Lines ~104–105: target list →
     `x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, x86_64-pc-windows-gnu,
     aarch64-apple-darwin, x86_64-apple-darwin`.
3. `Cargo.toml` (root, line ~235) + `crates/llm/Cargo.toml` (line ~16): comment
   "the static musl and windows-gnu CI targets" → "the gnu cross-build CI targets"
   (musl is gone; rustls+gzip rationale unchanged).
4. `crates/graph/Cargo.toml` (line ~13): comment "zigbuild windows-gnu +
   aarch64-apple-darwin" — still accurate; add `x86_64-apple-darwin` to the list
   if the comment enumerates targets, otherwise leave unchanged.

**Acceptance criteria (machine-checkable):**
1. `grep -c -i 'msvc' AGENTS.md README.md` reports 0 for both files.
2. `grep -c 'zig objcopy' AGENTS.md` == 0.
3. `grep -c -i 'musl' AGENTS.md README.md Cargo.toml crates/llm/Cargo.toml crates/graph/Cargo.toml` reports 0 for all files.
4. `grep -c -i 'gitea' AGENTS.md README.md` reports 0 for both (the Gitea remote
   mirror sentence in AGENTS.md stays — it says "the Gitea remote (`origin`) is a
   plain git mirror without CI", which is still true; AC checks only that no
   Gitea-RELEASE wording remains: `grep -c 'Gitea Release' AGENTS.md README.md`
   == 0 for both).
5. `grep -q 'x86_64-apple-darwin' README.md` AND
   `grep -q 'windows-case-shim' AGENTS.md`.
6. `git status --porcelain` shows ONLY the five declared files.
7. `cargo metadata --format-version 1 > /dev/null` exits 0 (Cargo.toml edits
   broke nothing).

---

## 1.7 Fix site docs — GitHub Releases, new matrix, pipeline shape

- **Goal:** the docs site (`site/docs/`) still describes the OLD Gitea pipeline
  (single job, Gitea-fork actions, musl targets, `zig objcopy` strip) and links to
  a `gitea-releases` page. Rewrite the affected pages for the shipped
  GitHub-native pipeline (design D1/D3/D5/D7/D8 of this change).
- **File scope (all under `site/`):**
  - `site/docs/developer/gitea-releases.mdx` → `git mv` to
    `site/docs/developer/github-releases.mdx` (auto sidebar picks up the new name;
    `sidebar_position: 8` stays) and REWRITE the content.
  - `site/docs/developer/ci-cd.mdx` — REWRITE (whole page is stale: Gitea
    runner, two jobs `checks`/`coverage`, single-job release, musl table, objcopy).
  - `site/docs/developer/setup.mdx` — target table (lines ~60–66).
  - `site/docs/quickstart.mdx` — target table + "Gitea Releases" wording (lines ~15–25).
  - `site/docs/guides/installation.mdx` — description, both target tables,
    zigbuild example, and the link to the releases page (lines ~3, ~20–27, ~72–78).
  - `site/docs/roadmap.mdx` — the "Self-contained build + Gitea CI" bullet and
    its links (line ~23).
  - Update every internal link `developer/gitea-releases` →
    `developer/github-releases` (found in `installation.mdx`, `roadmap.mdx`).
  Nothing else (no `docusaurus.config.*`, no `sidebars.ts` — the sidebar is
  autogenerated from the folder structure).
- **Dependencies:** 1.4, 1.5, 1.6 (docs describe the committed pipeline).
- **References:** `design.md` D1/D3/D5/D7/D8 of this change; the committed
  `.github/workflows/{release.yml,ci.yml}` (the source of truth for what the pages
  describe).

Final state the pages must describe:

1. **`ci.yml`** — ONE job `ci` on `ubuntu-latest`, push/PR to `main`,
   `concurrency: cancel-in-progress`: toolchain 1.96.0 (`clippy, rustfmt,
   llvm-tools-preview`) → rust-cache (`shared-key: host`) → `cargo fmt --check` →
   clippy `-D warnings` → `cargo test` → cargo-llvm-cov 0.9.0
   (`--workspace --lcov --output-path lcov.info`, `continue-on-error`, upload lcov
   via `actions/upload-artifact@v4`). Measure-first, no coverage gates.
2. **`release.yml`** — on `v*` tags, three jobs: `gate` (fmt + clippy + test on
   the tagged commit, rust-cache `shared-key: host`) → `build` (5-way parallel
   matrix, one `cargo zigbuild` target per leg, rust-cache `key: <leg>`, Zig
   0.16.0, NO strip step — the profile strips, package
   `synopsis_<version>_<name>.<ext>`) → `publish` (`actions/download-artifact@v4`
   + `softprops/action-gh-release@v3`, `SHA256SUMS.txt`, categorized changelog
   from conventional commits). If a leg fails, nothing is published — re-push the
   tag to retry.
3. **The five targets** (same table shape as today):

   | Target triple | Platform | Archive (v0.1.0 example) |
   |---|---|---|
   | `x86_64-unknown-linux-gnu` | Linux amd64 | `synopsis_0.1.0_linux_amd64.tar.gz` |
   | `aarch64-unknown-linux-gnu` | Linux arm64 | `synopsis_0.1.0_linux_arm64.tar.gz` |
   | `x86_64-pc-windows-gnu` | Windows amd64 | `synopsis_0.1.0_windows_amd64.zip` |
   | `aarch64-apple-darwin` | macOS arm64 | `synopsis_0.1.0_darwin_arm64.tar.gz` |
   | `x86_64-apple-darwin` | macOS amd64 | `synopsis_0.1.0_darwin_amd64.tar.gz` |

   Note: **windows-gnu, not msvc** — Zig 0.16 has no libc/headers for the MSVC
   target, so C code (ring, libsqlite3-sys, usearch) cannot compile for msvc from
   a Linux host; the Windows leg carries a one-line case-sensitivity shim for
   usearch's `#include <Windows.h>`.
4. **What ships in an archive** (unchanged): stripped binary (profile-stripped),
   `README.md`, `workspace/configs/**`, `workspace/datasets/edtech/ontology/**`;
   ONNX runtime + model weights excluded (downloaded per `onnx.yaml`).
5. **Wording:** "Gitea Releases" → "GitHub Releases" everywhere; drop all
   "self-hosted Gitea runner / no artifact service / Gitea-fork actions / do not
   upgrade to GitHub actions" rationale (superseded — the GitHub remote is the
   CI home; the Gitea remote is a plain mirror).
6. **`github-releases.mdx`** (renamed page): describe the gate → build matrix →
   publish flow, the 5 targets, archive contents, the re-push-to-retry rule, and
   the cache-key behavior (per-leg keys; first tag after a key change is cold).

**Acceptance criteria (machine-checkable):**
1. `test ! -f site/docs/developer/gitea-releases.mdx` AND
   `test -f site/docs/developer/github-releases.mdx`.
2. `grep -rc -i 'gitea' site/docs --include='*.mdx' | grep -v ':0'` exits non-zero
   (no file contains "gitea" case-insensitively).
3. `grep -rc -i 'musl' site/docs --include='*.mdx' | grep -v ':0'` exits non-zero.
4. `grep -rc 'zig objcopy' site/docs --include='*.mdx' | grep -v ':0'` exits
   non-zero.
5. `grep -rc 'windows-msvc' site/docs --include='*.mdx' | grep -v ':0'` exits
   non-zero.
6. `grep -q 'x86_64-apple-darwin' site/docs/developer/github-releases.mdx` AND
   `grep -q 'softprops/action-gh-release@v3' site/docs/developer/github-releases.mdx`.
7. `grep -q 'shared-key' site/docs/developer/ci-cd.mdx` AND
   `grep -q 'matrix' site/docs/developer/ci-cd.mdx`.
8. No broken internal links: `grep -rc 'developer/gitea-releases' site/docs
   --include='*.mdx' | grep -v ':0'` exits non-zero.
9. `git status --porcelain` shows ONLY files under `site/docs/` (including the
   rename as `R` status).
