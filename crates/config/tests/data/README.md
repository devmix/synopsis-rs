# Test fixtures

The files in this directory are self-contained test fixtures for this crate, recorded from the
original implementation's config/ontology files. `config.default.yaml` and `onnx.yaml` are
byte-for-byte copies; `global.xml` and the `domains/` fixtures are adapted to the D15 XML
contract (see their sections). If a fixture changes, update the SHA-256 below — do not edit the
fixture files by hand.

## `config.default.yaml`

Byte-for-byte copy of the original implementation's `configs/config.default.yaml`.

| Field | Value |
|---|---|
| Recorded on | 2026-08-18 (task config-module 1.1) |
| SHA-256 | `2b76bc0bdabec7e4734a38b9a77e30b763705f31825624b29068066a475c4779` |

Verify:

```sh
sha256sum crates/config/tests/data/config.default.yaml   # must match the table above
```

## `onnx.yaml`

Byte-for-byte copy of the original implementation's `configs/onnx.yaml` (byte-identical to
`onnx.yaml.default`).

| Field | Value |
|---|---|
| Recorded on | 2026-08-19 (task config-module 2.1) |
| SHA-256 | `3a079c5747bb58731119da52b03508288459fcc9996004ae6317fda2a0d3c177` |

Verify:

```sh
sha256sum crates/config/tests/data/onnx.yaml   # must match the table above
```

## `global.xml`

**Adapted copy**, not byte-identical: the D15 XML contract (design decision 2026-08-19, change
`config-module`, revision 4) puts every group of repeated elements inside a plural wrapper —
`<entities>`, `<relations>`, `<sources>` under `<global>`, `<domains>` inside each `<source>`,
`<expressions>` and `<methods>` inside `<cross-domain-links>`, `<methods>` inside `<ner>`,
`<attributes>` inside every `<entity>`/`<relation>`, `<synonyms>` inside every `<entity>`, and
`<regex-rules>` inside `<extraction>`. The recorded source already carries the first five
wrappers; the adaptation **adds** 36 wrapper lines — one pair around each run of consecutive
`<attribute/>` (4 entity + 6 relation runs), `<synonym>` (5 entity runs), `<method>`
(cross-domain-links and ner) and `<regex/>` (extraction) elements. Each added line carries the
indent of its first wrapped line, so every source byte — element names, attributes, text, XML
comments, even the attribute-only spelling `<method>equals</method>` (see below) — is unchanged.

| Field | Value |
|---|---|
| Recorded from | the original implementation's `data/ontology/global.xml` |
| Adapted on | 2026-08-19 (task config-module 3.1, revision 4 / design D15) |
| SHA-256 of the adapted fixture | `99732219c3af5bf92eb6b6526ed87f92bbbe45fec4f84ab70dc2a39000ce250d` |
| SHA-256 of the recorded source (pre-adaptation) | `759089a409721d0bbc3b136b341a892f6b49a572ad926809a29dcfca1a6b6a5d` |

Verify (strip the added wrapper lines; the result must be exactly the recorded source, i.e. the
diff against it must be empty — the 36 added lines are the only difference):

```sh
grep -vE '^[[:space:]]*</?(attributes|synonyms|methods|regex-rules)>[[:space:]]*$' \
  crates/config/tests/data/global.xml
sha256sum crates/config/tests/data/global.xml   # must match the table above
```

> Note on `<method>equals</method>`: that line in the recorded source is an element with one
> empty-valued *attribute* and no text. quick-xml surfaces a lone empty-valued attribute name as
> the element's string content, so it parses to `"equals"` — the fixture keeps this spelling
> verbatim because it is what the source ships (verified 2026-08-19).

## `domains/domain_hr.xml`, `domains/domain_it.xml`, `domains/domain_product.xml`

**Adapted copies**, not byte-identical: the same D15 XML contract as `global.xml` (revision 4).
The adaptation **adds** wrapper lines only — `<entities>`, `<relations>` around each run of
consecutive `<entity>`/`<relation>` items, `<attributes>` and `<synonyms>` inside their owners,
and `<regex-rules>` around the `<regex/>` line inside `<extraction>` (only `domain_it.xml` has a
rule; the other two `<extraction>` blocks stay empty). Each added line carries the indent of its
first wrapped line, so every source byte — element names, attributes, text, XML comments — is
unchanged. A section comment that separates two sections in the recorded source (e.g.
`<!-- Relation: salary_of -->`) stays where the source had it, i.e. just before the closing
wrapper tag.

Recorded from the original implementation's `data/ontology/domains/<file>`; adapted on
2026-08-19 (task config-module 3.2, revision 1 / design D15).

| File | SHA-256 of the adapted fixture | SHA-256 of the recorded source (pre-adaptation) |
|---|---|---|
| `domains/domain_hr.xml` | `f9815903dde5c43592d5dd45c1deaa21bf8baf9cb4e52e371f7c544820985f1a` | `1f81dc56ea9d884f2e2e13564fd526fa2846b6c3b580a52643ec003053ceaf31` |
| `domains/domain_it.xml` | `3869bb6f4bc8ed71c9059053da30fe84bb416d1a7a5862db51079b3c965c32c5` | `b53c90ca17a06a830a62dbc47faa8e67ce143362b2e6f897901e778fb8f5b853` |
| `domains/domain_product.xml` | `a7583bd13567d0e98652bb000dd944e901a64baa626c6592d469ea169dd2e600` | `a7dea6ec26832fdd47081f9bb7179004387aaf7af047a63517a807d952144898` |

Verify (strip the added wrapper lines; the result must be exactly the recorded source, i.e. the
diff against it must be empty — the added lines are the only difference; repeat per file):

```sh
grep -vE '^[[:space:]]*</?(entities|relations|attributes|synonyms|regex-rules)>[[:space:]]*$' \
  crates/config/tests/data/domains/domain_hr.xml
sha256sum crates/config/tests/data/domains/*.xml   # must match the table above
```

> Note on location: fixtures live under `tests/data/`, not `fixtures/` — the root
> `.gitignore` ignores `fixtures/*` at any depth, which would drop these files from
> version control (design D10).
