# workspace/ — layout, config presets, and runtime artifacts

`workspace/` is the single root for all non-code artifacts of the `synopsis` binary:
tracked configuration presets and the demo dataset (ontology + corpus), plus gitignored
downloaded/runtime artifacts. The split is **global vs per-dataset** (design D1 of
change `storage-layout-restructure`): models, the ONNX runtime, and the cache DB are
global; ontology, content, knowledge DB, and vectors belong to one dataset
(`dataset.name` in the preset, `--dataset` CLI override).

## Layout: tracked vs gitignored

| Path | Tracked? | Contents |
|---|---|---|
| `workspace/configs/` | tracked | presets (`config.default.yaml`, `config.demo.yaml`), `onnx.yaml` (ONNX model/runtime registry), minijinja prompt templates under `prompts/` |
| `workspace/models/` | gitignored | ONNX model files downloaded per `configs/onnx.yaml` (bge-m3-int8, bge-small-en-v1.5, paraphrase-multilingual-MiniLM-L12-v2) |
| `workspace/onnxruntime/` | gitignored | ONNX Runtime shared library (`libonnxruntime.so.1.28.0`) |
| `workspace/db/cache/cache.db` | gitignored | GLOBAL cache DB (LLM linker/NER + embeddings) |
| `workspace/datasets/<name>/ontology/` | tracked (edtech) | ontology XML: `global.xml` + `domains/*.xml` |
| `workspace/datasets/<name>/content/` | tracked (edtech) | ingestion corpus: `documents/`, `wiki/`, `site/` |
| `workspace/datasets/<name>/state/` | gitignored | per-dataset runtime state, created from scratch on first ingestion: `db/knowledge.db` (dataset-bound knowledge DB) + `vectors/` (dataset-bound Lance ANN index) |

Legacy-project artifacts (`state_store.db` BadgerDB dir, `stream_store`,
`vectors.lance`) are NOT carried over: Rust builds its own DB and vectors from scratch
and never opens the legacy DB (AGENTS.md hard rule).

## Config presets

Default configuration presets for the `synopsis` binary, resolved relative to the
executable / CWD. Without `--config` the binary uses `config.default.yaml` (preset
`default`) and `onnx.yaml` (the ONNX model/runtime registry).

| File | Provenance | Notes |
|---|---|---|
| `onnx.yaml` | Byte-for-byte from the legacy config `configs/onnx.yaml` | `runtime.platforms[*]` + `models.entries[*]` (bge-m3-int8, bge-small-en-v1.5, paraphrase-multilingual-MiniLM-L12-v2). Unchanged. |
| `config.default.yaml` | Based on the legacy default `configs/config.default.yaml` | Deliberate deviation: `embeddings.local.model_name` is `bge-m3-int8` with `vector_dim: 1024` (Rust default per the frozen stack) instead of the legacy default `bge-small-en-v1.5` (dim 384). All other sections follow the legacy structure. |
| `prompts/entity-linker/{system,user}.tmpl` | Rust minijinja templates (NOT a copy of the legacy `configs/prompts/entity-linker/*.tmpl`) | The legacy prompts are Go `text/template` and do not parse under minijinja, so they were re-expressed in minijinja with the same prompt text and data shape (field paths `entity_a.*` / `entity_b.*` instead of `.EntityA.*` / `.EntityB.*`; `join`/`truncate` are minijinja functions; context numbering uses a registered `enumerate` filter). Byte-identical to the embedded defaults in `crates/graph/src/templates/entity-linker/`. |
| `prompts/ner/{system,user}.tmpl` | Rust minijinja templates (NOT a copy of the legacy `configs/prompts/ner/*.tmpl`) | Same engine reason as above. Byte-identical to the embedded defaults in `crates/ingestion/src/ner/templates/`. |

### Prompt override semantics

Templates are loaded from `paths.prompts_path` (default `workspace/configs/prompts`): a
present `{system,user}.tmpl` file overrides the embedded default and is noted in the
run's notes; a missing file silently falls back to the embedded default. Because the
shipped files are byte-identical to the embedded defaults, the template-source SHA-256
hashes (the decision-cache key) are the same either way.

## Runtime artifacts (preloaded per onnx.yaml)

Binary artifacts for the native-seam spikes and later product crates, laid out exactly as
the legacy runtime's `onnxruntime_go.SetSharedLibraryPath` / model cache expect:

| Path | Declared in | Size (bytes) | sha256 |
|---|---|---|---|
| `workspace/onnxruntime/libonnxruntime.so.1.28.0` | `configs/onnx.yaml` → runtime.platforms[linux-amd64] | 24 268 848 | `1461ef7cc3d9e49982591721683cc3e3a55580aeca9a5254e7aac47b75ee4bab` |
| `workspace/models/bge-m3-int8/model.onnx` | models.entries[bge-m3-int8].files[0] | 724 923 | `f84251230831afb359ab26d9fd37d5936d4d9bb5d1d5410e66442f630f24435b` |
| `workspace/models/bge-m3-int8/model.onnx_data` | models.entries[bge-m3-int8].files[1] | 2 266 820 608 | `1eebfb28493f67bba03ce0ef64bfdc7fc5a3bd9d7493f818bb1d78cd798416b4` |
| `workspace/models/bge-m3-int8/tokenizer.json` | models.entries[bge-m3-int8].files[2] | 17 082 821 | `6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790` |
| `workspace/models/bge-small-en-v1.5/model.onnx` | models.entries[bge-small-en-v1.5].files[0] (registry **default**) | 133 093 490 | `828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35` |
| `workspace/models/bge-small-en-v1.5/tokenizer.json` | models.entries[bge-small-en-v1.5].files[1] | 711 396 | `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` |
| `workspace/models/bge-small-en-v1.5/tokenizer_config.json` | models.entries[bge-small-en-v1.5].files[2] | 366 | `9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3` |
| `workspace/models/paraphrase-multilingual-MiniLM-L12-v2/model.onnx` | models.entries[paraphrase-multilingual-MiniLM-L12-v2].files[0] | 470 301 610 | `10f7a088420252b26caf819236ca2c9d2987afd0fc06fec7553b542a5655a05a` |
| `workspace/models/paraphrase-multilingual-MiniLM-L12-v2/tokenizer.json` | models.entries[paraphrase-multilingual-MiniLM-L12-v2].files[1] | 9 081 518 | `2c3387be76557bd40970cec13153b3bbf80407865484b209e655e5e4729076b8` |

### Provenance (task 2.1, 2026-08-18)

Preloaded by copying the legacy project's own already-downloaded artifacts — the legacy
downloader (`internal/onnx/downloader.go` in the legacy repo) fetched these from the URLs
in the legacy `configs/onnx.yaml` and verified them against `size_bytes`. The copy
command below is a one-time historical seeding step (the artifacts are already committed
and the legacy repo is no longer present; they then lived under `data/`, and task 1.4 of
`storage-layout-restructure` relocated them to `workspace/`):

```sh
cp <legacy repo>/data/models/bge-m3-int8/{model.onnx,model.onnx_data,tokenizer.json} data/models/bge-m3-int8/
cp <legacy repo>/data/models/bge-small-en-v1.5/{model.onnx,tokenizer.json,tokenizer_config.json} data/models/bge-small-en-v1.5/
cp "<legacy repo>/data/models/paraphrase-multilingual-MiniLM-L12-v2"/{model.onnx,tokenizer.json} "data/models/paraphrase-multilingual-MiniLM-L12-v2/"
cp <legacy repo>/data/onnxruntime/libonnxruntime.so.1.28.0 data/onnxruntime/
```

Every file's size was re-verified against the `size_bytes` declared in onnx.yaml (all eight model
files + runtime lib match exactly); sha256 recorded above for future spot-checks. The spike
(`cargo run -p spikes --bin s2_onnx`) re-checks existence + size of all three models at every start
and fails closed otherwise. All three registry models are covered by the spike (Revision 2,
task 2.1), including `bge-small-en-v1.5` which is the default model of both onnx.yaml and
config.default.yaml.

Regeneration from scratch (when the legacy copy is unavailable): fetch each URL from onnx.yaml,
verify `size_bytes`, place under the same layout; for the runtime archive extract
`onnxruntime-linux-x64-1.28.0/lib/libonnxruntime.so.1.28.0`.

## Dataset `edtech` — ontology (task 1.1, change ship-ontology-demo-data)

Shipped verbatim from the legacy repo so the binary loads the ontology out-of-the-box:

| File | Source in legacy repo |
|---|---|
| `workspace/datasets/edtech/ontology/global.xml` | `<legacy repo>/data/ontology/global.xml` (9480 B before adaptation) |
| `workspace/datasets/edtech/ontology/domains/domain_hr.xml` | `<legacy repo>/data/ontology/domains/domain_hr.xml` (1935 B, byte-identical) |
| `workspace/datasets/edtech/ontology/domains/domain_it.xml` | `<legacy repo>/data/ontology/domains/domain_it.xml` (6093 B, byte-identical) |
| `workspace/datasets/edtech/ontology/domains/domain_product.xml` | `<legacy repo>/data/ontology/domains/domain_product.xml` (5347 B, byte-identical) |

**Path adaptation (design D1):** the legacy `global.xml` declares ingestion sources under
`<sources>` with 8 `<source path=>` entries pointing at `./data/storage/edtech/...`. In this repo
the demo corpus lives under `workspace/datasets/edtech/content/`, so those 8 prefixes were
rewritten `./data/storage/edtech/` → `./data/demo/edtech/` (9456 B after adaptation; task 1.1 of
`ship-ontology-demo-data`) and then `./data/demo/edtech/` →
`./workspace/datasets/edtech/content/` (task 1.4 of `storage-layout-restructure`). Every other
byte of `global.xml` (entities, relations, expressions, extraction, cross-domain links) is
identical to the legacy file; the three domain XMLs are byte-identical (`cmp`). This is a path
adaptation, not a semantic change. No sha256 table is kept for these files (user decision Q3=no).

## Dataset `edtech` — demo corpus (task 1.2, change ship-ontology-demo-data)

Full demo corpus copied from the legacy repo so ingestion has something to consume:

- **Source in legacy repo:** `<legacy repo>/data/storage/edtech/` (entire tree: `documents/`, `wiki/`, `site/`).
- **Destination:** `workspace/datasets/edtech/content/` (originally `data/demo/edtech/`, user
  decision Q4; relocated by task 1.4 of `storage-layout-restructure`; the `documents/`, `wiki/`,
  `site/` subtree is preserved exactly — markdown docs, mediawiki wiki, scraped website
  HTML/static, and images).
- **Size:** ~46 MB (37 png, 19 md, 12 json, 2 jpg, 2 gitkeep). The repo-growth tradeoff is
  accepted by the user (Q1 = all). No sha256 table (Q3=no).
- **Ingest:** `synopsis serve --config workspace/configs/config.demo.yaml` — the demo preset
  (task 1.3) makes the binary ingest this corpus; the ingestion sources themselves are declared
  in `workspace/datasets/edtech/ontology/global.xml` (pointing at
  `./workspace/datasets/edtech/content/...`).

Copy command: `cp -r <legacy repo>/data/storage/edtech/. data/demo/edtech/` (verified
byte-identical with `diff -rq`; one-time historical seeding step — the legacy repo is no
longer present and the corpus is already committed).
