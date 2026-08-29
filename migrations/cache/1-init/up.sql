-- Migration 1-init: the cache database schema (task 1.9,
-- storage-layout-restructure). The cache database holds ONLY cache tables
-- — never the knowledge schema (which lives in `migrations/knowledge`).
--
-- `app_kv` was moved here from the knowledge migration: it holds generic
-- KV markers (`last_linking_run` and the LLM linker decision cache), not
-- knowledge data. `llm_ner_cache`/`llm_linker_cache` are the LLM response
-- caches (the NER one is also created lazily at runtime — design D6 — but
-- its canonical home is this schema).

CREATE TABLE llm_ner_cache (cache_key TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE llm_linker_cache (cache_key TEXT PRIMARY KEY, decision TEXT NOT NULL);
CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT, updated_at DATETIME DEFAULT CURRENT_TIMESTAMP);
