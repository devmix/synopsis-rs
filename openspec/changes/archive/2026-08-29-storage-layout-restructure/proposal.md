# Proposal: Storage layout restructure (data/ + configs/ → workspace/)

## Why

The current flat `data/` and `configs/` directories do not express the product's real
storage semantics: ontology, ingested content, the knowledge DB, and the ANN index are all
**bound to a dataset** (e.g. `edtech`), while ONNX models, the ONNX runtime, and the LLM/
embedding cache are **global**. A single flat `data/` mixes these concerns and prevents
multi-dataset organization. The user wants a `workspace/` root that separates global artifacts
(`configs`, `models`, `onnxruntime`, `db/cache`) from per-dataset artifacts
(`datasets/<name>/{ontology,content,state}`).

## What Changes

- Introduce `workspace/` as the single storage root. `configs/`, `models/`, `onnxruntime/`
  move under it (global). `data/ontology`, `data/demo/edtech`, `data/state_store.db`,
  `data/vectors.lance` move under `workspace/datasets/edtech/{ontology,content,state}`.
- Config model: `PathsConfig.data_dir` → `workspace_dir`; remove `global_config_path`,
  `documents_dir`; add `DatasetConfig { name }` with per-dataset path helpers. `DatabaseConfig`
  drops `cache_path`; the cache DB is derived at `workspace/db/cache/cache.db` (global).
- CLI: remove `--db`; add `--dataset` (overrides `dataset.name`).
- `data/` and `configs/` are deleted.

## Non-goals

- No schema migration of any DB — Rust builds its own DB/vectors from scratch; file moves are
  relocations only.
- No new MCP tools or capability specs; this is a storage/contract refactor.
- No multi-dataset UI/selection beyond the single shipped `edtech` dataset (layout allows more).

## Decision (user-approved 2026-08-28)

- Q1: dataset name via `dataset.name: edtech` + optional `--dataset` CLI override.
- Q2: cache DB exists in code (`open_cache` at bootstrap); `cache_path` removed, cache → `workspace/db/cache`.
- Q3: `configs/` moved wholesale to `workspace/configs/`.
- Q4: gitignore tracks `workspace/configs/**`, `workspace/datasets/edtech/ontology/**`,
  `workspace/datasets/edtech/content/**`; ignores `workspace/models/`, `workspace/onnxruntime/`,
  `workspace/db/cache/`, `workspace/datasets/*/state/**`.
- Q5: only one dataset (`edtech`) shipped.
- EXTRA: remove `--db`; delete `global_config_path`; remove unused `documents_dir`; rename
  `data_dir` → `workspace_dir`.
- CORRECTION: models and onnxruntime are GLOBAL (resolved from `workspace_dir`), NOT per-dataset —
  `DatasetConfig` has only `ontology_path/content_path/state_path/db_path/vectors_path`.
