# Test fixtures — provenance

The files in this directory are **verbatim copies** of configuration sources from
the Go oracle (`../synopsis`). They are committed so the Rust test suite runs
without the oracle present (CI never has `../synopsis` on disk). If an oracle file
changes, re-copy it and update the SHA-256 below — do not edit these files by hand.

## `config.default.yaml`

| Field | Value |
|---|---|
| Source | `../synopsis/configs/config.default.yaml` |
| Copied on | 2026-08-18 (task config-module 1.1) |
| SHA-256 | `2b76bc0bdabec7e4734a38b9a77e30b763705f31825624b29068066a475c4779` |

Regenerate:

```sh
cp ../synopsis/configs/config.default.yaml crates/config/tests/data/config.default.yaml
sha256sum crates/config/tests/data/config.default.yaml   # update the table above if it changed
```

> Note on location: fixtures live under `tests/data/`, not `fixtures/` — the root
> `.gitignore` ignores `fixtures/*` at any depth, which would drop these files from
> version control (design D10).
