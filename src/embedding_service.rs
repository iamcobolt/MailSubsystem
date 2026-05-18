//! Embedding generation for semantic search (RAG).
//! Supports Gemini and OpenAI-compatible (omlx, LM Studio, Ollama) embedding models.
//! Embedding dimensions are auto-detected from the model at startup.

use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use std::sync::OnceLock;

/// Provider that embeds text into vectors.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text. Returns a vector whose length matches `dimensions()`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// The embedding dimension this provider produces (probed at creation).
    fn dimensions(&self) -> usize;

    /// The model identifier (e.g. "gemini-embedding-001", "snowflake-arctic-embed-l-v2.0-bf16").
    fn model_name(&self) -> &str;

    /// Embed multiple texts in one batch (optional optimization).
    /// Default implementation calls embed() sequentially.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// Gemini embedding provider.
pub struct GeminiEmbeddingProvider {
    api_key: String,
    model: String,
    dims: usize,
    client: reqwest::Client,
}

impl GeminiEmbeddingProvider {
    pub fn with_model(api_key: String, model: String, dims: usize) -> Self {
        Self {
            api_key,
            model,
            dims,
            client: reqwest::Client::new(),
        }
    }

    /// Probe the model to discover its output dimension.
    async fn probe(api_key: &str, model: &str) -> Result<usize> {
        let model_name = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{}", model)
        };
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{}:embedContent?key={}",
            model_name, api_key
        );
        let body = serde_json::json!({
            "model": model_name,
            "content": { "parts": [{ "text": "dimension probe" }] }
        });
        let client = reqwest::Client::new();
        let res = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Gemini embed probe request")?;
        let status = res.status();
        let text_resp = res.text().await.context("Gemini embed probe response")?;
        if !status.is_success() {
            anyhow::bail!("Gemini embed probe error {}: {}", status, text_resp);
        }
        let v: serde_json::Value =
            serde_json::from_str(&text_resp).context("Parse Gemini embed probe response")?;
        let values = v
            .get("embedding")
            .or_else(|| v.get("embeddings").and_then(|e| e.get(0)))
            .and_then(|e| e.get("values"))
            .and_then(|v| v.as_array())
            .context("Missing embedding.values in probe response")?;
        Ok(values.len())
    }
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model_name = if self.model.starts_with("models/") {
            self.model.clone()
        } else {
            format!("models/{}", self.model)
        };
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{}:embedContent?key={}",
            model_name, self.api_key
        );
        let body = serde_json::json!({
            "model": model_name,
            "content": {
                "parts": [{ "text": text }]
            },
            "outputDimensionality": self.dims
        });
        let res = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Gemini embed API request")?;
        let status = res.status();
        let text_resp = res.text().await.context("Gemini embed response body")?;
        if !status.is_success() {
            anyhow::bail!("Gemini embed API error {}: {}", status, text_resp);
        }
        let v: serde_json::Value =
            serde_json::from_str(&text_resp).context("Parse Gemini embed response")?;
        let values = v
            .get("embedding")
            .or_else(|| v.get("embeddings").and_then(|e| e.get(0)))
            .and_then(|e| e.get("values"))
            .and_then(|v| v.as_array())
            .context("Missing embedding.values in response")?;
        let vec: Vec<f32> = values
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect();
        if vec.len() != self.dims {
            anyhow::bail!(
                "Expected embedding dimension {}, got {}",
                self.dims,
                vec.len()
            );
        }
        Ok(vec)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// OpenAI-compatible embedding provider (omlx, LM Studio, Ollama, etc.).
/// Hits POST {base_url}/embeddings with the standard OpenAI embeddings request shape.
pub struct OpenAICompatibleEmbeddingProvider {
    base_url: String,
    model: String,
    dims: usize,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAICompatibleEmbeddingProvider {
    pub fn new(base_url: String, model: String, dims: usize, api_key: Option<String>) -> Self {
        let base_url = base_url
            .trim_end_matches('/')
            .strip_suffix("/embeddings")
            .unwrap_or(base_url.trim_end_matches('/'))
            .to_string();
        Self {
            base_url,
            model,
            dims,
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Probe the model to discover its output dimension.
    async fn probe(base_url: &str, model: &str, api_key: Option<&str>) -> Result<usize> {
        let base_url = base_url
            .trim_end_matches('/')
            .strip_suffix("/embeddings")
            .unwrap_or(base_url.trim_end_matches('/'));
        let url = format!("{}/embeddings", base_url);
        let body = serde_json::json!({
            "model": model,
            "input": "dimension probe",
        });
        let client = reqwest::Client::new();
        let mut request = client.post(&url).json(&body);
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
        }
        let res = request.send().await.context("Embed probe request")?;
        let status = res.status();
        let text_resp = res.text().await.context("Embed probe response")?;
        if !status.is_success() {
            anyhow::bail!("Embed probe error {}: {}", status, text_resp);
        }
        let v: serde_json::Value =
            serde_json::from_str(&text_resp).context("Parse embed probe response")?;
        let values = v
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|e| e.get("embedding"))
            .and_then(|v| v.as_array())
            .context("Missing data[0].embedding in probe response")?;
        Ok(values.len())
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAICompatibleEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });
        let mut request = self.client.post(&url).json(&body);
        if let Some(api_key) = self.api_key.as_deref() {
            request = request.bearer_auth(api_key);
        }
        let res = request
            .send()
            .await
            .context("OpenAI-compatible embed API request")?;
        let status = res.status();
        let text_resp = res.text().await.context("Embed response body")?;
        if !status.is_success() {
            anyhow::bail!("Embed API error {}: {}", status, text_resp);
        }
        let v: serde_json::Value =
            serde_json::from_str(&text_resp).context("Parse embed response")?;
        let values = v
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|e| e.get("embedding"))
            .and_then(|v| v.as_array())
            .context("Missing data[0].embedding in response")?;
        let vec: Vec<f32> = values
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect();
        if vec.len() != self.dims {
            anyhow::bail!(
                "Expected embedding dimension {}, got {}",
                self.dims,
                vec.len()
            );
        }
        Ok(vec)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProviderChoice {
    Auto,
    Local,
    Gemini,
}

impl EmbeddingProviderChoice {
    fn from_env() -> Result<Self> {
        match std::env::var("EMBEDDING_PROVIDER") {
            Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
                "" | "auto" => Ok(Self::Auto),
                "local" | "openai-compatible" | "openai_compatible" | "lmstudio" | "ollama" => {
                    Ok(Self::Local)
                }
                "gemini" | "frontier" => Ok(Self::Gemini),
                other => anyhow::bail!(
                    "Invalid EMBEDDING_PROVIDER '{}'. Expected auto, local, or gemini.",
                    other
                ),
            },
            Err(_) => Ok(Self::Auto),
        }
    }
}

fn has_gemini_embedding_config() -> bool {
    std::env::var("GEMINI_API_KEY").is_ok() || std::env::var("EMBEDDING_API_KEY").is_ok()
}

fn should_try_local_embedding(choice: EmbeddingProviderChoice) -> bool {
    match choice {
        EmbeddingProviderChoice::Local => true,
        EmbeddingProviderChoice::Gemini => false,
        EmbeddingProviderChoice::Auto => {
            if std::env::var("EMBEDDING_MODEL").is_ok() {
                return true;
            }
            std::env::var("LOCAL_LLM_URL").is_ok()
                && !has_gemini_embedding_config()
                && std::env::var("EMBEDDING_GEMINI_MODEL").is_err()
        }
    }
}

fn should_try_gemini_embedding(choice: EmbeddingProviderChoice) -> bool {
    match choice {
        EmbeddingProviderChoice::Gemini => true,
        EmbeddingProviderChoice::Local => false,
        EmbeddingProviderChoice::Auto => has_gemini_embedding_config(),
    }
}

async fn create_local_embedding_provider() -> Result<Box<dyn EmbeddingProvider>> {
    let base_url =
        std::env::var("LOCAL_LLM_URL").context("LOCAL_LLM_URL required for local embeddings")?;
    let model =
        std::env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "nomic-embed-text-v1.5".to_string());
    let api_key = std::env::var("LOCAL_LLM_API_KEY").ok();
    log::info!(
        "Probing embedding model '{}' at {} for dimensions...",
        model,
        base_url
    );
    let dims = OpenAICompatibleEmbeddingProvider::probe(&base_url, &model, api_key.as_deref())
        .await
        .with_context(|| {
            format!(
                "Failed to probe local embedding model dimensions for '{}'. \
                 Set EMBEDDING_MODEL to an installed embedding model, or set \
                 EMBEDDING_PROVIDER=gemini to use frontier embeddings.",
                model
            )
        })?;
    log::info!(
        "Embedding model '{}' produces {}-dimensional vectors",
        model,
        dims
    );
    Ok(Box::new(OpenAICompatibleEmbeddingProvider::new(
        base_url, model, dims, api_key,
    )))
}

async fn create_gemini_embedding_provider() -> Result<Box<dyn EmbeddingProvider>> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("EMBEDDING_API_KEY"))
        .context("GEMINI_API_KEY or EMBEDDING_API_KEY required for embeddings")?;
    let model = std::env::var("EMBEDDING_GEMINI_MODEL")
        .unwrap_or_else(|_| "gemini-embedding-001".to_string());
    log::info!(
        "Probing Gemini embedding model '{}' for dimensions...",
        model
    );
    let dims = GeminiEmbeddingProvider::probe(&api_key, &model)
        .await
        .context("Failed to probe Gemini embedding model dimensions")?;
    log::info!(
        "Embedding model '{}' produces {}-dimensional vectors",
        model,
        dims
    );
    Ok(Box::new(GeminiEmbeddingProvider::with_model(
        api_key, model, dims,
    )))
}

/// Auto-detect embedding provider from environment configuration.
/// Probes the model to discover its output dimension.
/// In auto mode, EMBEDDING_MODEL selects local OpenAI-compatible embeddings;
/// otherwise Gemini/frontier embeddings win when an embedding API key is present.
/// Returns an error if no embedding provider can be configured.
pub async fn create_embedding_provider() -> Result<Box<dyn EmbeddingProvider>> {
    let choice = EmbeddingProviderChoice::from_env()?;

    if should_try_local_embedding(choice) {
        return create_local_embedding_provider().await;
    }
    if should_try_gemini_embedding(choice) {
        return create_gemini_embedding_provider().await;
    }

    if choice == EmbeddingProviderChoice::Auto && std::env::var("LOCAL_LLM_URL").is_ok() {
        return create_local_embedding_provider().await;
    }

    anyhow::bail!(
        "No embedding provider configured. Set EMBEDDING_PROVIDER=local with \
         LOCAL_LLM_URL and EMBEDDING_MODEL, or set EMBEDDING_PROVIDER=gemini with \
         GEMINI_API_KEY / EMBEDDING_API_KEY."
    )
}

/// Validate that the configured embedding model matches what is stored in the DB.
/// - First run (no metadata): stores current model + dims, returns Ok.
/// - Match: returns Ok.
/// - Mismatch: rebuilds embedding storage by default, or fails when
///   EMBEDDING_MODEL_MISMATCH=fail.
pub async fn ensure_embedding_model(
    db: &crate::db::Database,
    provider: &dyn EmbeddingProvider,
) -> Result<()> {
    let stored_model = db.get_system_metadata("embedding_model").await?;
    let stored_dims = db.get_system_metadata("embedding_dimensions").await?;

    let current_model = provider.model_name();
    let current_dims = provider.dimensions().to_string();

    match (stored_model, stored_dims) {
        (None, _) | (_, None) => {
            log::info!(
                "Recording embedding model: {} ({}d)",
                current_model,
                current_dims
            );
            db.set_system_metadata("embedding_model", current_model)
                .await?;
            db.set_system_metadata("embedding_dimensions", &current_dims)
                .await?;
            Ok(())
        }
        (Some(ref sm), Some(ref sd)) if sm == current_model && *sd == current_dims => {
            log::debug!("Embedding model unchanged: {} ({}d)", sm, sd);
            Ok(())
        }
        (Some(sm), Some(sd)) => {
            let mode =
                std::env::var("EMBEDDING_MODEL_MISMATCH").unwrap_or_else(|_| "rebuild".to_string());
            if mode.eq_ignore_ascii_case("fail") || mode.eq_ignore_ascii_case("manual") {
                anyhow::bail!(
                    "Embedding model mismatch!\n\
                     Stored:     {} ({}d)\n\
                     Configured: {} ({}d)\n\n\
                     Existing embeddings are incompatible with the new model.\n\
                     Run `mailsubsystem embed-rebuild` to re-embed all emails, \
                     or set EMBEDDING_MODEL_MISMATCH=rebuild for automatic reset.",
                    sm,
                    sd,
                    current_model,
                    current_dims
                );
            }

            log::warn!(
                "Embedding model changed from {} ({}d) to {} ({}d); nulling existing embeddings and rebuilding the index",
                sm,
                sd,
                current_model,
                current_dims
            );
            let nulled = db.null_all_embeddings().await?;
            db.rebuild_embedding_index(provider.dimensions()).await?;
            db.set_system_metadata("embedding_model", current_model)
                .await?;
            db.set_system_metadata("embedding_dimensions", &current_dims)
                .await?;
            log::warn!(
                "Embedding model reset complete; {} existing embedding(s) will be regenerated by embed-backfill/core embed work",
                nulled
            );
            Ok(())
        }
    }
}

/// Backwards-compatible name for callers/tests that still expect validation.
pub async fn validate_embedding_model(
    db: &crate::db::Database,
    provider: &dyn EmbeddingProvider,
) -> Result<()> {
    ensure_embedding_model(db, provider).await
}

/// Truncate text to fit embedding model input limit (e.g. 2048 tokens ~ 8k chars).
/// Truncates by character count to avoid splitting multi-byte UTF-8 sequences.
pub fn truncate_for_embedding(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let s: String = text.chars().take(max_chars).collect();
    format!("{}...", s)
}

/// Normalize body text before embedding:
/// - remove script/style and HTML tags when present
/// - decode common HTML entities
/// - collapse whitespace
/// - truncate to a bounded size
pub fn clean_email_body_text(text: &str, max_chars: usize) -> String {
    static SCRIPT_STYLE_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    static WS_RE: OnceLock<Regex> = OnceLock::new();

    let script_style_re = SCRIPT_STYLE_RE.get_or_init(|| {
        Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>")
            .expect("valid script/style regex")
    });
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("valid HTML tag regex"));
    let ws_re = WS_RE.get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"));

    let mut out = text.replace('\0', " ");
    out = script_style_re.replace_all(&out, " ").to_string();
    out = tag_re.replace_all(&out, " ").to_string();
    out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out = ws_re.replace_all(&out, " ").to_string();
    let out = out.trim();
    if out.is_empty() {
        return String::new();
    }
    truncate_for_embedding(out, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_email_body_text_strips_html_and_normalizes_spaces() {
        let raw = "<html><body><h1>Hello&nbsp;World</h1><script>alert(1)</script><p>Line 2</p></body></html>";
        let out = clean_email_body_text(raw, 10_000);
        assert_eq!(out, "Hello World Line 2");
    }

    #[test]
    fn clean_email_body_text_truncates() {
        let out = clean_email_body_text("a b c d e f g", 5);
        assert!(out.chars().count() <= 8);
        assert!(out.ends_with("..."));
    }
}
