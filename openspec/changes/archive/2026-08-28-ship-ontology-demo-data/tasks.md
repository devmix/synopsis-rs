# Tasks: Ship ontology XML and demo data

## 1.1 Ship ontology XML (`data/ontology/`)

**Goal:** Copy the oracle ontology into the repo so the binary loads it out-of-the-box.

**Scope файлов:**
- `data/ontology/global.xml` (NEW — verbatim from `../synopsis/data/ontology/global.xml` EXCEPT the 8 `<source path=>` prefixes rewritten `./data/storage/edtech/` → `./data/demo/edtech/`)
- `data/ontology/domains/domain_hr.xml` (NEW — byte-identical to `../synopsis/data/ontology/domains/domain_hr.xml`)
- `data/ontology/domains/domain_it.xml` (NEW — byte-identical)
- `data/ontology/domains/domain_product.xml` (NEW — byte-identical)
- `.gitignore` (add `!data/ontology/`)
- `data/README.md` (add Ontology provenance section; NO sha256 per Q3)

**Dependencies:** none.

**Критерии приёмки:**
- `data/ontology/global.xml` exists and is well-formed XML (`xmllint --noout` or `python3 -c "import xml.dom.minidom; xml.dom.minidom.parse('data/ontology/global.xml')"`).
- All 8 `<source path=>` entries in `global.xml` reference `./data/demo/edtech/...` (none reference `./data/storage/edtech/`).
- The three domain XMLs are byte-identical to the oracle (verify with `cmp`).
- `git check-ignore data/ontology/global.xml` returns NON-ignored (tracked); `git check-ignore data/models` still ignored.
- `data/README.md` documents the ontology source + D1 path adaptation.
- `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean (packaging only; no code change expected).

**Oracle reference:** `../synopsis/data/ontology/{global.xml, domains/*.xml}`.

## 1.2 Ship demo data (`data/demo/edtech/`)

**Goal:** Copy the full oracle demo corpus so ingestion has something to consume.

**Scope файлов:**
- `data/demo/edtech/**` (NEW — entire `../synopsis/data/storage/edtech/` tree: `documents/`, `wiki/`, `site/`)
- `.gitignore` (add `!data/demo/`)
- `data/README.md` (add Demo data section: source, destination, size ~46 MB, ingest command)

**Dependencies:** 1.1 (global.xml paths point here).

**Критерии приёмки:**
- `data/demo/edtech/documents/`, `data/demo/edtech/wiki/`, `data/demo/edtech/site/` all present with the same subtree as the oracle.
- `git check-ignore data/demo/edtech/documents/hr/hiring_policy.md` returns NON-ignored (tracked); `git check-ignore data/models` still ignored.
- `data/README.md` documents demo data + `synopsis serve --config configs/config.demo.yaml`.
- `cargo fmt --check` / `cargo clippy` clean.

**Oracle reference:** `../synopsis/data/storage/edtech/` (full tree).

## 1.3 Demo config preset (`configs/config.demo.yaml`)

**Goal:** Provide a one-command preset that ingests the demo corpus.

**Scope файлов:**
- `configs/config.demo.yaml` (NEW — copy of `configs/config.default.yaml` with a header comment; sources come from `global.xml` which already points at `data/demo/edtech/`)
- `crates/config/src/preset.rs` (optional: add a `#[cfg(test)]` test `loads_demo_config` that calls the config loader on `configs/config.demo.yaml` and asserts it parses; if a loader entrypoint is not easily reachable, skip and rely on `synopsis --version`/manual parse)

**Dependencies:** 1.1, 1.2.

**Критерии приёмки:**
- `configs/config.demo.yaml` is valid YAML and parses via the existing config loader (verified by the unit test above, or by `cargo run -p cli -- --config configs/config.demo.yaml serve --help` returning 0 without panicking on config load).
- The preset does not alter frozen config *format*; it is an additive preset.
- `cargo test -p config` passes (if test added) and `cargo clippy --workspace --all-targets -- -D warnings` clean.

**Oracle reference:** `../synopsis/configs/config.default.yaml` (template for the preset).
