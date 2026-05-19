//! Async rate limiting utilities and provider wrappers.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

use crate::ai::{self, AICompletionRequest, AIConfig, AIProvider, AIResponse, CostTier, Message};
use crate::embeddings::EmbeddingProvider;

#[derive(Debug)]
struct RateLimiterState {
    next_allowed: Instant,
}

/// Simple interval limiter (requests/minute).
/// It enforces a minimum delay between requests and is safe for concurrent use.
#[derive(Debug)]
pub struct RateLimiter {
    min_interval: Duration,
    state: Mutex<RateLimiterState>,
}

impl RateLimiter {
    pub fn new(requests_per_minute: u32) -> Option<Self> {
        if requests_per_minute == 0 {
            return None;
        }
        let interval_secs = 60.0 / requests_per_minute as f64;
        Some(Self {
            min_interval: Duration::from_secs_f64(interval_secs),
            state: Mutex::new(RateLimiterState {
                next_allowed: Instant::now(),
            }),
        })
    }

    pub async fn acquire(&self) {
        loop {
            let sleep_for = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                if state.next_allowed <= now {
                    state.next_allowed = now + self.min_interval;
                    None
                } else {
                    Some(state.next_allowed - now)
                }
            };
            if let Some(delay) = sleep_for {
                sleep(delay).await;
                continue;
            }
            break;
        }
    }
}

pub struct RateLimitedAIProvider {
    inner: Arc<dyn AIProvider>,
    limiter: Option<Arc<RateLimiter>>,
}

impl RateLimitedAIProvider {
    pub fn new(inner: Arc<dyn AIProvider>, requests_per_minute: Option<u32>) -> Self {
        let limiter = requests_per_minute.and_then(RateLimiter::new).map(Arc::new);
        Self { inner, limiter }
    }

    async fn wait_for_slot(&self) {
        if let Some(limiter) = &self.limiter {
            limiter.acquire().await;
        }
    }
}

#[async_trait::async_trait]
impl AIProvider for RateLimitedAIProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.wait_for_slot().await;
        self.inner.complete(messages).await
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        self.wait_for_slot().await;
        self.inner.complete_with_request(request).await
    }

    fn supports_structured_output(&self) -> bool {
        self.inner.supports_structured_output()
    }
    fn supports_thinking(&self) -> bool {
        self.inner.supports_thinking()
    }
    fn supports_tool_calling(&self) -> bool {
        self.inner.supports_tool_calling()
    }
    fn supports_confidence_scores(&self) -> bool {
        self.inner.supports_confidence_scores()
    }
    fn max_context_tokens(&self) -> usize {
        self.inner.max_context_tokens()
    }
    fn is_local(&self) -> bool {
        self.inner.is_local()
    }
    fn cost_tier(&self) -> CostTier {
        self.inner.cost_tier()
    }
}

pub struct RateLimitedEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    limiter: Option<Arc<RateLimiter>>,
}

impl RateLimitedEmbeddingProvider {
    pub fn new(inner: Arc<dyn EmbeddingProvider>, requests_per_minute: Option<u32>) -> Self {
        let limiter = requests_per_minute.and_then(RateLimiter::new).map(Arc::new);
        Self { inner, limiter }
    }

    async fn wait_for_slot(&self) {
        if let Some(limiter) = &self.limiter {
            limiter.acquire().await;
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for RateLimitedEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.wait_for_slot().await;
        self.inner.embed(text).await
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }
}

pub fn wrap_ai_provider(
    provider: Arc<dyn AIProvider>,
    requests_per_minute: Option<u32>,
) -> Arc<dyn AIProvider> {
    if requests_per_minute.unwrap_or(0) == 0 {
        return provider;
    }
    Arc::new(RateLimitedAIProvider::new(provider, requests_per_minute))
}

pub fn wrap_embedding_provider(
    provider: Arc<dyn EmbeddingProvider>,
    requests_per_minute: Option<u32>,
) -> Arc<dyn EmbeddingProvider> {
    if requests_per_minute.unwrap_or(0) == 0 {
        return provider;
    }
    Arc::new(RateLimitedEmbeddingProvider::new(
        provider,
        requests_per_minute,
    ))
}

pub fn effective_frontier_provider_name(config: &AIConfig) -> String {
    let provider = config.provider.to_lowercase();
    if provider == "hybrid" {
        config
            .frontier_provider
            .clone()
            .unwrap_or_else(|| "gemini".to_string())
            .to_lowercase()
    } else {
        provider
    }
}

pub fn is_local_provider_name(provider: &str) -> bool {
    matches!(
        provider.to_lowercase().as_str(),
        "lmstudio" | "local" | "ollama" | "omlx"
    )
}

pub fn wrap_configured_ai_provider(
    config: &AIConfig,
    provider_name: &str,
    provider: Arc<dyn AIProvider>,
) -> Arc<dyn AIProvider> {
    if provider.is_local() {
        return provider;
    }
    wrap_ai_provider(provider, config.rate_limit_for_provider(provider_name))
}

pub fn build_local_ai_provider(config: &AIConfig) -> Option<Arc<dyn AIProvider>> {
    let is_local_primary = is_local_provider_name(&config.provider);
    if !config.local_llm_enabled && !is_local_primary {
        return None;
    }

    let mut local_config = config.clone();
    local_config.provider = "lmstudio".to_string();
    ai::create_provider(&local_config)
        .ok()
        .map(Arc::from)
        .map(|provider: Arc<dyn AIProvider>| {
            wrap_configured_ai_provider(&local_config, "lmstudio", provider)
        })
}

pub fn build_frontier_ai_provider(config: &AIConfig) -> Result<Option<Arc<dyn AIProvider>>> {
    let mut frontier_config = config.clone();
    if frontier_config.provider.eq_ignore_ascii_case("hybrid") {
        frontier_config.provider = effective_frontier_provider_name(&frontier_config);
    }

    match ai::create_provider(&frontier_config) {
        Ok(provider) => {
            let provider: Arc<dyn AIProvider> = Arc::from(provider);
            let provider_name = effective_frontier_provider_name(&frontier_config);
            Ok(Some(wrap_configured_ai_provider(
                &frontier_config,
                &provider_name,
                provider,
            )))
        }
        Err(error) => {
            let local_available =
                config.local_llm_enabled || is_local_provider_name(&config.provider);
            if is_local_provider_name(&frontier_config.provider) || local_available {
                Ok(None)
            } else {
                Err(error).context("create frontier provider")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limiter_enforces_interval() {
        let limiter = RateLimiter::new(120).expect("limiter");
        let started = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        let elapsed = started.elapsed();
        // 120 RPM = 0.5s between requests; allow scheduler jitter.
        assert!(elapsed >= Duration::from_millis(450));
    }
}
