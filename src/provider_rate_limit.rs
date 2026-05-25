//! Adaptive provider pressure control and thin provider adapters.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

use crate::ai::{self, AICompletionRequest, AIConfig, AIProvider, AIResponse, CostTier, Message};
use crate::embeddings::EmbeddingProvider;

const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);
const DEFAULT_SLOW_LATENCY: Duration = Duration::from_secs(45);
const MIN_ERROR_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct ProviderPressureState {
    next_allowed: Instant,
    current_interval: Duration,
    success_streak: u32,
}

/// Adaptive provider pressure limiter.
///
/// Existing RPM settings are treated as an initial floor, not as the control
/// plane. Runtime pressure signals increase the interval; healthy responses
/// gradually recover toward the initial interval.
#[derive(Debug)]
pub struct ProviderPressureLimiter {
    key: String,
    base_interval: Duration,
    max_interval: Duration,
    slow_latency: Duration,
    state: Mutex<ProviderPressureState>,
}

impl ProviderPressureLimiter {
    pub fn new(key: impl Into<String>, requests_per_minute: Option<u32>) -> Self {
        Self::with_settings(
            key,
            interval_from_rpm(requests_per_minute),
            DEFAULT_MAX_BACKOFF,
            DEFAULT_SLOW_LATENCY,
        )
    }

    fn with_settings(
        key: impl Into<String>,
        base_interval: Duration,
        max_interval: Duration,
        slow_latency: Duration,
    ) -> Self {
        Self {
            key: key.into(),
            base_interval,
            max_interval: max_interval.max(base_interval),
            slow_latency,
            state: Mutex::new(ProviderPressureState {
                next_allowed: Instant::now(),
                current_interval: base_interval,
                success_streak: 0,
            }),
        }
    }

    pub async fn acquire(&self) {
        loop {
            let sleep_for = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                if state.next_allowed <= now {
                    state.next_allowed = now + state.current_interval;
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

    pub async fn record_success(&self, latency: Duration) {
        let mut state = self.state.lock().await;
        if latency >= self.slow_latency {
            state.current_interval = grow_interval(
                state.current_interval,
                self.base_interval,
                self.max_interval,
                1.25,
            );
            state.success_streak = 0;
            crate::metrics::gauge(
                "provider_pressure_interval_seconds",
                state.current_interval.as_secs_f64(),
                &[("provider", self.key.as_str())],
            );
            return;
        }

        state.success_streak = state.success_streak.saturating_add(1);
        if state.success_streak >= 3 && state.current_interval > self.base_interval {
            state.current_interval = shrink_interval(state.current_interval, self.base_interval);
            state.success_streak = 0;
            crate::metrics::gauge(
                "provider_pressure_interval_seconds",
                state.current_interval.as_secs_f64(),
                &[("provider", self.key.as_str())],
            );
        }
    }

    pub async fn record_failure(&self, error: &anyhow::Error) {
        let message = error.to_string();
        if !is_provider_pressure_error(&message) {
            return;
        }

        let requested_delay = retry_after_delay(&message);
        let mut state = self.state.lock().await;
        let grown = grow_interval(
            state.current_interval,
            self.base_interval.max(MIN_ERROR_BACKOFF),
            self.max_interval,
            2.0,
        );
        let minimum_delay = self.base_interval.max(MIN_ERROR_BACKOFF);
        state.current_interval = requested_delay
            .unwrap_or(grown)
            .clamp(minimum_delay, self.max_interval.max(minimum_delay));
        state.next_allowed = Instant::now() + state.current_interval;
        state.success_streak = 0;
        crate::metrics::counter(
            "provider_pressure_backoff_total",
            1,
            &[("provider", self.key.as_str())],
        );
        crate::metrics::gauge(
            "provider_pressure_interval_seconds",
            state.current_interval.as_secs_f64(),
            &[("provider", self.key.as_str())],
        );
    }

    async fn record_outcome(&self, latency: Duration, error: Option<&anyhow::Error>) {
        if let Some(error) = error {
            self.record_failure(error).await;
        } else {
            self.record_success(latency).await;
        }
    }
}

fn interval_from_rpm(requests_per_minute: Option<u32>) -> Duration {
    match requests_per_minute {
        Some(rpm) if rpm > 0 => Duration::from_secs_f64(60.0 / rpm as f64),
        _ => Duration::ZERO,
    }
}

fn grow_interval(
    current: Duration,
    minimum: Duration,
    maximum: Duration,
    multiplier: f64,
) -> Duration {
    let floor = minimum.max(MIN_ERROR_BACKOFF);
    let current = current.max(floor);
    Duration::from_secs_f64((current.as_secs_f64() * multiplier).min(maximum.as_secs_f64()))
}

fn shrink_interval(current: Duration, base: Duration) -> Duration {
    if current <= base {
        return base;
    }
    Duration::from_secs_f64((current.as_secs_f64() * 0.85).max(base.as_secs_f64()))
}

fn is_provider_pressure_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("retry-after")
        || lower.contains("503")
        || lower.contains("service unavailable")
        || lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("high demand")
        || lower.contains("timed out")
        || lower.contains("timeout")
}

fn retry_after_delay(message: &str) -> Option<Duration> {
    let lower = message.to_ascii_lowercase();
    let index = lower.find("retry-after")?;
    let rest = &lower[index + "retry-after".len()..];
    let digits: String = rest
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    let secs = digits.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

static PROVIDER_LIMITERS: OnceLock<StdMutex<HashMap<String, Arc<ProviderPressureLimiter>>>> =
    OnceLock::new();

fn shared_limiter(key: String, requests_per_minute: Option<u32>) -> Arc<ProviderPressureLimiter> {
    let registry = PROVIDER_LIMITERS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry.lock().expect("provider limiter registry poisoned");
    registry
        .entry(key.clone())
        .or_insert_with(|| Arc::new(ProviderPressureLimiter::new(key, requests_per_minute)))
        .clone()
}

fn provider_pressure_key(provider_name: impl Into<String>) -> String {
    format!("provider:{}", provider_name.into().to_ascii_lowercase())
}

pub struct PressureLimitedAIProvider {
    inner: Arc<dyn AIProvider>,
    limiter: Arc<ProviderPressureLimiter>,
}

impl PressureLimitedAIProvider {
    pub fn new(
        inner: Arc<dyn AIProvider>,
        provider_name: impl Into<String>,
        requests_per_minute: Option<u32>,
    ) -> Self {
        let limiter = shared_limiter(provider_pressure_key(provider_name), requests_per_minute);
        Self { inner, limiter }
    }

    async fn wait_for_slot(&self) {
        self.limiter.acquire().await;
    }
}

#[async_trait::async_trait]
impl AIProvider for PressureLimitedAIProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.wait_for_slot().await;
        let started = Instant::now();
        let result = self.inner.complete(messages).await;
        self.limiter
            .record_outcome(started.elapsed(), result.as_ref().err())
            .await;
        result
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        self.wait_for_slot().await;
        let started = Instant::now();
        let result = self.inner.complete_with_request(request).await;
        self.limiter
            .record_outcome(started.elapsed(), result.as_ref().err())
            .await;
        result
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

pub struct PressureLimitedEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    limiter: Arc<ProviderPressureLimiter>,
}

impl PressureLimitedEmbeddingProvider {
    pub fn new(
        inner: Arc<dyn EmbeddingProvider>,
        provider_name: impl Into<String>,
        requests_per_minute: Option<u32>,
    ) -> Self {
        let limiter = shared_limiter(provider_pressure_key(provider_name), requests_per_minute);
        Self { inner, limiter }
    }

    async fn wait_for_slot(&self) {
        self.limiter.acquire().await;
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for PressureLimitedEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.wait_for_slot().await;
        let started = Instant::now();
        let result = self.inner.embed(text).await;
        self.limiter
            .record_outcome(started.elapsed(), result.as_ref().err())
            .await;
        result
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
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

pub fn wrap_ai_provider_with_pressure(
    provider: Arc<dyn AIProvider>,
    provider_name: &str,
    requests_per_minute: Option<u32>,
) -> Arc<dyn AIProvider> {
    Arc::new(PressureLimitedAIProvider::new(
        provider,
        provider_name,
        requests_per_minute,
    ))
}

pub fn wrap_embedding_provider_with_pressure(
    provider: Arc<dyn EmbeddingProvider>,
    provider_name: &str,
    requests_per_minute: Option<u32>,
) -> Arc<dyn EmbeddingProvider> {
    Arc::new(PressureLimitedEmbeddingProvider::new(
        provider,
        provider_name,
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
    wrap_ai_provider_with_pressure(
        provider,
        provider_name,
        config.rate_limit_for_provider(provider_name),
    )
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
    async fn provider_pressure_limiter_enforces_initial_interval() {
        let limiter = ProviderPressureLimiter::with_settings(
            "test",
            Duration::from_millis(500),
            Duration::from_secs(5),
            Duration::from_secs(45),
        );
        let started = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(450));
    }

    #[tokio::test]
    async fn provider_pressure_limiter_backs_off_on_pressure_error() {
        let limiter = ProviderPressureLimiter::with_settings(
            "test",
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::from_secs(45),
        );

        limiter
            .record_failure(&anyhow::anyhow!("429 Too Many Requests"))
            .await;

        let started = Instant::now();
        limiter.acquire().await;
        assert!(started.elapsed() >= Duration::from_millis(900));
    }
}
