# Design: Ship ontology XML and demo data

## D1 — Ontology copy (with path adaptation)

**Source (read-only oracle):** `../synopsis/data/ontology/global.xml` (9480 B) and
`../synopsis/data/ontology/domains/{domain_hr.xml (1935 B), domain_it.xml (6093 B), domain_product.xml (5347 B)}`.

**Destination:** `data/ontology/global.xml` + `data/ontology/domains/*.xml`.

The ingestion sources are declared inside `global.xml` (`<sources>`), NOT in `config.default.yaml`.
The oracle's `<source path=>` entries point at `./data/storage/edtech/...`. To honor the user's
Q4 decision (`data/demo/`), `global.xml` is copied **verbatim except for the 8 `<source path=>`
prefixes**, rewritten from `./data/storage/edtech/` to `./data/demo/edtech/`. Every other byte of
`global.xml` (entities, relations, expressions, extraction, cross-domain links) is identical to the
oracle. The three domain XMLs are copied **byte-identical**.

- *Alternative considered:* place the demo tree at `data/storage/edtech/` and keep `global.xml`
  fully verbatim. **Rejected** — user explicitly chose `data/demo/` (Q4).
- *Alternative considered:* keep `data/demo/` but also keep `global.xml` verbatim (paths pointing at
  `data/storage/edtech/`). **Rejected** — the binary would fail to find sources and ingest nothing.

This is a path adaptation, not a semantic change; it is recorded in `data/README.md`.

## D2 — Demo data copy

**Source:** entire `../synopsis/data/storage/edtech/` (~46 MB; 37 png, 19 md, 12 json, 2 jpg, 2 gitkeep).
**Destination:** `data/demo/edtech/` (preserving `documents/`, `wiki/`, `site/` subtree).

User decision Q1 = **all** (including `wiki/` images and `site/` scraped HTML/static). The 46 MB
repo-growth tradeoff is accepted by the user.

## D3 — Demo config preset

`configs/config.demo.yaml` is created as a copy of `configs/config.default.yaml` with a header
comment explaining it is the demo preset. Because sources live in `global.xml` (D1 already points
them at `data/demo/edtech/`), this preset makes `synopsis serve --config configs/config.demo.yaml`
ingest the demo corpus. No source paths are duplicated in the config.

## D4 — .gitignore exceptions

Current rule: `data/*` ignored, `!data/README.md` tracked. Add, after that exception:

```
!data/ontology/
!data/demo/
```

Runtime artifacts (`data/models/`, `data/onnxruntime/`, `data/vectors.lance`, `data/state_store.db`,
`data/stream_store/`) MUST remain ignored — verify with `git check-ignore` that they are still
ignored after the change.

## D5 — README provenance

Extend `data/README.md` with two sections:
- **Ontology** — source paths in oracle, the D1 path adaptation note, file list (no sha256 per Q3).
- **Demo data** — source path in oracle, destination `data/demo/edtech/`, how to ingest
  (`synopsis serve --config configs/config.demo.yaml`), and the ~46 MB size note.

## Oracle references

- `../synopsis/data/ontology/global.xml`
- `../synopsis/data/ontology/domains/{domain_hr,domain_it,domain_product}.xml`
- `../synopsis/data/storage/edtech/` (full tree)

## Risks

- 46 MB binary images committed to git (accepted, Q1=all).
- `.gitignore` glob must not accidentally untrack runtime artifacts — verified in D4.
- `global.xml` path rewrite must keep valid XML and match the demo tree exactly.
