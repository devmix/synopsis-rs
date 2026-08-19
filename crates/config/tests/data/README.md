# Test fixtures — provenance

The files in this directory are copies of configuration sources from the Go oracle
(`../synopsis`) — verbatim, except `global.xml`, which is adapted to the D15 XML contract (see
its section). They are committed so the Rust test suite runs without the oracle present (CI never
has `../synopsis` on disk). If an oracle file changes, re-copy it and update the SHA-256 below —
do not edit these files by hand.

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

## `onnx.yaml`

| Field | Value |
|---|---|
| Source | `../synopsis/configs/onnx.yaml` (byte-identical to `onnx.yaml.default`) |
| Copied on | 2026-08-19 (task config-module 2.1) |
| SHA-256 | `3a079c5747bb58731119da52b03508288459fcc9996004ae6317fda2a0d3c177` |

Regenerate:

```sh
cp ../synopsis/configs/onnx.yaml crates/config/tests/data/onnx.yaml
sha256sum crates/config/tests/data/onnx.yaml   # update the table above if it changed
```

## `global.xml`

**Adapted copy**, not byte-identical: the D15 XML contract (design decision 2026-08-19, change
`config-module`, revision 4) puts every group of repeated elements inside a plural wrapper —
`<entities>`, `<relations>`, `<sources>` under `<global>`, `<domains>` inside each `<source>`,
`<expressions>` and `<methods>` inside `<cross-domain-links>`, `<methods>` inside `<ner>`,
`<attributes>` inside every `<entity>`/`<relation>`, `<synonyms>` inside every `<entity>`, and
`<regex-rules>` inside `<extraction>`. The oracle file already carries the first five wrappers;
the adaptation **adds** 36 wrapper lines — one pair around each run of consecutive `<attribute/>`
(4 entity + 6 relation runs), `<synonym>` (5 entity runs), `<method>` (cross-domain-links and ner)
and `<regex/>` (extraction) elements. Each added line carries the indent of its first wrapped
line, so every original byte — element names, attributes, text, XML comments, even the
attribute-only spelling `<method>equals</method>` (see below) — is unchanged.

| Field | Value |
|---|---|
| Source | `../synopsis/data/ontology/global.xml` |
| Adapted on | 2026-08-19 (task config-module 3.1, revision 4 / design D15) |
| SHA-256 of the adapted fixture | `99732219c3af5bf92eb6b6526ed87f92bbbe45fec4f84ab70dc2a39000ce250d` |
| SHA-256 of the oracle original (pre-adaptation) | `759089a409721d0bbc3b136b341a892f6b49a572ad926809a29dcfca1a6b6a5d` |

Regenerate (re-copy, then insert the wrapper pairs described above; a line-based script suffices —
wrap each maximal run of consecutive `<attribute/>` lines inside an `<entity>`/`<relation>`,
each `<synonym>` run inside an `<entity>`, each `<method>` run in `<cross-domain-links>` and
`<ner>`, and the `<regex/>` line inside `<extraction>`, with the wrapper at the indent of its
first wrapped line):

```sh
cp ../synopsis/data/ontology/global.xml crates/config/tests/data/global.xml
# ... insert the 36 wrapper lines per the rule above ...
sha256sum crates/config/tests/data/global.xml   # must match the table; update it if the oracle changed
diff <(grep -vE '^[[:space:]]*</?(attributes|synonyms|methods|regex-rules)>[[:space:]]*$' \
  crates/config/tests/data/global.xml) ../synopsis/data/ontology/global.xml   # must be empty
```

> Note on `<method>equals</method>`: that line in the oracle file is an element with one
> empty-valued *attribute* and no text. Both Go's `encoding/xml` and quick-xml surface a lone
> empty-valued attribute name as the element's string content, so it parses to `"equals"` in
> both parsers — the fixture keeps this spelling verbatim because it is what the oracle ships
> and both implementations agree on it (verified against the Go oracle 2026-08-19).

> Note on location: fixtures live under `tests/data/`, not `fixtures/` — the root
> `.gitignore` ignores `fixtures/*` at any depth, which would drop these files from
> version control (design D10).
