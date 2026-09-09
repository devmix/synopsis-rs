//! Startup health check (design D4).
//!
//! Log-only by contract: the caller treats a returned error as a warning,
//! never fatal. The database ping is the single hard probe (an error is
//! returned); the document count and the embedding probe are logged inline.
//!
//! Design decision: the probe logs the provider instance already built by
//! the bootstrap (name + live dimension) instead of building a fresh one —
//! a second ONNX session for a probe would be pure waste. The dimension is
//! not compared against the config: the provider and the ANN index both
//! derive it from the same `onnx.yaml` registry entry
//! (registry-as-model-source-of-truth D5), so there is nothing left to
//! check here.

use config::Config;
use db::{ConnectionOrTx, Db, DocumentDao, DocumentFilter};
use embedding::EmbeddingProvider;

use crate::error::CliError;

/// Runs the startup health check: database ping, document count, embedding
/// provider probe.
///
/// Every finding is logged (`tracing`); nothing here is fatal.
///
/// # Errors
///
/// [`CliError::Db`] when the database ping fails (the caller logs a warning
/// and continues).
pub fn run_health_check(
    db: &Db,
    embed: &dyn EmbeddingProvider,
    config: &Config,
) -> Result<(), CliError> {
    tracing::info!("startup health check started");

    // 1. Database connectivity.
    match db.with_conn(|conn| conn.query_row("SELECT 1", [], |row| row.get::<_, i32>(0))) {
        Ok(_) => tracing::info!(
            component = "database",
            status = "ok",
            "startup health check"
        ),
        Err(err) => {
            tracing::error!(
                component = "database",
                status = "fail",
                error = %err,
                "startup health check"
            );
            return Err(CliError::Db(err));
        }
    }

    // 2. Check for existing data.
    let count = db.with_conn(|conn| {
        DocumentDao::new(ConnectionOrTx::Connection(conn)).count(&DocumentFilter::default())
    });
    match count {
        Ok(Ok(count)) => {
            tracing::info!(
                component = "documents",
                status = "ok",
                count,
                "startup health check"
            );
        }
        Ok(Err(err)) => {
            tracing::warn!(
                component = "document_count",
                error = %err,
                "startup health check"
            );
        }
        Err(err) => {
            tracing::warn!(
                component = "document_count",
                error = %err,
                "startup health check"
            );
        }
    }

    // 3. Embedding provider probe: log the live instance built by the
    //    bootstrap (a bootstrap failure is already fatal upstream). No
    //    dimension comparison: the provider and the ANN index both derive
    //    their dimension from the same onnx.yaml registry entry
    //    (registry-as-model-source-of-truth D5).
    tracing::info!(
        component = "embedding_provider",
        status = "ok",
        provider = embed.name(),
        model = %config.embeddings.local.model_name,
        vector_dim = embed.vector_dim(),
        "startup health check"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use config::preset::{Config, EmbeddingsMode, LocalEmbedding};
    use db::test_util::in_memory_db;
    use embedding::EmbeddingError;

    use super::*;

    /// A deterministic in-test provider (no ONNX Runtime).
    struct ConstProvider {
        dim: usize,
        name: &'static str,
    }

    impl EmbeddingProvider for ConstProvider {
        fn generate_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(vec![vec![1.0; self.dim]; texts.len()])
        }

        fn vector_dim(&self) -> usize {
            self.dim
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn local_config() -> Config {
        Config {
            embeddings: config::preset::EmbeddingsConfig {
                mode: EmbeddingsMode::Local,
                local: LocalEmbedding {
                    model_name: "test".to_string(),
                },
                api: Default::default(),
                auto_rebuild_vectors: false,
            },
            ..Default::default()
        }
    }

    #[test]
    fn health_check_passes_on_fresh_db() {
        let db = in_memory_db();
        let provider = Arc::new(ConstProvider {
            dim: 1024,
            name: "const",
        });
        let config = local_config();

        run_health_check(&db, provider.as_ref(), &config)
            .expect("health check passes on a fresh migrated db");
    }

    #[test]
    fn health_check_reports_document_count() {
        let db = in_memory_db();
        // Seed one document so the count is observable.
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO documents (source_type, original_path) VALUES ('markdown', '/a.md')",
                [],
            )
        })
        .expect("insert")
        .expect("insert row");

        let provider = Arc::new(ConstProvider {
            dim: 4,
            name: "const",
        });
        let config = local_config();
        run_health_check(&db, provider.as_ref(), &config)
            .expect("health check passes with documents present");
    }

    #[test]
    fn health_check_succeeds_when_provider_dim_differs_from_config() {
        let db = in_memory_db();
        let provider = Arc::new(ConstProvider {
            dim: 384,
            name: "const",
        });
        let config = local_config();

        run_health_check(&db, provider.as_ref(), &config)
            .expect("the embedding probe is log-only: a differing dimension is not a failure");
    }
}
