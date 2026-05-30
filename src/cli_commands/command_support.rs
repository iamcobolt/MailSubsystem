use std::sync::Arc;

use crate::{ai, db, embeddings, rag, rate_limit};

/// Default path for env file, loaded at startup.
pub const DEFAULT_ENV_PATH: &str = ".env";

pub fn load_agent_specs_dir() -> String {
    std::env::var("AGENTS_DIR").unwrap_or_else(|_| "./specs/agents".to_string())
}

/// Create RAG context builder with embeddings provider.
/// Probes the embedding model to discover its dimensions. Errors if no provider is configured.
pub async fn create_rag_builder(
    database: Arc<db::Database>,
    ai_config: Option<&ai::AIConfig>,
) -> anyhow::Result<Arc<rag::RAGContextBuilder>> {
    let embedder = embeddings::create_embedding_provider().await?;
    embeddings::validate_embedding_model(&database, embedder.as_ref()).await?;
    let embedding_provider_name = embedder.provider_name().to_string();
    let rpm = ai_config.and_then(|cfg| {
        if embedder.is_local() {
            None
        } else {
            cfg.rate_limit_for_provider(&embedding_provider_name)
        }
    });
    let wrapped = rate_limit::wrap_embedding_provider_with_pressure(
        Arc::from(embedder),
        &embedding_provider_name,
        rpm,
    );
    Ok(Arc::new(rag::RAGContextBuilder::with_embedder(
        database, wrapped,
    )))
}
