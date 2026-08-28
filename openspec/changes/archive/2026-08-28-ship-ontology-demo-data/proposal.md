# Proposal: Ship ontology XML and demo data

## Problem

The shipped `configs/config.default.yaml` sets `global_config_path: "data/ontology"`, and the
Rust loader (`crates/config/src/ontology.rs`) expects `data/ontology/global.xml` plus
`data/ontology/domains/*.xml` to exist. They are **not** present in the repo, so the binary
cannot load the cross-domain ontology out-of-the-box. Separately, there is no demo corpus to
exercise ingestion, so a fresh clone has nothing to ingest and the product looks empty.

The Go oracle (`../synopsis`, read-only) carries both: `data/ontology/{global.xml, domains/*.xml}`
and a demo corpus at `data/storage/edtech/` (markdown docs + mediawiki wiki + scraped website).

## Proposed

1. Ship the ontology XML files from the oracle into `data/ontology/` (verbatim, with one
   documented path adaptation — see design D1).
2. Ship the full demo corpus from the oracle into `data/demo/edtech/` (user decision: all of it).
3. Add `configs/config.demo.yaml` preset so `synopsis serve --config configs/config.demo.yaml`
   ingests the demo out-of-the-box.
4. Add `.gitignore` exceptions so `data/ontology/` and `data/demo/` are tracked (currently
   `data/*` is ignored except `data/README.md`).
5. Document provenance in `data/README.md` (no sha256 table — user decision).

## Non-goals

- No change to frozen contracts (MCP tools, CLI surface, data schema, config *format*).
  `configs/config.demo.yaml` is an additive preset file, not a format change.
- No migration of the Go `knowledge.db`; demo data is raw markdown/mediawiki/html for ingestion.
- No code changes to loaders; only packaging + one path adaptation inside `global.xml`.

## Decision (user-approved 2026-08-28)

- Q1 demo scope: **all** of `data/storage/edtech/` (incl. `wiki/` + `site/`, ~46 MB, mostly images).
- Q2 demo config: **yes**, create `configs/config.demo.yaml`.
- Q3 ontology sha256: **no** sha256 table in README (verbatim copy, trust).
- Q4 demo directory: **`data/demo/`**.
