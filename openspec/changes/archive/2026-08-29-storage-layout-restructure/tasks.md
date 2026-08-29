# Tasks: Storage layout restructure (data/ + configs/ → workspace/)

- [x] 1.1 Config model + presets + preset tests (commit a4b3c3d)
- [x] 1.2 Embedding/vectors/db path resolution (commit d80f939)
- [x] 1.3 CLI path resolution + flags (--dataset, drop --db) + no-data bootstrap (commit b78604e)
- [x] 1.4 File moves + global.xml + gitignore + README (commit 6999da5)
- [x] 1.5 Cross-crate test updates (incl. 1.3+1.4 reviewer nits) (commit 7a18047)
- [x] 1.7 Correct ontology XML to parser-expected structure + anchor source paths to global.xml dir (commit ee13c54)
- [x] 1.8 Fix demo preset config (vector_dim 384 + model_name gpt-oss-20b) (commit a0aea2a)
- [x] 1.9 Restructure migrations (knowledge/cache) + cache Db schema (only cache tables) (commit e028cb6)
- [x] 1.10 Linker cache -> llm_linker_cache (cache Db, request-keyed) + real test (commit 23235f3)
- [x] 1.6 Validation + smoke (re-run after 1.9 + 1.10) — fmt/clippy/test/build --release clean; serve boots, knowledge DB (knowledge schema) + cache DB (cache-only schema) created correctly, MCP listening :8080
- [x] 1.11 Decouple migrations from pool in Db::open_with (migrate on raw conn BEFORE pool; kill r2d2 "database is locked") (commit f18e40e)

## 1.1 Config model + presets + preset tests

**Goal:** Redefine the config path model; remove deprecated fields; add `DatasetConfig`.

**Scope файлов:**
- `crates/config/src/preset.rs` — `PathsConfig`: rename `data_dir`→`workspace_dir` (default `"workspace"`); DELETE `global_config_path`; DELETE `documents_dir`; keep `onnx_config` (default `"workspace/configs/onnx.yaml"`), `prompts_path` (default `"workspace/configs/prompts"`). `DatabaseConfig`: DELETE `cache_path`; DELETE `path` (knowledge-DB path is NO LONGER configurable — derived automatically from `workspace_dir` + `dataset.name`); ADD `cache_db_path()` on `PathsConfig` → `workspace_dir/db/cache/cache.db` (global). ADD `DatasetConfig { name: String }` (default `""` — NO dataset by default) with `ontology_path()/content_path()/state_path()/db_path(ws)/vectors_path()` helpers (all from `workspace_dir/datasets/<name>/`). `Config` gains `dataset: DatasetConfig`.
- `configs/config.default.yaml` — rename `data_dir`→`workspace_dir`; remove `cache_path`, `global_config_path`, `documents_dir`, AND `database.path` (DB path is derived, not configurable); update `prompts_path`/`onnx_config`; set `dataset: { name: "" }` (no dataset by default).
- `configs/config.demo.yaml` — same edits, but `dataset: { name: "edtech" }` (demo ingests the shipped corpus).
- `crates/config/src/preset.rs` (tests) — remove `global_config_path == "data/ontology"` and `documents_dir == "documents"` asserts; update `data_dir`→`workspace_dir`; add `DatasetConfig` default asserts.

**Dependencies:** none.

**Критерии приёмки:**
- `cargo test -p config` passes; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- No reference to removed fields remains in `preset.rs` struct/defaults/tests.
- `cache_db_path()` returns `<workspace_dir>/db/cache/cache.db`; `DatasetConfig::db_path()` returns `<workspace_dir>/datasets/edtech/state/db/knowledge.db`.
- `configs/config.default.yaml` + `config.demo.yaml` parse to the new shape (verified by `loads_demo_config` test + a default-load test).

**Oracle reference:** n/a (Rust config re-architecture; oracle `../synopsis/configs/*.yaml` is the behavioral source, not copied).

## 1.2 Path resolution in embedding / vectors / db crates

**Goal:** Use `workspace_dir` (global) for models/onnxruntime/cache and `DatasetConfig` (per-dataset) for db/vectors.

**Scope файлов:**
- `crates/embedding/src/lib.rs` + `crates/embedding/src/onnx_runtime.rs` — `LibraryManager::new(&config.paths.workspace_dir, &onnx)`, `new_onnx_provider(&config.embeddings.local, &config.paths.workspace_dir, &onnx)`; models resolve `<workspace_dir>/models/<name>/...`, runtime `<workspace_dir>/onnxruntime`.
- `crates/vectors/src/engine.rs` + `crates/vectors/src/sync.rs` — `LanceEngine::create(&dataset.vectors_path(), ...)` instead of `<data_dir>/vectors.lance`.
- `crates/db/src/lib.rs` + `crates/db/src/connection.rs` — knowledge DB via `dataset.db_path()`; cache via `config.paths.cache_db_path()` (global).

**Dependencies:** 1.1 (config model must exist).

**Критерии приёмки:**
- Each crate compiles (`cargo build -p embedding -p vectors -p db`); `cargo clippy --all-targets -- -D warnings` clean.
- No `data_dir` / `cache_path` / `global_config_path` references remain in these crates.
- Models/onnxruntime resolve from `workspace_dir` (global); vectors/db from `DatasetConfig` (per-dataset).

**Oracle reference:** n/a.

## 1.3 CLI path resolution + flags

**Goal:** Drop `--db`; add `--dataset`; wire per-dataset ontology/db paths in bootstrap + watcher.

**Scope файлов:**
- `crates/cli/src/cli.rs` — REMOVE `Arg "db"`; ADD `Arg "dataset"`; remove `db` from `SynopsisArgs`.
- `crates/cli/src/sync.rs` — remove `db_path: Option<PathBuf>`; pass `dataset` override instead.
- `crates/cli/src/serve/bootstrap.rs` — `bootstrap(cfg_path, dataset_override: Option<&str>)`; apply `dataset` override to `config.dataset.name`; drop `db_path` application; `discover_domains(&config.dataset.ontology_path())` instead of `global_config_path`; cache via `config.paths.cache_db_path()`.
- `crates/cli/src/serve/watcher.rs` — `load_global_config(&config.dataset.ontology_path())`; remove all `config.paths.global_config_path = ...` assignments (lines ~1100/1125/1138).
- `crates/cli/src/serve/server.rs` — update `data_dir`→`workspace_dir` references.

**Dependencies:** 1.1, 1.2.

**Критерии приёмки:**
- `cargo build -p cli` succeeds; `cargo clippy --all-targets -- -D warnings` clean.
- `--db` absent from `--help`; `--dataset` present and overrides `dataset.name`.
- No `global_config_path` / `db_path` references remain in cli crate.

**Oracle reference:** n/a.

## 1.4 File moves + global.xml + gitignore + README

**Goal:** Relocate all artifacts to `workspace/`; rewrite ontology sources; fix ignore + docs.

**Scope файлов:**
- Filesystem (TRACKED → `git mv`; gitignored → plain `mv`): `git mv configs/ workspace/configs/`; `git mv data/ontology workspace/datasets/edtech/ontology`; `git mv data/demo/edtech workspace/datasets/edtech/content`; `git mv data/README.md workspace/README.md`. Plain `mv data/models workspace/models`; `mv data/onnxruntime workspace/onnxruntime` (these are gitignored local artifacts). **DO NOT move Go-created DB artifacts** `data/state_store.db`, `data/stream_store`, `data/vectors.lance` — they are gitignored and Rust builds its own DB/vectors from scratch (hard rule); they are removed by `rm -rf data`. `mkdir -p workspace/db/cache`; `rm -rf data configs`.
- `workspace/datasets/edtech/ontology/global.xml` — rewrite all `<source path=>` `./data/demo/edtech/` → `./workspace/datasets/edtech/content/` (0 remaining `./data/demo/edtech/`).
- `.gitignore` — replace `data/*` (+`!data/README.md` + `!data/ontology/` + `!data/demo/`) with `workspace/*` + `!workspace/configs/` + `!workspace/datasets/edtech/ontology/` + `!workspace/datasets/edtech/content/`.
- `workspace/README.md` — merge `data/README.md` + `configs/README.md` content; document layout, tracked vs ignored, dataset structure.
- `crates/cli/src/config_resolver.rs` — replace every hardcoded `configs` directory segment with `workspace/configs` (lines ~46/51/64/73/138/176/187/213/240/248) so default/preset config resolution finds the moved presets. This is production code required by the move.
- `crates/config/src/preset.rs` (tests) — update `"/../../configs/config.{demo,default}.yaml"` test fixture paths to `"/../../workspace/configs/..."` (breakage caused directly by the move).

**Dependencies:** 1.3 (code must compile against new paths before moves; moves are pure relocation).

**Критерии приёмки:**
- `git status` shows `data/` and `configs/` gone; `workspace/` present with all subtrees.
- `global.xml` valid XML, all `./data/demo/edtech/` source paths rewritten to `./workspace/datasets/edtech/content/` (0 remaining `./data/demo/edtech/`).
- `git check-ignore`: `workspace/configs/onnx.yaml` NOT ignored; `workspace/models`, `workspace/onnxruntime`, `workspace/db/cache`, `workspace/datasets/edtech/state` ignored; `workspace/datasets/edtech/ontology/global.xml` + `workspace/datasets/edtech/content/...` NOT ignored.
- `workspace/README.md` documents layout.

**Oracle reference:** `../synopsis/data/ontology/global.xml` (source paths), `../synopsis/data/storage/edtech/` (corpus).

## 1.5 Cross-crate test updates

**Goal:** Fix remaining test assertions referencing old paths / flags / fields.

**Scope файлов:**
- `crates/config/src/preset.rs` (tests) — already handled in 1.1; verify no stale `data/ontology`/`documents` asserts remain.
- `crates/cli/tests/cli.rs` — `data_dir`→`workspace_dir`; remove `--db` flag tests; add `--dataset` tests.
- `crates/cli/src/serve/bootstrap.rs` (tests) — `data_dir`→`workspace_dir`; remove `global_config_path` refs; update cache/db path asserts. Also rename local test-helper params `data_dir`→`workspace_dir` (e.g. `fn local_config(data_dir: &Path)` ~line 747) per 1.3 reviewer nits.
- `crates/cli/src/serve/server.rs` (tests) — rename local test-helper param `data_dir`→`workspace_dir` (e.g. `fn test_config(data_dir: &Path)` ~line 791) per 1.3 reviewer nits.
- `crates/cli/src/serve/watcher.rs` (tests) — remove `global_config_path` refs; per-dataset ontology path.
- `crates/cli/src/loadtest/mod.rs` (tests) — rename local test-helper `data_dir`→`workspace_dir` (~line 243) per 1.3 reviewer nits.
- `crates/parity-harness/tests/parity_test.rs` — update `repo_root.join("configs/onnx.yaml")` → `workspace/configs/onnx.yaml` and `data/models/...` → `workspace/models/...` (build_provider must resolve the moved ONNX/model paths; otherwise the latency gate silently SKIPs). Per 1.4 findings.
- `workspace/configs/config.demo.yaml` (header comment ~line 4) and `workspace/configs/config.default.yaml` (header comment ~line 3) — update stale path references (`data/demo/edtech/`, `data/ontology/global.xml`, `data/README.md`, `--config configs/...`) to the new `workspace/...` layout. Per 1.4 reviewer nits (cosmetic, non-functional).
- `crates/parity-harness/**` — VERIFY no other `data/`/`configs/` path refs remain; if any, update.

**Dependencies:** 1.1–1.3.

**Критерии приёмки:**
- `cargo test --workspace` passes (0 failures).
- No `global_config_path` / `documents_dir` / `--db` / `data_dir` references remain in tests.
- If 1.5 diff exceeds ~500 lines, split per crate (1.5a cli tests, 1.5b watcher tests).

**Oracle reference:** n/a.

## 1.6 Validation + smoke

**Goal:** Full gate pass + runtime smoke of the new layout.

**Scope файлов:** none (verification only).

**Dependencies:** 1.1–1.5.

**Критерии приёмки:**
- `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` 0 failed; `cargo build --release` succeeds.
- Smoke: `./target/release/synopsis --version` works; `synopsis serve --config workspace/configs/config.default.yaml` (or default) loads config without path errors; `workspace/` tree matches the spec.

**Oracle reference:** n/a.

## 1.7 Correct ontology XML to parser-expected structure

**Goal:** Fix `workspace/datasets/edtech/ontology/*.xml` to the XML structure the Rust `crates/config/src/ontology.rs` parser expects (matching `crates/config/tests/data/` fixtures), so the demo preset boots. This is a CONTENT fix — the parser is correct; the shipped files were wrongly copied 1:1 from the Go oracle (which uses a bare structure the Rust parser rejects).

**Scope файлов:**
- `workspace/datasets/edtech/ontology/global.xml`
- `workspace/datasets/edtech/ontology/domains/domain_hr.xml`
- `workspace/datasets/edtech/ontology/domains/domain_it.xml`
- `workspace/datasets/edtech/ontology/domains/domain_product.xml`

**What is wrong:** the shipped files (copied byte-identical from `../synopsis/data/ontology/`) use a bare XML structure — `<method>` items directly under `<cross-domain-links>`/`<ner>` (no `<methods>` wrapper), and `<entity>`/`<relation>`/`<attribute>`/`<synonym>`/`<regex-rule>` without their `<entities>`/`<relations>`/`<attributes>`/`<synonyms>`/`<regex-rules>` parent wrappers. The Rust parser (verified by `crates/config/tests/data/global.xml` + `domains/*.xml`, which parse cleanly) expects those wrappers.

**Fix:** rewrite the shipped files to match the STRUCTURE of `crates/config/tests/data/` (add the missing wrapper elements, keep all semantic content: method/entity/relation/synonym/attribute/regex names). For `global.xml`, keep `<source path=>` as `./workspace/datasets/edtech/content/...` (do NOT copy the fixture's `./data/storage/edtech/...` paths — those are the oracle's old layout). For domain XMLs, align structure to the fixtures while preserving every entity/relation/synonym/attribute/regex entry.

**Dependencies:** 1.4 (files moved); surfaces in 1.6 smoke.

**Критерии приёмки:**
- `cargo test -p config` passes (ontology parser tests green against the corrected files).
- `synopsis serve --config workspace/configs/config.demo.yaml` boots, loads `workspace/datasets/edtech/ontology/global.xml` without "cross-domain-links.methods must have at least one method", and starts ingestion.
- `global.xml` still has 0 `./data/demo/edtech/` / `./data/storage/edtech/` references; all sources use `./workspace/datasets/edtech/content/`.
- XML of each file is well-formed (xmllint --noout clean).

**Oracle reference:** `../synopsis/data/ontology/` (content source ONLY — structure must follow the Rust parser / `crates/config/tests/data` fixtures, NOT copied 1:1).

## 1.8 Fix demo preset config (vector_dim + model_name)

**Goal:** Make `workspace/configs/config.demo.yaml` boot end-to-end with the local embedding model and the local LLM endpoint.

**Scope файлов:**
- `workspace/configs/config.demo.yaml` — `embeddings.local.vector_dim: 1024` → `384` (matches `model_name: bge-small-en-v1.5`, which is 384-dim; the oracle's `1024` was for bge-m3 and is rejected by the embedding provider). `ingestion.ner.llm.model_name: "openai/gpt-oss-20b"` → `"gpt-oss-20b"` and `linker.llm.model_name: "openai/gpt-oss-20b"` → `"gpt-oss-20b"` (drop the `openai/` prefix so the local OpenAI-compatible endpoint at `192.168.1.7:1234` resolves the model; the `openai/` prefix caused `HTTP 400: model not found`).

**Dependencies:** 1.7 (ontology loads); surfaces in 1.6 smoke.

**Критерии приёмки:**
- `cargo test --workspace` still passes (config-only change; no code).
- `synopsis serve --config workspace/configs/config.demo.yaml` no longer logs `vector dimension mismatch` and no longer logs `HTTP 400: model ... not found` for NER/linker LLM calls.
- No other config field changed.

**Oracle reference:** n/a (local-dev tuning; consistent with `config.default.yaml` local-dev override `477a272`).

## 1.9 Restructure migrations (knowledge/cache) + cache Db schema

**Goal:** Split migrations into nested `migrations/knowledge` and `migrations/cache` directories; the cache Db must contain ONLY cache tables (`llm_ner_cache`, `llm_linker_cache`, `app_kv`), not the knowledge schema. Fixes the defect where `open_cache` ran the full knowledge migration on the cache Db.

**Scope файлов:**
- `migrations/1-init/up.sql` → move to `migrations/knowledge/1-init/up.sql`. REMOVE the `app_kv` and `llm_ner_cache` table definitions from it (they are caches, not knowledge). Keep all knowledge tables (documents, chunks, chunks_fts*, entities, facts, entity_links, entity_sources, fact_sources, chunk_entities, FTS5 triggers/indexes, etc.).
- New `migrations/cache/1-init/up.sql`:
  ```sql
  CREATE TABLE llm_ner_cache (cache_key TEXT PRIMARY KEY, result TEXT NOT NULL);
  CREATE TABLE llm_linker_cache (cache_key TEXT PRIMARY KEY, decision TEXT NOT NULL);
  CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT, updated_at DATETIME DEFAULT CURRENT_TIMESTAMP);
  ```
  (`app_kv` stays in the cache Db — it holds `last_linking_run` and other generic KV markers; see 1.10.)
- `crates/db/src/connection.rs`: add `static KNOWLEDGE_MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../migrations/knowledge");` and `static CACHE_MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../migrations/cache");`. Refactor `apply_migrations` to take `&Migrations` (or add `apply_migrations_with(conn, &MIGRATIONS)`). Add `pub fn open_knowledge<P: AsRef<Path>>(path) -> Result<Self, DbError>` (applies KNOWLEDGE_MIGRATIONS) and `pub fn open_cache<P: AsRef<Path>>(path) -> Result<Self, DbError>` (applies CACHE_MIGRATIONS). Replace `Db::open` callers (bootstrap.rs:239 knowledge, :281 cache) with the specific constructors; keep `Db::open` only if still referenced (else remove it).
- `crates/db/src/test_util.rs`: `in_memory_db()` must apply KNOWLEDGE_MIGRATIONS (it is a knowledge Db in tests). Update it to call `Db::open_knowledge(":memory:")` or apply the knowledge set.
- `crates/cli/src/serve/bootstrap.rs`: line 239 `Db::open(path)` → `Db::open_knowledge(path)`; line 281 `Db::open(path)` → `Db::open_cache(path)`.

**Dependencies:** none.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all pass.
- A freshly created cache Db (`Db::open_cache`) contains exactly `llm_ner_cache`, `llm_linker_cache`, `app_kv` (+ `sqlite_master`/`sqlite_sequence`). A freshly created knowledge Db contains the knowledge schema WITHOUT `app_kv`/`llm_ner_cache`.
- `migrations/1-init/` is gone; `migrations/knowledge/1-init/up.sql` and `migrations/cache/1-init/up.sql` exist.
- NOTE (operator): the existing `workspace/db/cache/cache.db` is gitignored + regenerable; because it already has `user_version=1` (full schema), the new cache migration will be a no-op and the stale full schema remains. The user must delete `workspace/db/cache/cache.db` so it is recreated with the correct cache-only schema. Document this in the task report.

**Oracle reference:** n/a (Rust migration re-architecture; oracle `../synopsis/migrations/*.sql` is the behavioral source, not copied).

## 1.10 Linker cache -> llm_linker_cache (cache Db, request-keyed) + NER key fix + real test

**Goal:** Move linker DECISIONS out of `app_kv` into a dedicated `llm_linker_cache` table in the cache Db (global), keyed by the LLM request signature (prompts + model + params) — NOT by entity IDs or dataset. Fix the NER `build_cache_key` to drop the redundant `chunk_content` (the chunk is already rendered into `user_prompt`). `last_linking_run` stays in `app_kv` (cache Db). Make the linker cache actually work and prove it with a real test.

**Scope файлов:**
- `crates/graph/src/linker.rs`:
  - New `LlmLinkerCache` struct (analogous to `ingestion::ner::llm_cache::LlmNerCache`): table `llm_linker_cache`, `get(key)`/`set(key, &LinkDecision)`, `ensure_table` (`CREATE TABLE IF NOT EXISTS llm_linker_cache (cache_key TEXT PRIMARY KEY, decision TEXT NOT NULL)`). Bound to `ConnectionOrTx`.
  - `llm_cache_key` rebuilt from the LLM request signature: `sha256(model:temperature:max_tokens:rendered_system_prompt:rendered_user_prompt)` — NO entity IDs, NO dataset (global-safe, mirrors the NER key). Compute it from the rendered prompt inside `process_llm_pair` (refactor `decide_via_llm` to also return the rendered system/user prompts, or compute the key there).
  - `read_cached_decision` / `write_cached_decision` operate on `llm_linker_cache` via `LlmLinkerCache` (not `AppKv`).
  - `build_entity_links` / `process_llm_pair` receive the cache Db separately from the knowledge Db (`self.db`). The cache Db is used for BOTH `llm_linker_cache` decisions AND `app_kv` `last_linking_run`.
- `crates/ingestion/src/ner/llm_cache.rs`: `build_cache_key` drops the `chunk_content` parameter; key = `sha256(server:model:temperature:max_tokens:system_prompt:user_prompt)` (the chunk is already inside `user_prompt` — see `llm.rs:222-226` `render_user(..., normalized_content, ...)`). Update the doc comment.
- `crates/ingestion/src/ner/llm.rs:226`: call `build_cache_key(server, model, temperature, max_tokens, &system, &user)` (drop `normalized_content`). Update the key test helper (~line 650-654) accordingly.
- `crates/ingestion/src/ner/llm_cache.rs` tests: update `build_cache_key` call sites to the 6-arg form; the `cache_key_is_deterministic_and_input_sensitive` test must vary `user_prompt` (which embeds the chunk) instead of a separate `chunk_content` arg, and assert the key changes when `user_prompt` changes.
- `crates/ingestion/src/runner/cleanup.rs`: `build_entity_links_locked` passes `self.llm_cache` (cache Db, `Option<&Db>` or `as_ref()`) to `graph::build_entity_links`. `last_linking_run` read/write (lines 155, 217) use `app_kv` on the cache Db. If `llm_cache` is `None` (cache disabled), linker runs without cache and without the `last_linking_run` marker (nil-on-failure port) — non-fatal.
- `crates/ingestion/src/runner/mod.rs`: `Runner.llm_cache: Option<Db>` already exists; pass it to `build_entity_links`. (server.rs:381 / bootstrap.rs:537 already wire `llm_cache: cache` in production.)
- `crates/graph/tests/llm_linker_pipeline.rs`: rewrite the cache assertions into a REAL test — use a file-based cache Db (or two separate Db handles) SHARED across two `build_entity_links` calls where the SECOND call uses a FRESH knowledge Db but the SAME cache Db. Assert `server.request_count()` does NOT increase on the second run (cache hit avoids the LLM call); assert decisions live in `llm_linker_cache` (not `app_kv`). Also assert a changed prompt invalidates the cache (request_count increases).
- `crates/db/src/app_kv.rs` (`AppKv` DAO) is KEPT — used for `last_linking_run`. `app_kv` table stays in the cache migration (1.9).

**Dependencies:** 1.9.

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all pass.
- The rewritten linker test proves a cache hit avoids an LLM call (request_count stable across runs sharing the cache Db); a changed prompt triggers a re-call.
- Linker decisions stored in `llm_linker_cache`; `app_kv` still holds `last_linking_run`.
- `llm_cache_key` contains no entity IDs / dataset — only the LLM request signature.
- NER `build_cache_key` has no `chunk_content` param; key is sensitive to `user_prompt` (which embeds the chunk).
- No `app_kv` `llm_link_*` writes remain in the linker (grep `app_kv WHERE key LIKE 'llm_link%'` in tests returns 0).

**Oracle reference:** `../synopsis/internal/graph/linker.go` (decision cache behavior), `../synopsis/internal/database/dao/app_kv.go` (last_linking_run marker), `../synopsis/internal/ingestion/ner/llm_cache.go` (NER key format).

## 1.11 Decouple migrations from pool in Db::open_with

**Goal:** Eliminate the `ERROR r2d2: database is locked` startup error. Migrations must run on a SINGLE dedicated raw `rusqlite::Connection` BEFORE the r2d2 pool is built, so no pooled connection can race the migration's write lock. The application must not start serving until migrations are fully applied. Migrations must NOT be tied to pooled connections (current `Db::open_with` runs them on a pooled checkout, while the pool is being initialized — the race).

**Scope файлов:**
- `crates/db/src/connection.rs` — `open_with` ONLY. New sequence:
  1. create parent dirs (unchanged);
  2. open a raw `rusqlite::Connection` to `path` (`Connection::open`);
  3. `apply_pragmas(&conn)` then `apply_migrations(&mut conn, migrations)` on that raw connection (exclusive write lock, no other connection exists yet);
  4. drop the raw connection (releases the lock) — the migration state persists in the file (`PRAGMA user_version` + schema);
  5. build the r2d2 pool via `SqliteConnectionManager::file(path).with_init(apply_pragmas)` (migrations already done → no lock contention during pool init).
- `run_migrations` (pub(crate)) is KEPT — it is still used by `crates/db/src/test_util.rs::in_memory_db()` (the in-memory test helper, which is a separate path and does not exhibit the file-lock race). Do NOT remove it.

**Dependencies:** 1.9 (introduced `open_with` / `open_knowledge` / `open_cache`).

**Критерии приёмки:**
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all pass.
- `open_with` does NOT call `run_migrations` on a pooled connection; migrations run on the raw `Connection` opened before `Self::new(...)`.
- `Db::open_knowledge` / `Db::open_cache` still produce correct schemas (existing tests `open_creates_fresh_v5_schema`, `open_cache_creates_cache_only_schema`, `reopen_is_noop_migration_and_data_survives` stay green).
- `in_memory_db()` still works (its `run_migrations` path is untouched).
- Smoke `synopsis serve --config workspace/configs/config.default.yaml` shows NO `ERROR r2d2: database is locked` in logs; server still reaches `MCP server listening (Streamable HTTP) 0.0.0.0:8080`.
- No new dependencies; `unsafe_code=forbid` preserved.

**Oracle reference:** n/a (pure Rust connection/migration mechanism fix; the oracle's `database/sql` pool semantics are unrelated — see connection.rs module docs D1/D3/D8).

## Revisions

### 1.1 — user correction (2026-08-28, after rust-reviewer approve, before commit)

Two design corrections from the user, applied before committing 1.1:

1. **DB path must NOT be configurable.** The knowledge-DB path is derived automatically from
   `workspace_dir` + `dataset.name`; the `database.path` config field is removed entirely (no override).
   `db_path()` resolution lives on `DatasetConfig` (derived), not on `DatabaseConfig`.
2. **No default dataset.** `DatasetConfig.name` defaults to `""` (empty = "no data"). `edtech` is NOT
   hardcoded as a default. Bootstrap semantics (task 1.3): if `dataset.name` is empty OR
   `workspace/datasets/<name>/` does not exist → no dataset, skip ontology load + ingestion/indexing,
   run with no data (no error). Only `config.demo.yaml` sets `dataset.name: "edtech"` to ingest the demo.

These corrections are folded into task 1.1 (config model + presets) and design D1/D2.

### 1.7 — user correction (2026-08-28, after rust-reviewer approve, before commit)

The first 1.7 implementation set `global.xml` source paths to `./workspace/datasets/edtech/content/...`
(relative to cwd). User correction: a relative `<source path>` must be relative to the `global.xml`
FILE (the ontology directory), not the process cwd. Revision: (1) the 8 source paths in `global.xml`
became `../content/...`; (2) `crates/config/src/ontology.rs` `load_global_config(dir)` now anchors
relative `source.path` values to `dir` (the ontology dir) — single resolution point covering the
watcher and `discover_domains`/ingestion. The XML parser itself was NOT modified. Re-reviewed
(rust-reviewer approve) and committed as `ee13c54`.
