# data/ — runtime artifacts (preloaded per onnx.yaml)

Binary artifacts for the native-seam spikes and later product crates, laid out exactly as the
Go oracle's `onnxruntime_go.SetSharedLibraryPath` / model cache expect:

| Path | Declared in | Size (bytes) | sha256 |
|---|---|---|---|
| `data/onnxruntime/libonnxruntime.so.1.28.0` | `configs/onnx.yaml` → runtime.platforms[linux-amd64] | 24 268 848 | `1461ef7cc3d9e49982591721683cc3e3a55580aeca9a5254e7aac47b75ee4bab` |
| `data/models/bge-m3-int8/model.onnx` | models.entries[bge-m3-int8].files[0] | 724 923 | `f84251230831afb359ab26d9fd37d5936d4d9bb5d1d5410e66442f630f24435b` |
| `data/models/bge-m3-int8/model.onnx_data` | models.entries[bge-m3-int8].files[1] | 2 266 820 608 | `1eebfb28493f67bba03ce0ef64bfdc7fc5a3bd9d7493f818bb1d78cd798416b4` |
| `data/models/bge-m3-int8/tokenizer.json` | models.entries[bge-m3-int8].files[2] | 17 082 821 | `6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790` |
| `data/models/bge-small-en-v1.5/model.onnx` | models.entries[bge-small-en-v1.5].files[0] (registry **default**) | 133 093 490 | `828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35` |
| `data/models/bge-small-en-v1.5/tokenizer.json` | models.entries[bge-small-en-v1.5].files[1] | 711 396 | `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` |
| `data/models/bge-small-en-v1.5/tokenizer_config.json` | models.entries[bge-small-en-v1.5].files[2] | 366 | `9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3` |
| `data/models/paraphrase-multilingual-MiniLM-L12-v2/model.onnx` | models.entries[paraphrase-multilingual-MiniLM-L12-v2].files[0] | 470 301 610 | `10f7a088420252b26caf819236ca2c9d2987afd0fc06fec7553b542a5655a05a` |
| `data/models/paraphrase-multilingual-MiniLM-L12-v2/tokenizer.json` | models.entries[paraphrase-multilingual-MiniLM-L12-v2].files[1] | 9 081 518 | `2c3387be76557bd40970cec13153b3bbf80407865484b209e655e5e4729076b8` |

## Provenance (task 2.1, 2026-08-18)

Preloaded by copying the oracle's own already-downloaded artifacts — the Go downloader
(`../synopsis/internal/onnx/downloader.go`) fetched these from the URLs in
`../synopsis/configs/onnx.yaml` and verified them against `size_bytes`; the copy command:

```sh
cp ../synopsis/data/models/bge-m3-int8/{model.onnx,model.onnx_data,tokenizer.json} data/models/bge-m3-int8/
cp ../synopsis/data/models/bge-small-en-v1.5/{model.onnx,tokenizer.json,tokenizer_config.json} data/models/bge-small-en-v1.5/
cp "../synopsis/data/models/paraphrase-multilingual-MiniLM-L12-v2"/{model.onnx,tokenizer.json} "data/models/paraphrase-multilingual-MiniLM-L12-v2/"
cp ../synopsis/data/onnxruntime/libonnxruntime.so.1.28.0 data/onnxruntime/
```

Every file's size was re-verified against the `size_bytes` declared in onnx.yaml (all eight model
files + runtime lib match exactly); sha256 recorded above for future spot-checks. The spike
(`cargo run -p spikes --bin s2_onnx`) re-checks existence + size of all three models at every start
and fails closed otherwise. All three registry models are covered by the spike (Revision 2,
task 2.1), including `bge-small-en-v1.5` which is the default model of both onnx.yaml and
config.default.yaml.

Regeneration from scratch (when the oracle copy is unavailable): fetch each URL from onnx.yaml,
verify `size_bytes`, place under the same layout; for the runtime archive extract
`onnxruntime-linux-x64-1.28.0/lib/libonnxruntime.so.1.28.0`.
