# Design: rebuild-github-cicd-flow

## Context

History: `rebuild-ci-cd` (2026-09-04) built the GitHub-native pipeline — `ci.yml`
(checks + coverage in parallel) and `release.yml` (5-way matrix `build` + `release`
job, `softprops/action-gh-release@v3`). `make-ci-gitea-compatible` (2026-09-04) then
re-targeted the same files at Gitea (no artifact service): `release.yml` collapsed to
a single sequential job, Gitea-fork actions swapped in. Since then the project has
moved to GitHub: CI runs on GitHub Actions (`github` remote =
`https://github.com/devmix/synopsis-rs.git`), and there is no `.gitea/` directory —
the Gitea runner executes nothing. The first tag (`v0.1.0`) exposed the latent
`zig objcopy` bug in the single-job loop. The first run of THIS pipeline
(34100195267) failed the `windows-msvc` leg (Zig has no MSVC libc for C
compilation — D7) and showed the rust-cache keys are job-name-based (D8);
both are fixed by revision 2026-09-07 (tasks 1.4–1.5).

Frozen stack + constraints: `openspec/config.yaml`. No recorded fixtures / contract
specs are the reference (no behavior change).

## Decisions

### D1 — `ci.yml`: one sequential job (checks + coverage merged)

- **Decision:** a single job `ci` (display name `Checks + Coverage (ubuntu-latest)`):
  checkout → toolchain 1.96.0 (components `clippy, rustfmt, llvm-tools-preview`) →
  `Swatinem/rust-cache@v2` (`shared-key: host`, D8) → `cargo fmt --check` →
  `cargo clippy --all-targets -- -D warnings` → `cargo test` → install
  `cargo-llvm-cov@0.9.0`
  (`taiki-e/install-action@v2`) → `cargo llvm-cov --workspace --lcov --output-path
  lcov.info` → `actions/upload-artifact@v4` (`name: coverage-lcov`, `path: lcov.info`).
  `concurrency` (cancel-in-progress) and `permissions: contents: read` are preserved.
- **Why:** the two parallel jobs each did checkout + toolchain install + cache restore
  and each compiled the workspace — 2× runner-minutes per push for a personal project
  on the free tier. Merging halves runner usage; the shared `./target` cache is
  restored once. Wall time grows (serial instead of parallel) — accepted by the user.
  `cargo-llvm-cov` builds in its own `target/llvm-cov-target` directory, so it neither
  pollutes nor reuses `./target` — no cache interference with the test step.
- **Alternative rejected:** `coverage` as a second job with `needs: [checks]` — still
  two runners, two caches, two full compiles; no benefit over one job.
- **Alternative rejected:** dropping coverage from CI — rejected, measure-first is a
  standing project decision (`COVERAGE.md`); the user asked to optimize, not remove.

### D2 — Coverage stays non-blocking: `continue-on-error: true`

- **Decision:** the `Measure coverage` step (and its artifact upload) carry
  `continue-on-error: true`.
- **Why:** the original parallel layout made a coverage failure unable to block the
  core gate ("measure-first, no gates" — change `coverage-rust-workspace`). Merging
  into one job would silently change that semantic; the flag restores it explicitly.
  No `--fail-under-*` flags are added (gating remains deferred).
- **Alternative rejected:** letting a coverage failure fail the run — rejected, it
  would turn a measurement into a gate without a decision to do so.

### D3 — `release.yml`: `gate` → `build` (matrix) → `publish`

- **Decision:** three jobs:
  1. `gate` — checkout → toolchain 1.96.0 (`clippy, rustfmt`) → rust-cache
     (`shared-key: host`, D8) → fmt → clippy → test. No coverage (keeps the gate
     fast; coverage lives in dev CI).
  2. `build` (`needs: gate`) — `strategy.matrix.include` of the 5 rows (table in D7;
     each row carries `target`, `name`, `ext`). Each leg: checkout → toolchain 1.96.0
     (`targets: ${{ matrix.target }}`) → rust-cache (`key: ${{ matrix.name }}`, D8) →
     `cargo install --locked cargo-zigbuild` → `mlugg/setup-zig@v2` (0.16.0) →
     (darwin legs: macOS SDK download + `SDKROOT`, D9) → `cargo zigbuild --release
     --target` (Windows leg: `CXXFLAGS` case shim, D7) → package
     `synopsis_<version>_<name>.<ext>` (no strip — profile strips, D5) →
     `actions/upload-artifact@v4` (`name: synopsis-<name>`, `path: dist/...`).
  3. `publish` (`needs: build`) — checkout (`fetch-depth: 0`, the changelog needs
     history + tags) → `actions/download-artifact@v4` (`merge-multiple: true`,
     `path: dist`) → `sha256sum` → `dist/SHA256SUMS.txt` → changelog script (D6) →
     `softprops/action-gh-release@v3` (`tag_name: ${{ github.ref_name }}`,
     `files: dist/*`, `body_path: RELEASE_BODY.md`, `prerelease: ${{ contains(github.
     ref_name, '-') }}`).
- **Why `gate`:** a tag push does not re-run the branch CI, and a tag can point at any
  commit (one with failing CI, or one never pushed to main). The gate guarantees a
  release is only built from verified code. Cost: ~3–4 min of re-verification per
  tag — accepted.
- **Why matrix:** GitHub has an artifact service, so the five archives can be passed
  from the build legs to `publish` — the constraint that forced `make-ci-gitea-
  compatible` D1 (single job) no longer exists. Wall time drops from a sequential
  ~25–30 min loop to `gate + max(target) + publish` ≈ 8–10 min.
- **Alternative rejected:** `workflow_run` trigger (start the release only after a
  green CI run of the same SHA) — rejected: no inputs, depends on push ordering
  (CI must finish before the tag is pushed), harder to reason about and to re-run.
- **Alternative rejected:** keeping the single sequential job — rejected: the user
  asked for the optimization; the Gitea constraint is gone.

### D4 — Standard GitHub actions restore; Gitea forks out

- **Decision:** `actions/upload-artifact@v4`, `actions/download-artifact@v4`,
  `softprops/action-gh-release@v3`. The `NODE_OPTIONS: '--experimental-fetch'` env
  (a gitea-release-action Node < 18 workaround) is dropped.
- **Why:** CI runs only on GitHub; the forks existed solely for the Gitea runner.
  `softprops` v2.6.2 is deprecated (Node 20) — v3 (Node 24) is the current line, as
  already decided in `rebuild-ci-cd` D2. The `prerelease: auto` softprops feature is
  NOT used; the same computed boolean as before (`contains(github.ref_name, '-')`)
  keeps the prerelease rule byte-identical to the shipped behavior.
- **User decision:** 2026-09-07 — "Текущий CI/CD можно полностью поменять" (the
  current CI/CD may be fully changed), explicitly lifting the "Gitea-compatible on
  purpose / do not upgrade" gotcha. The Gitea remote stays a plain git mirror.

### D5 — NO strip step: `[profile.release] strip = true` already strips

- **Decision:** the `build` legs do NOT strip the binary — the old
  `zig objcopy --strip-all` step is removed entirely. The binary is packaged
  as produced by `cargo zigbuild --release`.
- **Why (two facts, both verified locally on this host, Zig 0.16.0 / rustc
  1.96.0):**
  1. **The profile already strips.** Root `Cargo.toml` sets
     `[profile.release] strip = true`, so rustc strips debug info + symbols at
     link time via LLVM. The built `x86_64-pc-windows-gnu` PE has **0 symbols**
     (checked in the COFF header: `NumberOfSymbols == 0`), with or without any
     external strip step. The step was redundant.
  2. **`zig objcopy` is ELF-ONLY.** Run on the built PE it fails with
     `error: invalid elf file: InvalidElfMagic`. The old comment ("handles ELF,
     PE/COFF, Mach-O with one command") was WRONG — the step could never have
     worked for the Windows (PE) or macOS (Mach-O) legs. So even the "fixed"
     two-file form (revision 1) was a latent bug for 4 of 5 targets.
- **Evidence:** local `zig objcopy --strip-all synopsis.exe out.exe` →
  `InvalidElfMagic` on the PE; local `zig cc --target=x86_64-windows-gnu` on a
  `#include <windows.h>` file → OK, on `#include <Windows.h>` → `file not found`
  (case-sensitivity, see D7). PE symbol count 0 with no strip step.
- **Alternative rejected:** `llvm-strip` (handles all three formats) — rejected:
  not needed; the profile strips at link time for every target with no external
  tool, and adding a per-format strip toolchain is complexity for zero benefit.
- **Mach-O note (verified 2026-09-07):** the darwin binaries retain 410 (x86_64)
  / 694 (arm64) symbols — this is the **irreducible minimum for a dyld-linked
  executable**, not a missing strip step. rustc 1.96.0 runs
  `rust-objcopy --strip-all` post-link (`rustc_codegen_ssa/src/back/link.rs`);
  LLVM's Mach-O objcopy backend keeps exactly the symbols the dynamic linker
  requires (undefined imports for two-level namespace binding,
  `REFERENCED_DYNAMICLY` symbols, and indirect-symbol-table entries for global
  data via `__la_symbol_ptr`). Analysis of both darwin binaries: every retained
  symbol is in one of those three categories; the only technically-removable
  symbol is `__mh_execute_header`, which Apple's own toolchain also keeps.
  ELF shows 0 symbols because glibc binding uses `.dynsym`, so the whole
  `.symtab` can be dropped. Size impact ≈ 15 KB / 17.4 MB (0.09%). A
  post-link `llvm-strip`/`zig objcopy` step for the darwin legs would change
  nothing.

### D6 — Release body: categorized changelog from conventional commits

- **Decision:** an inline `python3` script in the `publish` job writes
  `RELEASE_BODY.md`: commits in `<prev-tag>..HEAD` grouped by conventional-commit
  type into sections — `feat` → **New features**, `fix` → **Bug fixes**, `perf` →
  **Performance**, `refactor` → **Refactoring**, `docs` → **Documentation**, `test`
  → **Tests**, `build` → **Build**, `ci` → **CI**, `chore` → **Chores**, anything
  else → **Other**; each entry is `- <subject>` (scope preserved). `<prev-tag>` =
  the highest `v*` tag (by `sort -V`) excluding the current tag; if none exists
  (first release), the body is empty (assets only; the first release body is set
  manually once).
- **Why:** the user expects CI to own the release including the categorized changelog
  (the manual v0.1.0 body was created by hand precisely because CI had none). The
  repo already commits strictly by Conventional Commits, so a small deterministic
  script needs no new tool.
- **Alternative rejected:** release-plz / release-please — rejected: they manage
  versioning + tagging + commits, which would replace the manual-tag flow (a bigger
  contract change than approved); `softprops` has no built-in changelog.
- **Alternative rejected:** `generate_release_notes: true` (GitHub auto-notes) —
  rejected: GitHub's grouping is PR/commit-based, not conventional-commit-type-based,
  and the output is not reproducible.

### D7 — 2026 target matrix: drop musl, KEEP windows-gnu, add Intel macOS

- **Decision:** the 5-target matrix is:

| zigbuild target | `<name>` | `<ext>` |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `linux_amd64` | `tar.gz` |
| `aarch64-unknown-linux-gnu` | `linux_arm64` | `tar.gz` |
| `x86_64-pc-windows-gnu` | `windows_amd64` | `zip` |
| `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |
| `x86_64-apple-darwin` | `darwin_amd64` | `tar.gz` |

  Both `*-musl` targets are dropped; Windows STAYS `x86_64-pc-windows-gnu` (the
  msvc switch from revision 1 is reverted — see below); `x86_64-apple-darwin` is
  added. The `linux_arm64` name loses its `_gnu` disambiguation suffix (no musl
  sibling remains).
- **Why (user context, 2026-09-07):** the service is used by other people on
  different platforms, so a broad standard matrix is required — but the OLD matrix
  was not the current standard:
  - **musl out:** slow to compile (musl toolchain build + static link on every leg)
    and its only benefit is a fully static binary with no glibc dependency —
    irrelevant for end-user laptops/desktops shipping a normal distro. cargo-
    zigbuild's own docs position `*-musl` as "if you need a fully static binary".
  - **glibc:** no minimum-glibc suffix (e.g. `.2.28`) — zig's default minimum is
    broad enough for end-user machines; the suffix feature stays available if a
    minimum must be pinned later.
  - **Intel macOS in:** `x86_64-apple-darwin` covers the Intel Macs still in use.
- **windows-msvc TRIED AND REJECTED (2026-09-07, CI + local evidence):** the first
  run of this pipeline (34100195267) failed the msvc leg: the `ring` crate's C code
  could not compile — `fatal error: 'assert.h' file not found` (zig cc had no libc
  headers for the MSVC target). Local reproduction (Zig 0.16.0):
  `zig cc --target=x86_64-windows-msvc` → `error: unable to provide libc for target
  'x86_64-windows...msvc'` / `info: zig can provide libc for related target
  x86_64-windows-gnu`. **Zig ships no libc/headers for the MSVC target — only for
  windows-gnu.** cargo-zigbuild's "msvc support" (`src/zig.rs`, `tests/hello-
  windows`) applies to pure-Rust crates; this workspace compiles C/C++ (ring,
  libsqlite3-sys, usearch). The OLD AGENTS.md note "Zig cannot link MSVC ABI from a
  Linux host" was right in conclusion (the mechanism is C compilation, not linking).
- **usearch `Windows.h` case bug (found during local verification; affects the
  windows-gnu leg):** `include/usearch/index.hpp:78` does `#include <Windows.h>`
  (capital W). On a Linux host (case-sensitive FS) Zig's mingw headers only have
  `windows.h` → `fatal error: 'Windows.h' file not found` (reproduced locally; the
  lowercase include compiles). Fix: a one-line shim header
  `third_party/windows-case-shim/Windows.h` (`#include <windows.h>`) added to the
  C++ include path for the Windows leg via `CXXFLAGS_x86_64_pc_windows_gnu`
  (usearch's build.rs goes through cc-rs, which honors per-target env flags).
  Verified locally: full `x86_64-pc-windows-gnu` build succeeds with the shim.
  (Upstream usearch should use lowercase; the shim stays until it does.)
- **Risk (accepted):** `x86_64-unknown-linux-gnu` (replacing musl-amd64) and
  `x86_64-apple-darwin` are NEW legs for this workspace — verified by LOCAL builds
  of all 5 targets before the tag re-push (the darwin legs need the D9 macOS SDK),
  then by the first release run.
- **Archive contents per leg (unchanged, `make-ci-gitea-compatible` D4):** binary
  (`synopsis` / `synopsis.exe`, already stripped by the profile — D5),
  `README.md`, `workspace/configs/**`,
  `workspace/datasets/edtech/ontology/**`. Naming `synopsis_<version>_<name>.<ext>`
  (`<version>` = tag without the leading `v`).

### D8 — rust-cache keys: per-leg stable keys + shared host key

- **Decision:**
  - `build` legs: `Swatinem/rust-cache@v2` with `key: ${{ matrix.name }}` → key
    `v0-rust-<name>-build-Linux-x64-<envHash>`, one stable slot per target.
  - `gate` job AND `ci` job: `shared-key: host` → key `v0-rust-host-Linux-x64-<envHash>`,
    one shared slot for the host dev-profile build (clippy + test + llvm-cov).
- **Why:** the action's default key is `v0-rust-{jobName}-{OS}-{envHash}` (source:
  `src/config.ts`; `add-job-id-key` defaults to true, and the "job id" is the
  `GITHUB_JOB` name). Two problems observed in runs 34100195267 / 34100170840
  (2026-09-07):
  1. **The job rename silently invalidated all saved caches.** The old jobs were
     `checks`/`coverage`; the new ones are `ci`/`gate`/`build` → brand-new keys →
     the whole run was cold (ci job: clippy 313s + test 298s + llvm-cov 551s).
  2. **All 5 matrix legs shared ONE key** (`v0-rust-build-...`) — last saver wins,
     so at most one target was warm per run and the other four rebuilt from
     scratch on every tag.
  With per-leg keys, after the first tag all 5 targets are warm on re-runs. With
  the shared `host` key, the rare `gate` (tag) run reuses the cache saved by the
  frequent `ci` (push) runs — the gate's clippy+test become warm.
- **Concurrent-save note:** `ci` and `gate` can run concurrently (push + tag) and
  save the same `host` key. GitHub cache saves are additive (each save is a new
  entry; restore picks the latest) and only successful jobs save
  (`cache-on-failure` defaults to false), so a partial/failed build cannot poison
  the key.
- **`llvm-cov` is a third build by design** (instrumented, own
  `target/llvm-cov-target` dir, ~9 min cold) — it is NOT reducible without dropping
  coverage from CI (a separate policy decision, D2). With a warm `host` cache the
  instrumented artifacts are restored too, so it is fast on re-runs.
- **Alternative rejected:** `restore-keys` prefix matching — not supported by this
  action (only `key` / `shared-key` inputs exist).

### D9 — macOS SDK for the darwin legs

- **Decision:** the two darwin legs get Apple's `MacOSX11.3.sdk` (48.85 MB
  tarball from the `phracker/MacOSX-SDKs` GitHub release — the exact SDK the
  official cargo-zigbuild Docker image ships) downloaded to `$HOME/macosx-sdk/`,
  cached with `actions/cache@v4` (`key: macosx-sdk-11.3`), and
  `SDKROOT=$HOME/macosx-sdk/MacOSX11.3.sdk` exported in the build script for the
  darwin legs.
- **Why:** rustc's linker driver locates the macOS SDK via
  `xcrun --sdk macosx --show-sdk-path` — which does not exist on a Linux host →
  the final link fails: `error: linking with zigcc-<target> wrapper failed`
  (reproduced locally: both darwin legs compiled every dependency, then failed
  linking the `cli` binary). The cargo-zigbuild README documents `SDKROOT` as the
  mechanism ("Path to macOS SDK (auto-detected on macOS)"), and its official
  Dockerfile installs exactly this SDK:
  `curl ... MacOSX11.3.sdk.tar.xz | tar -J -x -C /opt; ENV SDKROOT=/opt/MacOSX11.3.sdk`.
  The SDK also covers the C/C++ dependencies (ring, libsqlite3-sys, usearch):
  zig cc for darwin targets reads `SDKROOT`, because zig bundles no darwin system
  libraries.
- **Evidence:** local reproduction without the SDK (both legs failed at the cli
  link with the xcrun warning); local build with `SDKROOT` set — both legs
  produce stripped Mach-O binaries (verified 2026-09-07).
- **Why 11.3:** the exact SDK of the official cargo-zigbuild Dockerfile — a
  proven combination with cargo-zigbuild 0.23.x + Zig 0.16. The binary uses no
  macOS frameworks, so the SDK version only affects the embedded SDK-version
  stamp.
- **Alternative rejected:** a container image with the SDK preinstalled (the
  cargo-zigbuild Docker image is pinned to Rust 1.93.0 and would need a custom
  build) — the download + cache approach keeps the legs on plain `ubuntu-latest`
  and costs one 51 MB download per cold cache.

## Action pins (carried over, verified 2026-09-04)

`actions/checkout@v7`, `dtolnay/rust-toolchain@1.96.0` (must match
`rust-toolchain.toml`), `Swatinem/rust-cache@v2`, `actions/cache@v4` (macOS SDK,
D9), `taiki-e/install-action@v2` (cargo-llvm-cov 0.9.0), `mlugg/setup-zig@v2`
(Zig 0.16.0), `actions/upload-artifact@v4`, `actions/download-artifact@v4`,
`softprops/action-gh-release@v3`.

## Risks

- **Runner-minutes ≈ the same for the release** (5 build legs + gate + publish),
  but wall time drops ~3×; dev CI halves its runner usage.
- **`zip` availability** for the Windows leg: present on GitHub `ubuntu-latest`
  (same assumption as before).
- **First tag after the key change is cold** (new `v0-rust-host-...` and
  `v0-rust-<name>-build-...` namespaces): one-time cost; every subsequent
  push/tag run is warm (D8).
- **Windows case shim** (`third_party/windows-case-shim/Windows.h`): a one-line
  header on the C++ include path; if upstream usearch switches to lowercase
  `windows.h`, the shim becomes a harmless no-op (it is only searched when the
  real header is not found first).
- **Re-trigger of `v0.1.0`:** the tag must be deleted and re-pushed after the new
  pipeline lands (an operational step outside this change's file scope); the existing
  manual release body is replaced by the script output (empty for the first release)
  and then set manually once.
