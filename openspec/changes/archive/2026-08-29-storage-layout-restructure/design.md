# Design: Storage layout restructure

## D1 — Config model (`crates/config/src/preset.rs`)

**Global paths (resolved from `workspace_dir`):**
- `workspace/configs/onnx.yaml` (`onnx_config`, default `"workspace/configs/onnx.yaml"`)
- `workspace/configs/prompts` (`prompts_path`, default `"workspace/configs/prompts"`)
- `workspace/models` (`models_dir()`)
- `workspace/onnxruntime` (`onnxruntime_dir()`)
- `workspace/db/cache/cache.db` (`cache_db_path()` — GLOBAL cache: LLM linker/NER + embeddings)

**Per-dataset paths (`DatasetConfig { name }`, resolved from `workspace_dir/datasets/<name>/`):**
- `ontology_path()` → `.../ontology`
- `content_path()`  → `.../content`
- `state_path()`    → `.../state`
- `db_path()`       → `.../state/db/knowledge.db` (dataset-bound knowledge DB)
- `vectors_path()`  → `.../state/vectors` (dataset-bound ANN index)

**Struct changes:**
- `PathsConfig`: rename `data_dir` → `workspace_dir` (default `"workspace"`); DELETE `global_config_path`;
  DELETE `documents_dir`; keep `onnx_config` (default `"workspace/configs/onnx.yaml"`), `prompts_path` (default `"workspace/configs/prompts"`).
- `DatabaseConfig`: DELETE `cache_path`; DELETE `path` — the knowledge-DB path is NO LONGER configurable,
  it is derived automatically from `workspace_dir` + `dataset.name` (see `DatasetConfig::db_path`).
  ADD `cache_db_path()` on `PathsConfig` returning `workspace_dir.join("db").join("cache").join("cache.db")` (GLOBAL cache).
- ADD `DatasetConfig { name: String }` (default `""` — NO dataset by default; empty name means "no data")
  with the five helpers above. `db_path(&self, ws)` → `<ws>/datasets/<name>/state/db/knowledge.db` (derived, not configurable).
- `Config` gains `dataset: DatasetConfig`.

## D2 — CLI (`crates/cli/src/{cli.rs,serve/bootstrap.rs,serve/server.rs,sync.rs}`)

- REMOVE `--db` flag (cli.rs Arg "db", `sync.rs::db_path`, `bootstrap(cfg_path, db_path)` param).
- ADD `--dataset <name>` flag; overrides `config.dataset.name` before path resolution.
- `bootstrap()` drops the `db_path` parameter; DB path comes from `DatasetConfig::db_path(ws)`.
- **No-data semantics:** if `dataset.name` is EMPTY (default) OR `workspace/datasets/<name>/` does not
  exist → there is NO dataset: skip ontology loading, skip ingestion/indexing, run with no data
  (binary stays up, logs a warning, does NOT error). Only if `name` is set AND the dataset dir exists
  does bootstrap load ontology + ingest content + open the per-dataset DB/vectors.

## D3 — Code path resolution (embedding / vectors / db / cli)

- embedding: `LibraryManager::new(&config.paths.workspace_dir, &onnx)` and
  `new_onnx_provider(&config.embeddings.local, &config.paths.workspace_dir, &onnx)` — models and
  runtime resolve from `workspace_dir` (GLOBAL), NOT dataset.
- vectors: `LanceEngine::create(&dataset.vectors_path(), ...)` instead of `<data_dir>/vectors.lance`.
- db: knowledge DB via `dataset.db_path()`; cache via `config.paths.cache_db_path()` (global).
- cli bootstrap: `discover_domains(&dataset.ontology_path())` instead of `global_config_path`;
  watcher `load_global_config(&dataset.ontology_path())`; remove all `global_config_path` assignments
  (watcher.rs:1100/1125/1138).

## D4 — File moves (`git mv` / `cp` / `rm`)

- `configs/` → `workspace/configs/`
- `data/models/` → `workspace/models/`
- `data/onnxruntime/` → `workspace/onnxruntime/`
- `data/ontology/` → `workspace/datasets/edtech/ontology/`
- `data/demo/edtech/` → `workspace/datasets/edtech/content/`
- CREATE `workspace/db/cache/` (empty, gitignored)
- DELETE `data/` and `configs/`

**Go-created DB artifacts are NOT carried over.** `data/state_store.db` (BadgerDB dir),
`data/stream_store`, and `data/vectors.lance` are gitignored local artifacts produced by the Go
oracle / earlier experiments. Per the hard rule (AGENTS.md: Rust builds its own DB from scratch,
never opens/upgrades/migrates the legacy Go DB), they are DELETED by `rm -rf data` — NOT moved into
`workspace/`. Rust recreates the per-dataset `knowledge.db` and `vectors/` from scratch on first
ingestion. Do NOT create `workspace/datasets/edtech/state/db/knowledge.db` or
`workspace/datasets/edtech/state/vectors/` manually.

Rust builds its own DB/vectors from scratch, so these are file relocations, NOT schema migrations.

## D5 — `global.xml` source rewrite

`workspace/datasets/edtech/ontology/global.xml`: rewrite the 8 `<source path=>` entries from
`./data/demo/edtech/` to `./workspace/datasets/edtech/content/`. All other bytes identical.

## D6 — `.gitignore`

Replace `data/*` (except `!data/README.md`) with:
```
workspace/*
!workspace/configs/
!workspace/datasets/edtech/ontology/
!workspace/datasets/edtech/content/
```
and keep runtime artifacts ignored: `workspace/models/`, `workspace/onnxruntime/`, `workspace/db/cache/`,
`workspace/datasets/*/state/**` remain ignored (covered by `workspace/*` + the three `!` exceptions).
Verify with `git check-ignore`.

## D7 — README consolidation

Merge `data/README.md` (runtime artifacts, sha256) + `configs/README.md` (presets) into
`workspace/README.md` documenting the new layout, what is tracked vs ignored, and the dataset structure.

## D8 — Frozen-contract deviations (documented, user-authorized)

- **Config format:** removed `cache_path`, `global_config_path`, `documents_dir`; renamed
  `data_dir` → `workspace_dir`; added `dataset.name`. `database.path` now per-dataset-derived.
- **CLI surface:** removed `--db`; added `--dataset`.
- **Data-schema paths:** new `workspace/` directory layout (global vs per-dataset split).
These deviate from the oracle-transcribed frozen contracts; justified by the user's explicit
restructure decision (2026-08-28).

## Oracle references

- `../synopsis/data/ontology/global.xml` (source paths reference `data/demo/edtech/...` — mirrored pre-restructure)
- `../synopsis/data/storage/edtech/` (demo corpus, now `workspace/datasets/edtech/content/`)
- Oracle is read-only; layout is a Rust re-architecture, not a 1:1 copy.

## Risks

- Multi-crate refactor; intermediate states not runnable until D3 complete.
- Old config files referencing removed fields must fail with a clear error (add validation).
- `git mv` must preserve history; verify no duplicate/untracked leftovers.
- `ingestion` test fixture `data/items.json` is test-only (unrelated to product `data/`) — out of scope.
