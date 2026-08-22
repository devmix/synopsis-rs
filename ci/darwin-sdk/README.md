# ci/darwin-sdk

Vendored macOS framework stubs used **only** when cross-compiling the
`aarch64-apple-darwin` CI leg with `cargo zigbuild`.

## Why this exists

The `lancedb` dependency graph (task 1.1 of the `vectors` change) pulls in
`lance-arrow`, which is built as a `cdylib`. Linking that dylib for macOS
requires `-framework CoreFoundation`, because the chain

```
lance-arrow (cdylib) -> arrow -> chrono -> iana-time-zone -> core-foundation-sys
```

ends in `#[link(name = "CoreFoundation", kind = "framework")]` in
`core-foundation-sys`. At link time, `lld` needs a definition file for the
framework.

The official Zig 0.16.0 distribution — the same one CI installs via
`mlugg/setup-zig@v2` — bundles a macOS SDK containing only `libSystem.tbd` and
`SDKSettings.json`; **no framework stubs at all**. This is a long-standing
upstream limitation ([ziglang/zig#1349](https://github.com/ziglang/zig/issues/1349),
open since 2018): linking against any macOS framework from a non-macOS host
requires an externally provided stub or a full Xcode SDK.

The stub here is the minimal external provider: a `tapi-tbd` v4 file for
`CoreFoundation` that exports exactly the symbols the link references. At
runtime on a real Mac, dyld resolves the recorded install-name
(`/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation`)
to the real system framework — the stub is consumed only at link time and is
never shipped in the artifact.

## How it is wired

`.github/workflows/ci.yml` (cross-builds job) sets:

```
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=-F<repo>/ci/darwin-sdk"
```

`-F` adds a framework search path; `lld` finds
`ci/darwin-sdk/CoreFoundation.framework/CoreFoundation.tbd` when the link line
contains `-framework CoreFoundation`. The variable name is target-specific, so
it is inert on every other matrix leg and needs no per-target `if:`.

The framework subdirectory layout is required by `lld`'s framework search —
a flat `ci/darwin-sdk/CoreFoundation.tbd` is not found.

## Contents

- `CoreFoundation.framework/CoreFoundation.tbd` — tapi-tbd v4, `arm64-macos`,
  install-name of the real system framework, `current-version: 2447.0.0`
  (matches macOS 15). Exported symbols (all referenced by `iana-time-zone
  0.1.65`, the only crate in the graph with undefined `CF*` references):

  | Symbol | Referenced by |
  |---|---|
  | `_CFRelease` | `iana-time-zone` |
  | `_CFStringGetBytes` | `iana-time-zone` |
  | `_CFStringGetCStringPtr` | `iana-time-zone` |
  | `_CFStringGetLength` | `iana-time-zone` |
  | `_CFTimeZoneCopySystem` | `iana-time-zone` |
  | `_CFTimeZoneGetName` | `iana-time-zone` |
  | `_CFTimeZoneResetSystem` | `iana-time-zone` |

## How to update

If a future dependency change adds new `CF*` references and the darwin leg
fails with `undefined symbol: _CF...`, add the symbol to the `symbols:` list.
To enumerate the full set the link needs:

```sh
# after a failed darwin build, list undefined CF* symbols across the
# darwin-target rlibs that the link consumes:
llvm-nm -u target/aarch64-apple-darwin/debug/deps/libiana_time_zone-*.rlib | grep ' _CF'
```

Then re-run the darwin leg locally with the same mechanism CI uses:

```sh
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=-F$PWD/ci/darwin-sdk" \
  cargo zigbuild --release --target aarch64-apple-darwin
```

Only add symbols the linker actually demands — keep the stub minimal so it
does not drift into a fake SDK.
