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
`zig objcopy` bug in the single-job loop.

Frozen stack + constraints: `openspec/config.yaml`. No recorded fixtures / contract
specs are the reference (no behavior change).

## Decisions

### D1 — `ci.yml`: one sequential job (checks + coverage merged)

- **Decision:** a single job `ci` (display name `Checks + Coverage (ubuntu-latest)`):
  checkout → toolchain 1.96.0 (components `clippy, rustfmt, llvm-tools-preview`) →
  `Swatinem/rust-cache@v2` → `cargo fmt --check` → `cargo clippy --all-targets --
  -D warnings` → `cargo test` → install `cargo-llvm-cov@0.9.0`
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
  1. `gate` — checkout → toolchain 1.96.0 (`clippy, rustfmt`) → rust-cache → fmt →
     clippy → test. No coverage (keeps the gate fast; coverage lives in dev CI).
  2. `build` (`needs: gate`) — `strategy.matrix.include` of the 5 rows (table below;
     each row carries `target`, `name`, `ext`). Each leg: checkout → toolchain 1.96.0
     (`targets: ${{ matrix.target }}`) → rust-cache → `cargo install --locked
     cargo-zigbuild` → `mlugg/setup-zig@v2` (0.16.0) → `cargo zigbuild --release
     --target` → strip (D5) → package `synopsis_<version>_<name>.<ext>` →
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

### D5 — `zig objcopy` fix: explicit output file

- **Decision:** `zig objcopy --strip-all "${bin}" "${bin}.stripped" && mv
  "${bin}.stripped" "${bin}"`.
- **Why:** verified against the Zig source (`lib/compiler/objcopy.zig`:
  `opt_output orelse fatal("expected output parameter")`) — `zig objcopy` takes
  input AND output positionals; there is no in-place mode. `--strip-all` is a
  supported option (ELF, PE/COFF, Mach-O). The two-step write-then-rename keeps the
  final binary path (`target/<target>/release/synopsis[.exe]`) unchanged for the
  packaging step.
- **Evidence:** release run 34092788497 — the first target compiled in 7m21s, then
  `error: expected output parameter`, exit 1.
- **Alternative rejected:** GNU `strip` — cannot handle Mach-O on the linux runner
  (and PE only with `llvm-strip`); `zig objcopy` already covers all three formats
  with one command (the original comment's rationale, kept).

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

### D7 — 2026 target matrix: drop musl, windows-gnu → msvc, add Intel macOS

- **Decision:** the 5-target matrix becomes:

| zigbuild target | `<name>` | `<ext>` |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `linux_amd64` | `tar.gz` |
| `aarch64-unknown-linux-gnu` | `linux_arm64` | `tar.gz` |
| `x86_64-pc-windows-msvc` | `windows_amd64` | `zip` |
| `aarch64-apple-darwin` | `darwin_arm64` | `tar.gz` |
| `x86_64-apple-darwin` | `darwin_amd64` | `tar.gz` |

  Both `*-musl` targets are dropped; `x86_64-pc-windows-gnu` is replaced by
  `x86_64-pc-windows-msvc`; `x86_64-apple-darwin` is added. The `linux_arm64` name
  loses its `_gnu` disambiguation suffix (no musl sibling remains).
- **Why (user context, 2026-09-07):** the service is used by other people on
  different platforms, so a broad standard matrix is required — but the OLD matrix
  was not the current standard:
  - **musl out:** slow to compile (musl toolchain build + static link on every leg)
    and its only benefit is a fully static binary with no glibc dependency —
    irrelevant for end-user laptops/desktops shipping a normal distro. cargo-
    zigbuild's own docs position `*-musl` as "if you need a fully static binary".
  - **windows-msvc in:** the standard Rust Windows target (what rustup installs by
    default on Windows). cargo-zigbuild explicitly supports `x86_64-pc-windows-msvc`
    and `aarch64-pc-windows-msvc` (dedicated handling in `src/zig.rs`: MSVC response
    files, `lib` archiver; `tests/hello-windows` covers it). The AGENTS.md note
    "Zig cannot link MSVC ABI from a Linux host" is stale (true for zig ~0.10 /
    2022; zig's linker gained MSVC-ABI support long ago).
  - **glibc:** no minimum-glibc suffix (e.g. `.2.28`) — zig's default minimum is
    broad enough for end-user machines; the suffix feature stays available if a
    minimum must be pinned later.
  - **Intel macOS in:** `x86_64-apple-darwin` covers the Intel Macs still in use.
- **Risk (accepted):** `windows-msvc` via zigbuild is a NEW build path for this
  project (the shipped matrix used `windows-gnu`); it is verified by the first
  release run. Fallback if it fails: one matrix row back to `x86_64-pc-windows-gnu`.
- **Archive contents per leg (unchanged, `make-ci-gitea-compatible` D4):** stripped
  binary (`synopsis` / `synopsis.exe`), `README.md`, `workspace/configs/**`,
  `workspace/datasets/edtech/ontology/**`. Naming `synopsis_<version>_<name>.<ext>`
  (`<version>` = tag without the leading `v`).

## Action pins (carried over, verified 2026-09-04)

`actions/checkout@v7`, `dtolnay/rust-toolchain@1.96.0` (must match
`rust-toolchain.toml`), `Swatinem/rust-cache@v2`, `taiki-e/install-action@v2`
(cargo-llvm-cov 0.9.0), `mlugg/setup-zig@v2` (Zig 0.16.0), `actions/upload-
artifact@v4`, `actions/download-artifact@v4`, `softprops/action-gh-release@v3`.

## Risks

- **Runner-minutes ≈ the same for the release** (5 build legs + gate + publish),
  but wall time drops ~3×; dev CI halves its runner usage.
- **`zip` availability** for the Windows leg: present on GitHub `ubuntu-latest`
  (same assumption as before).
- **Parallel cache saves** (5 legs saving the same rust-cache key): last save wins;
  the cache is content-addressed per rustc version, so a stale save cannot poison
  builds.
- **Re-trigger of `v0.1.0`:** the tag must be deleted and re-pushed after the new
  pipeline lands (an operational step outside this change's file scope); the existing
  manual release body is replaced by the script output (empty for the first release)
  and then set manually once.
