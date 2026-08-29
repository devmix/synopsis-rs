//! Persistent LLM-NER response cache (ingestion-ner design D6).
//!
//! Oracle references: `../synopsis/internal/ingestion/ner/llm_cache.go`
//! (`BuildCacheKey`, `LLMCache`) on top of the generic SQLite key-value store
//! `../synopsis/internal/cache/store.go`.
//!
//! Re-architected for the db crate's conventions (task 2.4) rather than
//! transcribed: the oracle's `*cache.Store` (its own `*sql.DB` pool plus a
//! table-name registry) collapses into the db crate's unified executor
//! surface — the cache is bound to a [`ConnectionOrTx`] exactly like the db
//! crate's DAOs (cf. `db::AppKv`), so it works over a pooled connection or
//! inside an in-flight pipeline transaction. Caching is disabled by the
//! caller simply not constructing a cache (`Option<LlmNerCache>` in the LLM
//! provider, task 2.5) — the oracle's nil-store no-op.
//!
//! The table `llm_ner_cache (cache_key TEXT PRIMARY KEY, result TEXT NOT
//! NULL)` is created lazily with `CREATE TABLE IF NOT EXISTS` on first use —
//! the frozen v5 migration shape stays untouched (the table is absent from
//! the oracle's own migrations too, design D6).
//!
//! Deliberate deviations from the oracle (recorded):
//!
//! - **Database errors propagate; they are not misses.** The oracle's store
//!   treats ANY driver error as a cache miss; the db crate convention (cf.
//!   `app_kv.rs`) propagates them instead — a broken database must not look
//!   empty (design D10: cache/DB failures are fatal for the extraction
//!   call). Corrupted JSON is still a miss (oracle behavior).
//! - **No table-name memoization.** The oracle memoizes created tables
//!   behind a mutex (`seen` map); here the idempotent `CREATE TABLE IF NOT
//!   EXISTS` simply runs on every call. The table persists in the database
//!   file and the DDL costs microseconds — YAGNI.
//! - **Fixed table name.** The oracle allows a caller-chosen table name
//!   (default `llm_ner_cache`); only this cache exists, so the name is a
//!   constant (YAGNI).
//! - **No context parameter.** Go's `ctx.Err()` checks are a runtime concern,
//!   not part of the data-flow contract (design D2).
//!
//! The cache key format is **internal-only** (design D6): it is never
//! compared across implementations, so no cross-language byte parity is a
//! contract.

use db::{ConnectionOrTx, DbExecutor};
use sha2::{Digest, Sha256};

use super::NerResult;
use crate::error::IngestionError;

/// The lazily-created cache table (design D6): deliberately absent from the
/// migration schema, created at runtime on first use.
const CACHE_TABLE: &str = "llm_ner_cache";

/// Persistent LLM-NER response cache over the `llm_ner_cache` table
/// (design D6).
///
/// One instance per unit of work, bound to either a pooled connection or an
/// in-flight transaction via [`ConnectionOrTx`] — the same shape as the db
/// crate's DAOs (cf. `db::AppKv`). The instance borrows from the handle it
/// is given, so the handle must outlive it.
///
/// Entries map a cache key (see [`build_cache_key`]) to a serialized
/// [`NerResult`].
pub struct LlmNerCache<'conn> {
    exec: ConnectionOrTx<'conn>,
}

impl<'conn> LlmNerCache<'conn> {
    /// Bind the cache to a shared connection or an in-flight transaction.
    #[must_use]
    pub fn new(exec: ConnectionOrTx<'conn>) -> Self {
        Self { exec }
    }

    /// Return the cached extraction result for `key`, or `None` on a miss.
    ///
    /// A miss is: no row, or a row whose JSON payload does not deserialize
    /// into [`NerResult`] (corrupted entry — oracle behavior; the next
    /// [`set`](Self::set) overwrites it). Database failures are NOT misses:
    /// they propagate as [`IngestionError::Db`] (db convention, see the
    /// module docs).
    pub fn get(&self, key: &str) -> Result<Option<NerResult>, IngestionError> {
        self.ensure_table()?;
        let rows: Vec<String> = self.exec.query(
            &format!("SELECT result FROM {CACHE_TABLE} WHERE cache_key = ?"),
            [key],
            |row| row.get(0),
        )?;
        // The primary key guarantees at most one row: an empty result set is
        // a plain miss (avoids the QueryReturnedNoRows error-identity dance
        // the DAOs inside the db crate do with their own error type).
        let Some(json) = rows.into_iter().next() else {
            return Ok(None);
        };
        match serde_json::from_str(&json) {
            Ok(result) => Ok(Some(result)),
            Err(_) => Ok(None),
        }
    }

    /// Store `result` under `key`, replacing any existing entry (oracle
    /// `INSERT OR REPLACE` semantics).
    pub fn set(&self, key: &str, result: &NerResult) -> Result<(), IngestionError> {
        let json = serde_json::to_string(result)
            .map_err(|source| IngestionError::NerCacheJson { source })?;
        self.ensure_table()?;
        self.exec.execute(
            &format!("INSERT OR REPLACE INTO {CACHE_TABLE} (cache_key, result) VALUES (?, ?)"),
            (key, json),
        )?;
        Ok(())
    }

    /// Create the cache table if it does not exist yet (design D6, lazy
    /// runtime creation — the table is deliberately NOT in the migrations).
    fn ensure_table(&self) -> Result<(), IngestionError> {
        self.exec.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {CACHE_TABLE} \
                 (cache_key TEXT PRIMARY KEY, result TEXT NOT NULL)"
            ),
            [],
        )?;
        Ok(())
    }
}

/// Builds the SHA-256 cache key from the LLM call parameters and prompts.
///
/// Key format: `sha256(server:model:temperature:max_tokens:system_prompt:
/// user_prompt)` — the `:`-joined parts. The chunk content is NOT a separate
/// key part: it is already rendered into `user_prompt` (the provider renders
/// the normalized chunk into the user template before the call), so a
/// distinct chunk always yields a distinct `user_prompt` and thus a distinct
/// key.
///
/// **The key format is internal-only** (design D6): keys are never compared
/// across implementations, so no cross-language byte parity is a contract.
/// The temperature uses Rust's shortest round-trip float representation
/// (e.g. `0.5` → `"0.5"`, `0.0` → `"0"`, `1.0` → `"1"`), which agrees with
/// the oracle's Go `%g` for the temperature range a config can express.
#[must_use]
pub fn build_cache_key(
    server: &str,
    model: &str,
    temperature: f64,
    max_tokens: i32,
    system_prompt: &str,
    user_prompt: &str,
) -> String {
    let raw = format!("{server}:{model}:{temperature}:{max_tokens}:{system_prompt}:{user_prompt}");
    let digest = Sha256::digest(raw.as_bytes());
    to_hex(&digest)
}

/// Lowercase hex encoding (same helper shape as `embedding::cache::to_hex`).
fn to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use db::test_util::in_memory_db;
    use db::{ConnectionOrTx, Db};
    use serde_json::{Map, Value};
    use sha2::{Digest, Sha256};

    use super::super::{NerEntity, NerFact};
    use super::*;

    /// A representative extraction result: one entity (with metadata) and
    /// one fact.
    fn sample_result() -> NerResult {
        let mut metadata = Map::new();
        metadata.insert("rule_id".to_string(), Value::String("r1".to_string()));
        NerResult {
            entities: vec![NerEntity {
                name: "Acme Corp".to_string(),
                entity_type: "company".to_string(),
                description: "A sample company.".to_string(),
                confidence: 0.9,
                domain: "crm".to_string(),
                metadata,
            }],
            facts: vec![NerFact {
                subject_type: "employee".to_string(),
                subject_name: "Alice".to_string(),
                predicate: "works_at".to_string(),
                object_type: "company".to_string(),
                object_name: "Acme Corp".to_string(),
                domain: "crm".to_string(),
                metadata: Map::new(),
            }],
        }
    }

    /// Run `f` with a cache bound to a pooled connection (checked out for
    /// the duration of the closure).
    fn with_cache<T>(db: &Db, f: impl FnOnce(&LlmNerCache<'_>) -> T) -> T {
        db.with_conn(|conn| f(&LlmNerCache::new(ConnectionOrTx::Connection(conn))))
            .unwrap()
    }

    /// Hex sha256 of `s` — the expected key for a known joined string.
    fn sha256_hex(s: &str) -> String {
        to_hex(&Sha256::digest(s.as_bytes()))
    }

    // roundtrip: set then get returns an equal copy; a fresh table misses.
    #[test]
    fn set_then_get_round_trip() {
        let db = in_memory_db();
        let key = build_cache_key("http://llm.local", "bge", 0.5, 2048, "sys", "usr");
        let result = sample_result();
        with_cache(&db, |cache| {
            assert_eq!(cache.get(&key).unwrap(), None, "fresh table misses");
            cache.set(&key, &result).unwrap();
            assert_eq!(cache.get(&key).unwrap(), Some(result));
        });
    }

    // get a missing key → None.
    #[test]
    fn get_missing_key_is_none() {
        let db = in_memory_db();
        with_cache(&db, |cache| {
            assert_eq!(cache.get("absent").unwrap(), None);
        });
    }

    // set overwrites an existing entry (INSERT OR REPLACE semantics).
    #[test]
    fn set_replaces_existing_entry() {
        let db = in_memory_db();
        with_cache(&db, |cache| {
            cache.set("k", &sample_result()).unwrap();
            let empty = NerResult::default();
            cache.set("k", &empty).unwrap();
            assert_eq!(cache.get("k").unwrap(), Some(empty));
        });
    }

    // corrupted or wrong-shaped JSON payload → miss, not an error
    // (oracle behavior).
    #[test]
    fn corrupted_entry_is_a_miss() {
        let db = in_memory_db();
        with_cache(&db, |cache| {
            cache.set("k", &sample_result()).unwrap();
        });

        // Corrupt the stored payload out-of-band (hand-edit / torn write).
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE llm_ner_cache SET result = 'not json' WHERE cache_key = ?",
                ["k"],
            )
        })
        .unwrap()
        .unwrap();
        with_cache(&db, |cache| {
            assert_eq!(
                cache.get("k").unwrap(),
                None,
                "corrupted JSON must be a miss"
            );
        });

        // Valid JSON with the wrong shape is a miss too.
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE llm_ner_cache SET result = '{\"bogus\": 1}' WHERE cache_key = ?",
                ["k"],
            )
        })
        .unwrap()
        .unwrap();
        with_cache(&db, |cache| {
            assert_eq!(
                cache.get("k").unwrap(),
                None,
                "wrong-shaped JSON must be a miss"
            );
        });
    }

    // the key is deterministic and every part participates in it.
    #[test]
    fn cache_key_is_deterministic_and_input_sensitive() {
        let base = build_cache_key("http://llm.local", "bge", 0.7, 2048, "sys", "usr");
        assert_eq!(base.len(), 64, "hex sha256 is 64 chars");
        assert_eq!(
            base,
            build_cache_key("http://llm.local", "bge", 0.7, 2048, "sys", "usr"),
            "same inputs must give the same key"
        );
        assert_ne!(
            base,
            build_cache_key("http://other.local", "bge", 0.7, 2048, "sys", "usr")
        );
        assert_ne!(
            base,
            build_cache_key("http://llm.local", "other-model", 0.7, 2048, "sys", "usr")
        );
        assert_ne!(
            base,
            build_cache_key("http://llm.local", "bge", 0.8, 2048, "sys", "usr")
        );
        assert_ne!(
            base,
            build_cache_key("http://llm.local", "bge", 0.7, 4096, "sys", "usr")
        );
        assert_ne!(
            base,
            build_cache_key("http://llm.local", "bge", 0.7, 2048, "sys2", "usr")
        );
        // The chunk content is embedded in `user_prompt` (rendered upstream),
        // so a different chunk surfaces as a different `user_prompt`.
        assert_ne!(
            base,
            build_cache_key("http://llm.local", "bge", 0.7, 2048, "sys", "usr2")
        );
        assert_ne!(
            base,
            build_cache_key(
                "http://llm.local",
                "bge",
                0.7,
                2048,
                "sys",
                "usr with a different chunk rendered in"
            )
        );
    }

    // temperature is formatted like Go's %g: shortest representation without
    // trailing zeros (0.5 → "0.5", 0.0 → "0", 1.0 → "1").
    #[test]
    fn key_uses_g_like_temperature_formatting() {
        let args = ("server", "model", 2048, "sys", "user");
        let key = |raw: &str| sha256_hex(raw);

        assert_eq!(
            build_cache_key(args.0, args.1, 0.5, args.2, args.3, args.4),
            key("server:model:0.5:2048:sys:user")
        );
        assert_eq!(
            build_cache_key(args.0, args.1, 0.0, args.2, args.3, args.4),
            key("server:model:0:2048:sys:user"),
            "0.0 must format as \"0\""
        );
        assert_eq!(
            build_cache_key(args.0, args.1, 1.0, args.2, args.3, args.4),
            key("server:model:1:2048:sys:user"),
            "1.0 must format as \"1\""
        );
        assert_eq!(
            build_cache_key(args.0, args.1, 0.7, args.2, args.3, args.4),
            key("server:model:0.7:2048:sys:user")
        );
    }

    // the table is NOT in the migrations: it appears only after the first
    // cache use (design D6, lazy runtime creation).
    #[test]
    fn table_is_created_lazily_on_first_use() {
        let db = in_memory_db();
        let table_exists = || -> i64 {
            db.with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name = 'llm_ner_cache'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap()
        };

        assert_eq!(
            table_exists(),
            0,
            "no table before first use (not in migrations)"
        );
        with_cache(&db, |cache| {
            assert!(cache.get("absent").unwrap().is_none());
        });
        assert_eq!(table_exists(), 1, "the first get created the table");
    }

    // the cache works bound to an in-flight transaction (the pipeline path,
    // task 2.5): a committed set is visible afterwards.
    #[test]
    fn works_inside_a_transaction() {
        let db = in_memory_db();
        let result = sample_result();
        db.exec_tx(|tx| {
            let cache = LlmNerCache::new(ConnectionOrTx::Transaction(&*tx));
            cache.set("k", &result)
        })
        .expect("commit");
        with_cache(&db, |cache| {
            assert_eq!(cache.get("k").unwrap(), Some(result));
        });
    }
}
