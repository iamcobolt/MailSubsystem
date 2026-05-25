//! AI provider abstraction and hybrid routing (local vs frontier).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rig::completion::{
    AssistantContent as RigAssistantContent, CompletionModel as RigCompletionModel,
    CompletionRequest as RigCompletionRequest, CompletionResponse as RigCompletionResponse,
    Message as RigMessage, ToolDefinition as RigToolDefinition, Usage as RigUsage,
};
use rig::message::ToolChoice as RigToolChoice;
use rig::prelude::CompletionClient as _;
use rig::OneOrMany as RigOneOrMany;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

/// Chat message for provider API.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }
}

/// Tool calling behavior for a completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

/// Tool schema for provider-native function/tool calling APIs.
#[derive(Debug, Clone)]
pub struct CompletionTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Provider-agnostic completion request payload.
#[derive(Debug, Clone)]
pub struct AICompletionRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<CompletionTool>,
    pub tool_choice: ToolChoice,
    pub temperature: f32,
    pub max_tokens: Option<u32>,
}

impl AICompletionRequest {
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            temperature: 0.2,
            max_tokens: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<CompletionTool>, tool_choice: ToolChoice) -> Self {
        self.tools = tools;
        self.tool_choice = tool_choice;
        self
    }
}

/// Tool call from model (function calling).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Token usage from a provider response (for frontier cost tracking).
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> Option<u32> {
        match (self.input_tokens, self.output_tokens) {
            (Some(i), Some(o)) => Some(i + o),
            (Some(i), None) | (None, Some(i)) => Some(i),
            (None, None) => None,
        }
    }
}

/// Response from AI provider.
#[derive(Debug, Clone)]
pub struct AIResponse {
    pub content: String,
    pub confidence: Option<f32>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: String,
    /// Token counts when available (Gemini, OpenAI, Anthropic).
    pub usage: Option<TokenUsage>,
}

/// Cost tier for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostTier {
    Free,
    Cheap,
    Expensive,
}

/// AI provider trait (OpenAI-compatible, Gemini, Anthropic, LM Studio).
#[async_trait::async_trait]
pub trait AIProvider: Send + Sync {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse>;
    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        self.complete(request.messages).await
    }
    fn supports_structured_output(&self) -> bool {
        true
    }
    fn supports_thinking(&self) -> bool {
        false
    }
    fn supports_tool_calling(&self) -> bool {
        false
    }
    fn supports_confidence_scores(&self) -> bool {
        false
    }
    fn max_context_tokens(&self) -> usize {
        128_000
    }
    fn is_local(&self) -> bool {
        false
    }
    fn cost_tier(&self) -> CostTier {
        CostTier::Cheap
    }
}

/// Full analysis result (matches DB columns and ai_summary).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnalysisResult {
    pub spam_status: Option<String>,
    pub phishing_status: Option<String>,
    pub marketing_status: Option<String>,
    pub otp_status: Option<String>,
    pub otp_code: Option<String>,
    #[serde(default)]
    pub otp_expires: Option<DateTime<Utc>>,
    pub threat_level: Option<String>,
    pub threat_indicators: Option<Vec<String>>,
    pub ai_summary: Option<Value>,
    pub human_summary: Option<String>,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub organization: Option<String>,
    pub topic: Option<String>,
    pub email_type: Option<String>,
    pub location_recommendation: Option<String>,
    #[serde(default)]
    pub offer_expires: Option<DateTime<Utc>>,
    /// Minimum confidence across fields (for local model escalation).
    #[serde(skip)]
    pub confidence: Option<f32>,
    /// Which provider produced this result: "local" or "frontier" (for hybrid visibility).
    #[serde(skip)]
    pub analyzed_by: Option<String>,
    /// Token usage from frontier call, when available.
    #[serde(skip)]
    pub token_usage: Option<TokenUsage>,
    /// When true, local result was saved and message_id was enqueued for frontier analysis (do not call frontier now).
    #[serde(skip)]
    pub queued_for_frontier: bool,
}

/// RAG tool definition for iterative analysis.
#[derive(Debug, Clone)]
pub struct RAGTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl From<RAGTool> for CompletionTool {
    fn from(value: RAGTool) -> Self {
        Self {
            name: value.name,
            description: value.description,
            parameters: value.parameters,
        }
    }
}

/// Location agent tools: list subfolders, folder emails summary, full folder tree.
#[cfg(test)]
pub fn location_tools() -> Vec<RAGTool> {
    vec![
        RAGTool {
            name: "list_subfolders".to_string(),
            description: "List immediate child folders of a given path".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Folder path (e.g. Personal/Banking); use empty string for top-level"}
                }
            }),
        },
        RAGTool {
            name: "get_folder_emails_summary".to_string(),
            description: "Get count and top organizations/categories of emails in a folder"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "limit": {"type": "integer", "description": "Max orgs/categories to return (default 10)"}
                }
            }),
        },
        RAGTool {
            name: "get_folder_tree".to_string(),
            description: "Get the full folder tree with NOSELECT tags".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        RAGTool {
            name: "get_folder_email_samples".to_string(),
            description:
                "Sample recent emails from a folder to inspect senders/categories before consolidation"
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "folder_path": {"type": "string", "description": "Full folder path (e.g. Receipts/2023)"},
                    "limit": {"type": "integer", "description": "Max emails to sample (default 5, max 10)"}
                },
                "required": ["folder_path"]
            }),
        },
    ]
}

/// AI configuration from environment.
#[derive(Clone)]
pub struct AIConfig {
    pub provider: String,
    pub local_llm_enabled: bool,
    pub local_llm_url: Option<String>,
    pub local_llm_model: Option<String>,
    pub local_llm_api_key: Option<String>,
    pub frontier_provider: Option<String>,
    pub frontier_model: Option<String>,
    pub gemini_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub analysis_mode: String,
    pub max_iterations: usize,
    /// Global API requests per minute cap for frontier providers/embeddings.
    pub api_rate_limit_rpm: u32,
    /// Optional provider-specific overrides.
    pub gemini_rate_limit_rpm: Option<u32>,
    pub openai_rate_limit_rpm: Option<u32>,
    pub anthropic_rate_limit_rpm: Option<u32>,
    pub spend_safety: crate::spend_safety::SpendSafetyConfig,
}

fn redacted_optional_secret(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "<redacted>")
}

impl fmt::Debug for AIConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local_llm_api_key = redacted_optional_secret(&self.local_llm_api_key);
        let gemini_api_key = redacted_optional_secret(&self.gemini_api_key);
        let openai_api_key = redacted_optional_secret(&self.openai_api_key);
        let anthropic_api_key = redacted_optional_secret(&self.anthropic_api_key);

        f.debug_struct("AIConfig")
            .field("provider", &self.provider)
            .field("local_llm_enabled", &self.local_llm_enabled)
            .field("local_llm_url", &self.local_llm_url)
            .field("local_llm_model", &self.local_llm_model)
            .field("local_llm_api_key", &local_llm_api_key)
            .field("frontier_provider", &self.frontier_provider)
            .field("frontier_model", &self.frontier_model)
            .field("gemini_api_key", &gemini_api_key)
            .field("openai_api_key", &openai_api_key)
            .field("anthropic_api_key", &anthropic_api_key)
            .field("analysis_mode", &self.analysis_mode)
            .field("max_iterations", &self.max_iterations)
            .field("api_rate_limit_rpm", &self.api_rate_limit_rpm)
            .field("gemini_rate_limit_rpm", &self.gemini_rate_limit_rpm)
            .field("openai_rate_limit_rpm", &self.openai_rate_limit_rpm)
            .field("anthropic_rate_limit_rpm", &self.anthropic_rate_limit_rpm)
            .field("spend_safety", &self.spend_safety)
            .finish()
    }
}

impl Default for AIConfig {
    fn default() -> Self {
        Self {
            provider: "hybrid".to_string(),
            local_llm_enabled: true,
            local_llm_url: Some("http://localhost:1234/v1".to_string()),
            local_llm_model: Some("local-model".to_string()),
            local_llm_api_key: None,
            frontier_provider: Some("gemini".to_string()),
            frontier_model: Some("gemini-2.0-flash-exp".to_string()),
            gemini_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            analysis_mode: "standard".to_string(),
            max_iterations: 5,
            api_rate_limit_rpm: 60,
            gemini_rate_limit_rpm: None,
            openai_rate_limit_rpm: None,
            anthropic_rate_limit_rpm: None,
            spend_safety: crate::spend_safety::SpendSafetyConfig::default(),
        }
    }
}

impl AIConfig {
    /// Load config from environment only (.env).
    pub fn load() -> Result<Self> {
        let _ = dotenvy::from_path(".env");
        Self::load_from_env()
    }

    /// Detect provider from available configuration when AI_PROVIDER is not set.
    /// Priority: if LOCAL_LLM_URL is set and any API key is also set → hybrid;
    /// if only LOCAL_LLM_URL → local; if only one API key → that provider.
    /// Detect provider from available configuration when AI_PROVIDER is not set.
    fn detect_provider() -> Result<String> {
        let has_local = std::env::var("LOCAL_LLM_URL").is_ok();
        let has_gemini = std::env::var("GEMINI_API_KEY").is_ok();
        let has_openai = std::env::var("OPENAI_API_KEY").is_ok();
        let has_anthropic = std::env::var("ANTHROPIC_API_KEY").is_ok();
        let has_codex = env_flag("CODEX_ENABLED");
        let has_any_frontier = has_gemini || has_openai || has_anthropic;

        match (has_local, has_any_frontier, has_codex) {
            (true, true, _) => Ok("hybrid".to_string()),
            (true, false, _) => Ok("local".to_string()),
            (false, true, _) => {
                if has_gemini {
                    Ok("gemini".to_string())
                } else if has_openai {
                    Ok("openai".to_string())
                } else {
                    Ok("anthropic".to_string())
                }
            }
            (false, false, true) => Ok("codex".to_string()),
            (false, false, false) => {
                anyhow::bail!(
                    "No AI provider configured. Set LOCAL_LLM_URL for a local model, \
                     set an API key (GEMINI_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY), \
                     or set AI_PROVIDER=codex after logging in with `codex login`."
                );
            }
        }
    }

    fn load_from_env() -> Result<Self> {
        let provider = match std::env::var("AI_PROVIDER") {
            Ok(p) => p,
            Err(_) => Self::detect_provider()?,
        };
        let local_llm_enabled = std::env::var("LOCAL_LLM_ENABLED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        let local_llm_url = std::env::var("LOCAL_LLM_URL").ok();
        let local_llm_model = std::env::var("LOCAL_LLM_MODEL").ok();
        let local_llm_api_key = std::env::var("LOCAL_LLM_API_KEY").ok();
        let frontier_provider = std::env::var("FRONTIER_PROVIDER").ok();
        let frontier_model = std::env::var("FRONTIER_MODEL").ok();
        let gemini_api_key = std::env::var("GEMINI_API_KEY").ok();
        let openai_api_key = std::env::var("OPENAI_API_KEY").ok();
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let analysis_mode =
            std::env::var("AI_ANALYSIS_MODE").unwrap_or_else(|_| "standard".to_string());
        let max_iterations = std::env::var("AI_MAX_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let api_rate_limit_rpm = std::env::var("API_RATE_LIMIT_RPM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_u32);
        let gemini_rate_limit_rpm = std::env::var("GEMINI_RATE_LIMIT_RPM")
            .ok()
            .and_then(|v| v.parse().ok());
        let openai_rate_limit_rpm = std::env::var("OPENAI_RATE_LIMIT_RPM")
            .ok()
            .and_then(|v| v.parse().ok());
        let anthropic_rate_limit_rpm = std::env::var("ANTHROPIC_RATE_LIMIT_RPM")
            .ok()
            .and_then(|v| v.parse().ok());
        let spend_safety = crate::spend_safety::SpendSafetyConfig::load_from_env()
            .context("load spend safety config")?;

        Ok(Self {
            provider,
            local_llm_enabled,
            local_llm_url,
            local_llm_model,
            local_llm_api_key,
            frontier_provider,
            frontier_model,
            gemini_api_key,
            openai_api_key,
            anthropic_api_key,
            analysis_mode,
            max_iterations,
            api_rate_limit_rpm,
            gemini_rate_limit_rpm,
            openai_rate_limit_rpm,
            anthropic_rate_limit_rpm,
            spend_safety,
        })
    }

    pub fn rate_limit_for_provider(&self, provider: &str) -> Option<u32> {
        let provider = provider.to_lowercase();
        if provider == "lmstudio"
            || provider == "local"
            || provider == "ollama"
            || provider == "omlx"
            || provider == "codex"
        {
            return None;
        }
        let override_rpm = match provider.as_str() {
            "gemini" => self.gemini_rate_limit_rpm,
            "openai" => self.openai_rate_limit_rpm,
            "anthropic" => self.anthropic_rate_limit_rpm,
            _ => None,
        };
        let rpm = override_rpm.unwrap_or(self.api_rate_limit_rpm);
        if rpm == 0 {
            None
        } else {
            Some(rpm)
        }
    }
}

/// Create provider from config. Returns frontier or local based on AI_PROVIDER.
pub fn create_provider(config: &AIConfig) -> Result<Box<dyn AIProvider>> {
    let provider = config.provider.to_lowercase();
    match provider.as_str() {
        "lmstudio" | "local" | "ollama" | "omlx" => {
            let url = config
                .local_llm_url
                .as_deref()
                .unwrap_or("http://localhost:1234/v1");
            let model = config
                .local_llm_model
                .clone()
                .unwrap_or_else(|| "local-model".to_string());
            Ok(Box::new(LMStudioProvider::new(
                url.to_string(),
                model,
                config.local_llm_api_key.clone(),
            )?))
        }
        "gemini" => {
            let api_key = config
                .gemini_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY required for gemini provider"))?;
            let model = config
                .frontier_model
                .clone()
                .unwrap_or_else(|| "gemini-2.0-flash-exp".to_string());
            Ok(Box::new(GeminiProvider::new(api_key, model)?))
        }
        "openai" => {
            let api_key = config
                .openai_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY required for openai provider"))?;
            let model = config
                .frontier_model
                .clone()
                .unwrap_or_else(|| "gpt-4o".to_string());
            Ok(Box::new(OpenAIProvider::new(api_key, model)?))
        }
        "anthropic" => {
            let api_key = config.anthropic_api_key.clone().ok_or_else(|| {
                anyhow::anyhow!("ANTHROPIC_API_KEY required for anthropic provider")
            })?;
            let model = config
                .frontier_model
                .clone()
                .unwrap_or_else(|| "claude-3-5-sonnet-20241022".to_string());
            Ok(Box::new(AnthropicProvider::new(api_key, model)?))
        }
        "codex" => Ok(Box::new(CodexCliProvider::from_env()?)),
        "hybrid" => {
            // For hybrid, create the frontier provider. If no API keys are set, fall back to local LLM as frontier (local-only).
            let frontier_name = config
                .frontier_provider
                .as_deref()
                .unwrap_or("gemini")
                .to_lowercase();
            let frontier = match frontier_name.as_str() {
                "gemini" => {
                    if let Some(api_key) = config.gemini_api_key.clone() {
                        let model = config
                            .frontier_model
                            .clone()
                            .unwrap_or_else(|| "gemini-2.0-flash-exp".to_string());
                        Some(Box::new(GeminiProvider::new(api_key, model)?) as Box<dyn AIProvider>)
                    } else {
                        None
                    }
                }
                "openai" => {
                    if let Some(api_key) = config.openai_api_key.clone() {
                        let model = config
                            .frontier_model
                            .clone()
                            .unwrap_or_else(|| "gpt-4o".to_string());
                        Some(Box::new(OpenAIProvider::new(api_key, model)?) as Box<dyn AIProvider>)
                    } else {
                        None
                    }
                }
                "anthropic" => {
                    if let Some(api_key) = config.anthropic_api_key.clone() {
                        let model = config
                            .frontier_model
                            .clone()
                            .unwrap_or_else(|| "claude-3-5-sonnet-20241022".to_string());
                        Some(Box::new(AnthropicProvider::new(api_key, model)?)
                            as Box<dyn AIProvider>)
                    } else {
                        None
                    }
                }
                "codex" => Some(Box::new(CodexCliProvider::from_env()?) as Box<dyn AIProvider>),
                _ => None,
            };
            if let Some(provider) = frontier {
                return Ok(provider);
            }
            // Fallback: use local LLM as the only provider when no frontier API keys are set.
            if config.local_llm_enabled {
                let url = config
                    .local_llm_url
                    .as_deref()
                    .unwrap_or("http://localhost:1234/v1");
                let model = config
                    .local_llm_model
                    .clone()
                    .unwrap_or_else(|| "local-model".to_string());
                return Ok(Box::new(LMStudioProvider::new(
                    url.to_string(),
                    model,
                    config.local_llm_api_key.clone(),
                )?));
            }
            anyhow::bail!(
                "AI_PROVIDER=hybrid requires either (1) FRONTIER_PROVIDER=gemini|openai|anthropic with the corresponding API key, (2) FRONTIER_PROVIDER=codex with `codex login`, or (3) LOCAL_LLM_ENABLED=true with local LLM running (no API keys needed)."
            )
        }
        _ => {
            if config.frontier_provider.is_some() || config.gemini_api_key.is_some() {
                let api_key = config.gemini_api_key.clone().ok_or_else(|| {
                    anyhow::anyhow!("GEMINI_API_KEY required for default frontier")
                })?;
                let model = config
                    .frontier_model
                    .clone()
                    .unwrap_or_else(|| "gemini-2.0-flash-exp".to_string());
                return Ok(Box::new(GeminiProvider::new(api_key, model)?));
            }
            anyhow::bail!(
                "Unknown or unsupported AI_PROVIDER: {}. Set AI_PROVIDER=gemini|openai|anthropic|codex|hybrid, and FRONTIER_PROVIDER + API key or codex for hybrid.",
                config.provider
            )
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Hybrid provider bundle used by harness-backed analysis.
pub struct HybridRouter {
    pub local_provider: Option<Arc<dyn AIProvider>>,
}

impl HybridRouter {
    pub fn new(
        local_provider: Option<Arc<dyn AIProvider>>,
        _frontier_provider: Arc<dyn AIProvider>,
        _config: &AIConfig,
    ) -> Self {
        Self { local_provider }
    }
}

/// Strip markdown code fence if present and trim.
fn strip_json_response(raw: &str) -> &str {
    let s = raw.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s)
        .trim();
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    s
}

/// Try to fix common LLM JSON mistakes: unescaped " inside string values.
fn repair_json_string_values(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let mut chars = s.chars().peekable();
    let mut in_string_value = false;
    let mut escape_next = false;
    let mut after_colon = false;
    while let Some(c) = chars.next() {
        if escape_next {
            out.push(c);
            escape_next = false;
            continue;
        }
        if c == '\\' && in_string_value {
            out.push(c);
            escape_next = true;
            continue;
        }
        if c == '"' {
            if in_string_value {
                // Possible end of string or unescaped inner quote.
                let rest: String = chars.clone().collect();
                let next_non_ws = rest.chars().find(|ch| !ch.is_whitespace());
                let is_closing = matches!(
                    next_non_ws,
                    Some(',') | Some('}') | Some(']') | Some(':') | Some('"') | None
                );
                if is_closing {
                    in_string_value = false;
                    out.push(c);
                } else {
                    out.push('\\');
                    out.push(c);
                }
            } else {
                out.push(c);
                if after_colon {
                    in_string_value = true;
                    after_colon = false;
                }
            }
            continue;
        }
        if c == ':' && !in_string_value {
            after_colon = true;
            out.push(c);
            continue;
        }
        if c != ' ' && c != '\t' && c != '\n' && c != '\r' {
            after_colon = false;
        }
        out.push(c);
    }
    out
}

/// Parse JSON analysis response into AnalysisResult. If require_confidence, extract min confidence.
/// Tolerates markdown code fences and unescaped quotes inside string values (repairs when possible).
pub fn parse_analysis_response(json_str: &str, require_confidence: bool) -> Result<AnalysisResult> {
    let s = strip_json_response(json_str);
    let repaired = repair_json_string_values(s);
    let v: Value = serde_json::from_str(&repaired).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse AI response as JSON: {}. First 200 chars: {:?}",
            e,
            &json_str.chars().take(200).collect::<String>()
        )
    })?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Expected JSON object"))?;

    let mut min_conf: Option<f32> = None;
    let get_f32 =
        |key: &str| -> Option<f32> { obj.get(key).and_then(|v| v.as_f64()).map(|f| f as f32) };
    if require_confidence {
        for key in [
            "spam_confidence",
            "phishing_confidence",
            "marketing_confidence",
            "category_confidence",
        ] {
            if let Some(c) = get_f32(key) {
                min_conf = Some(min_conf.map(|m| m.min(c)).unwrap_or(c));
            }
        }
    }

    let datetime_opt = |key: &str| -> Option<DateTime<Utc>> {
        obj.get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    };

    Ok(AnalysisResult {
        spam_status: obj
            .get("spam_status")
            .and_then(|v| v.as_str())
            .map(String::from),
        phishing_status: obj
            .get("phishing_status")
            .and_then(|v| v.as_str())
            .map(String::from),
        marketing_status: obj
            .get("marketing_status")
            .and_then(|v| v.as_str())
            .map(String::from),
        otp_status: obj
            .get("otp_status")
            .and_then(|v| v.as_str())
            .map(String::from),
        otp_code: obj
            .get("otp_code")
            .and_then(|v| v.as_str())
            .map(String::from),
        otp_expires: datetime_opt("otp_expires"),
        threat_level: obj
            .get("threat_level")
            .and_then(|v| v.as_str())
            .map(String::from),
        threat_indicators: obj.get("threat_indicators").and_then(|v| {
            v.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
        }),
        ai_summary: obj.get("ai_summary").cloned(),
        human_summary: obj
            .get("human_summary")
            .and_then(|v| v.as_str())
            .map(String::from),
        category: obj
            .get("category")
            .and_then(|v| v.as_str())
            .map(String::from),
        subcategory: obj
            .get("subcategory")
            .and_then(|v| v.as_str())
            .map(String::from),
        organization: obj
            .get("organization")
            .and_then(|v| v.as_str())
            .map(String::from),
        topic: obj.get("topic").and_then(|v| v.as_str()).map(String::from),
        email_type: obj
            .get("email_type")
            .and_then(|v| v.as_str())
            .map(String::from),
        location_recommendation: obj
            .get("location_recommendation")
            .and_then(|v| v.as_str())
            .map(String::from),
        offer_expires: datetime_opt("offer_expires"),
        confidence: min_conf,
        analyzed_by: None,
        token_usage: None,
        queued_for_frontier: false,
    })
}

// --- Provider implementations ---

fn to_rig_tool_choice(choice: ToolChoice) -> RigToolChoice {
    match choice {
        ToolChoice::Auto => RigToolChoice::Auto,
        ToolChoice::None => RigToolChoice::None,
        ToolChoice::Required => RigToolChoice::Required,
    }
}

fn to_rig_message(message: Message) -> RigMessage {
    match message.role.as_str() {
        "system" => RigMessage::system(message.content),
        "assistant" => RigMessage::assistant(message.content),
        _ => RigMessage::user(message.content),
    }
}

fn to_rig_completion_request(
    request: AICompletionRequest,
    model: &str,
) -> Result<RigCompletionRequest> {
    let mut chat_history = request
        .messages
        .into_iter()
        .map(to_rig_message)
        .collect::<Vec<_>>();
    if chat_history.is_empty() {
        chat_history.push(RigMessage::user(""));
    }

    let tools = request
        .tools
        .into_iter()
        .map(|tool| RigToolDefinition {
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
        })
        .collect::<Vec<_>>();
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(to_rig_tool_choice(request.tool_choice))
    };

    Ok(RigCompletionRequest {
        model: Some(model.to_string()),
        preamble: None,
        chat_history: RigOneOrMany::many(chat_history)
            .map_err(|_| anyhow!("AI completion request requires at least one message"))?,
        documents: Vec::new(),
        tools,
        temperature: Some(request.temperature as f64),
        max_tokens: request.max_tokens.map(u64::from),
        tool_choice,
        additional_params: None,
        output_schema: None,
    })
}

fn usage_count(value: u64) -> Option<u32> {
    if value == 0 {
        None
    } else {
        Some(value.min(u64::from(u32::MAX)) as u32)
    }
}

fn to_internal_usage(usage: RigUsage) -> Option<TokenUsage> {
    let input_tokens = usage_count(usage.input_tokens);
    let output_tokens = usage_count(usage.output_tokens);
    if input_tokens.is_none() && output_tokens.is_none() {
        None
    } else {
        Some(TokenUsage {
            input_tokens,
            output_tokens,
        })
    }
}

fn to_ai_response<T>(response: RigCompletionResponse<T>) -> AIResponse {
    let mut text_blocks = Vec::new();
    let mut tool_calls = Vec::new();

    for item in response.choice.iter() {
        match item {
            RigAssistantContent::Text(text) => {
                if !text.text.trim().is_empty() {
                    text_blocks.push(text.text.clone());
                }
            }
            RigAssistantContent::ToolCall(tool_call) => {
                tool_calls.push(ToolCall {
                    id: tool_call.id.clone(),
                    name: tool_call.function.name.clone(),
                    arguments: tool_call.function.arguments.clone(),
                });
            }
            RigAssistantContent::Reasoning(reasoning) => {
                let rendered = reasoning.display_text();
                if !rendered.trim().is_empty() {
                    text_blocks.push(rendered);
                }
            }
            RigAssistantContent::Image(_) => {}
        }
    }

    let finish_reason = if tool_calls.is_empty() {
        "stop".to_string()
    } else {
        "tool_calls".to_string()
    };

    AIResponse {
        content: text_blocks.join("\n"),
        confidence: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason,
        usage: to_internal_usage(response.usage),
    }
}

async fn run_rig_completion<M>(
    model: &M,
    request: AICompletionRequest,
    model_name: &str,
) -> Result<AIResponse>
where
    M: RigCompletionModel,
{
    let completion_request = to_rig_completion_request(request, model_name)?;
    let response = model.completion(completion_request).await?;
    Ok(to_ai_response(response))
}

/// LM Studio / Ollama (OpenAI-compatible).
pub struct LMStudioProvider {
    model: String,
    client: rig::providers::openai::CompletionsClient,
}

impl LMStudioProvider {
    pub fn new(base_url: String, model: String, api_key: Option<String>) -> Result<Self> {
        let normalized_base = base_url
            .trim_end_matches('/')
            .strip_suffix("/chat/completions")
            .unwrap_or(base_url.trim_end_matches('/'))
            .to_string();
        let api_key = api_key.unwrap_or_else(|| "lmstudio".to_string());
        let client = rig::providers::openai::CompletionsClient::builder()
            .api_key(&api_key)
            .base_url(&normalized_base)
            .build()
            .map_err(|err| anyhow!("failed to initialize LM Studio rig client: {err}"))?;

        Ok(Self { model, client })
    }
}

#[async_trait::async_trait]
impl AIProvider for LMStudioProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.complete_with_request(AICompletionRequest::from_messages(messages))
            .await
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        let model = self.client.completion_model(self.model.as_str());
        run_rig_completion(&model, request, &self.model).await
    }

    fn supports_confidence_scores(&self) -> bool {
        true
    }
    fn max_context_tokens(&self) -> usize {
        8_192
    }
    fn is_local(&self) -> bool {
        true
    }
    fn cost_tier(&self) -> CostTier {
        CostTier::Free
    }
}

/// OpenAI Codex CLI provider.
///
/// This uses the user's local `codex` login instead of an OpenAI API key. It is
/// intentionally a subprocess adapter: Codex is not exposed as a normal API
/// credential, but `codex exec` can run non-interactively under the signed-in
/// ChatGPT/Codex account.
pub struct CodexCliProvider {
    bin: String,
    model: Option<String>,
    profile: Option<String>,
    sandbox: String,
    timeout: Duration,
}

impl CodexCliProvider {
    pub fn from_env() -> Result<Self> {
        let bin = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let model = std::env::var("CODEX_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let profile = std::env::var("CODEX_PROFILE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let sandbox = std::env::var("CODEX_SANDBOX")
            .unwrap_or_else(|_| "read-only".to_string())
            .trim()
            .to_string();
        let timeout_secs = std::env::var("CODEX_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(300)
            .max(10);

        Ok(Self {
            bin,
            model,
            profile,
            sandbox,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    fn render_prompt(&self, request: &AICompletionRequest) -> String {
        let mut prompt = String::new();
        prompt.push_str(
            "You are acting as a pure completion backend for MailSubsystem.\n\
             Do not inspect or modify local files. Do not run shell commands. \
             Use only the conversation and tool schemas below.\n\
             Return only the assistant message content requested by the latest user message. \
             If the requested output is JSON, return only that JSON object.\n",
        );

        if !request.tools.is_empty() {
            prompt.push_str(
                "\nMailSubsystem tools are executed by the parent harness, not by Codex. \
                 To call one, output exactly one JSON object in this shape and stop:\n\
                 {\"action\":\"use_tool\",\"tool\":\"<tool_name>\",\"args\":{}}\n\n\
                 Available MailSubsystem tools:\n",
            );
            for tool in &request.tools {
                prompt.push_str(&format!(
                    "- `{}`: {}\n  parameters: {}\n",
                    tool.name, tool.description, tool.parameters
                ));
            }
        }

        prompt.push_str("\nConversation:\n");
        for message in &request.messages {
            prompt.push_str(&format!(
                "\n<{}>\n{}\n</{}>\n",
                message.role, message.content, message.role
            ));
        }

        prompt
    }

    fn temp_output_path() -> PathBuf {
        std::env::temp_dir().join(format!("mailsubsystem-codex-output-{}.txt", Uuid::new_v4()))
    }

    fn temp_work_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mailsubsystem-codex-work-{}", Uuid::new_v4()))
    }

    async fn ensure_logged_in(&self) -> Result<()> {
        let mut command = Command::new(&self.bin);
        command
            .arg("login")
            .arg("status")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match tokio::time::timeout(Duration::from_secs(15), command.output()).await {
            Ok(result) => result.map_err(|err| {
                anyhow!(
                    "failed to start Codex CLI with `{}`: {}. Install the Codex CLI or set CODEX_BIN to its path.",
                    self.bin,
                    err
                )
            })?,
            Err(_) => {
                anyhow::bail!(
                    "Codex CLI login check timed out. Run `codex login status` to inspect the local Codex session."
                );
            }
        };

        if output.status.success() {
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Codex CLI is not logged in. Run `codex login` once, then retry. \
             For headless environments, try `codex login --device-auth`. \
             `codex login status` stderr: {} stdout: {}",
            stderr.trim(),
            stdout.trim()
        );
    }

    async fn run_codex_exec(&self, prompt: &str) -> Result<String> {
        self.ensure_logged_in().await?;

        let output_path = Self::temp_output_path();
        let work_dir = Self::temp_work_dir();
        std::fs::create_dir_all(&work_dir).context("create Codex CLI temp workdir")?;
        let mut command = Command::new(&self.bin);
        command
            .arg("--ask-for-approval")
            .arg("never")
            .arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--cd")
            .arg(&work_dir)
            .arg("--sandbox")
            .arg(&self.sandbox)
            .arg("--color")
            .arg("never")
            .arg("--output-last-message")
            .arg(&output_path);

        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        if let Some(profile) = &self.profile {
            command.arg("--profile").arg(profile);
        }

        command
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|err| {
            anyhow!(
                "failed to start Codex CLI provider with `{}`: {}. Run `codex login` and set CODEX_BIN if needed.",
                self.bin,
                err
            )
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open Codex CLI stdin"))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("write Codex CLI prompt")?;
        drop(stdin);

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => result.context("wait for Codex CLI provider")?,
            Err(_) => {
                let _ = std::fs::remove_file(&output_path);
                let _ = std::fs::remove_dir_all(&work_dir);
                anyhow::bail!("Codex CLI provider timed out after {:?}", self.timeout);
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            let _ = std::fs::remove_file(&output_path);
            let _ = std::fs::remove_dir_all(&work_dir);
            anyhow::bail!(
                "Codex CLI provider failed with status {}. stderr: {} stdout: {}",
                output.status,
                stderr.trim(),
                stdout.trim()
            );
        }

        let final_message = std::fs::read_to_string(&output_path)
            .unwrap_or_else(|_| stdout.clone())
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_dir_all(&work_dir);

        if final_message.is_empty() {
            anyhow::bail!(
                "Codex CLI provider returned an empty message. stderr: {} stdout: {}",
                stderr.trim(),
                stdout.trim()
            );
        }

        Ok(final_message)
    }
}

#[async_trait::async_trait]
impl AIProvider for CodexCliProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.complete_with_request(AICompletionRequest::from_messages(messages))
            .await
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        let prompt = self.render_prompt(&request);
        let content = self.run_codex_exec(&prompt).await?;
        Ok(AIResponse {
            content,
            confidence: None,
            tool_calls: None,
            finish_reason: "stop".to_string(),
            usage: None,
        })
    }

    fn supports_tool_calling(&self) -> bool {
        true
    }

    fn max_context_tokens(&self) -> usize {
        128_000
    }

    fn cost_tier(&self) -> CostTier {
        CostTier::Cheap
    }
}

/// Google Gemini.
pub struct GeminiProvider {
    model: String,
    client: rig::providers::gemini::Client,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let client = rig::providers::gemini::Client::new(api_key)
            .map_err(|err| anyhow!("failed to initialize Gemini rig client: {err}"))?;
        Ok(Self { model, client })
    }
}

#[async_trait::async_trait]
impl AIProvider for GeminiProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.complete_with_request(AICompletionRequest::from_messages(messages))
            .await
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        let model = self.client.completion_model(self.model.as_str());
        run_rig_completion(&model, request, &self.model).await
    }

    fn supports_tool_calling(&self) -> bool {
        true
    }

    fn max_context_tokens(&self) -> usize {
        128_000
    }
    fn cost_tier(&self) -> CostTier {
        CostTier::Cheap
    }
}

/// OpenAI.
pub struct OpenAIProvider {
    model: String,
    client: rig::providers::openai::CompletionsClient,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let client = rig::providers::openai::CompletionsClient::new(api_key)
            .map_err(|err| anyhow!("failed to initialize OpenAI rig client: {err}"))?;
        Ok(Self { model, client })
    }
}

#[async_trait::async_trait]
impl AIProvider for OpenAIProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.complete_with_request(AICompletionRequest::from_messages(messages))
            .await
    }

    async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
        let model = self.client.completion_model(self.model.as_str());
        run_rig_completion(&model, request, &self.model).await
    }

    fn supports_tool_calling(&self) -> bool {
        true
    }

    fn cost_tier(&self) -> CostTier {
        CostTier::Cheap
    }
}

/// Anthropic Claude.
pub struct AnthropicProvider {
    model: String,
    client: rig::providers::anthropic::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let client = rig::providers::anthropic::Client::new(api_key)
            .map_err(|err| anyhow!("failed to initialize Anthropic rig client: {err}"))?;
        Ok(Self { model, client })
    }
}

#[async_trait::async_trait]
impl AIProvider for AnthropicProvider {
    async fn complete(&self, messages: Vec<Message>) -> Result<AIResponse> {
        self.complete_with_request(AICompletionRequest::from_messages(messages))
            .await
    }

    async fn complete_with_request(&self, mut request: AICompletionRequest) -> Result<AIResponse> {
        // Anthropic API requires max_tokens on every request.
        if request.max_tokens.is_none() {
            request.max_tokens = Some(4096);
        }
        let model = self.client.completion_model(self.model.as_str());
        run_rig_completion(&model, request, &self.model).await
    }

    fn supports_tool_calling(&self) -> bool {
        true
    }

    fn cost_tier(&self) -> CostTier {
        CostTier::Cheap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ai_config_debug_redacts_api_keys() {
        let config = AIConfig {
            local_llm_api_key: Some("local-super-secret".to_string()),
            gemini_api_key: Some("gemini-super-secret".to_string()),
            openai_api_key: Some("openai-super-secret".to_string()),
            anthropic_api_key: Some("anthropic-super-secret".to_string()),
            ..AIConfig::default()
        };

        let debug = format!("{config:?}");

        assert!(debug.contains("local_llm_api_key: Some(\"<redacted>\")"));
        assert!(debug.contains("gemini_api_key: Some(\"<redacted>\")"));
        assert!(debug.contains("openai_api_key: Some(\"<redacted>\")"));
        assert!(debug.contains("anthropic_api_key: Some(\"<redacted>\")"));
        for secret in [
            "local-super-secret",
            "gemini-super-secret",
            "openai-super-secret",
            "anthropic-super-secret",
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn test_parse_analysis_response_minimal() {
        let json = r#"{"spam_status":"not-spam","phishing_status":"not-phishing","marketing_status":"not-marketing","category":"work"}"#;
        let r = parse_analysis_response(json, false).unwrap();
        assert_eq!(r.spam_status.as_deref(), Some("not-spam"));
        assert_eq!(r.phishing_status.as_deref(), Some("not-phishing"));
        assert_eq!(r.category.as_deref(), Some("work"));
        assert_eq!(r.confidence, None);
    }

    #[test]
    fn test_parse_analysis_response_with_confidence() {
        let json = r#"{
            "spam_status":"spam",
            "spam_confidence":0.95,
            "phishing_confidence":0.90,
            "marketing_confidence":0.88,
            "category_confidence":0.92
        }"#;
        let r = parse_analysis_response(json, true).unwrap();
        assert_eq!(r.spam_status.as_deref(), Some("spam"));
        assert_eq!(r.confidence, Some(0.88));
    }

    #[test]
    fn test_parse_analysis_response_invalid_json() {
        assert!(parse_analysis_response("not json", false).is_err());
        assert!(parse_analysis_response("[]", false).is_err());
    }

    #[test]
    fn test_parse_analysis_response_empty_object() {
        let r = parse_analysis_response("{}", false).unwrap();
        assert_eq!(r.spam_status, None);
        assert_eq!(r.confidence, None);
    }

    #[test]
    fn test_parse_analysis_response_unescaped_quote_in_string() {
        // LLMs sometimes put unescaped " inside string values; repair should fix it.
        let json = r#"{"spam_status":"spam","phishing_status":"not-phishing","marketing_status":"marketing","otp_status":null,"otp_expires":null,"ai_summary":"Promotional message with "suspicious" links and poor grammar.","human_summary":null,"category":"personal","subcategory":null,"organization":null,"topic":null,"email_type":"actionable","location_recommendation":"Junk","offer_expires":null}"#;
        let r = parse_analysis_response(json, false).unwrap();
        assert_eq!(r.spam_status.as_deref(), Some("spam"));
        assert_eq!(
            r.ai_summary.as_ref().and_then(|v| v.as_str()),
            Some("Promotional message with \"suspicious\" links and poor grammar.")
        );
    }

    #[test]
    fn test_location_tools_names() {
        let tools = location_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "list_subfolders",
                "get_folder_emails_summary",
                "get_folder_tree",
                "get_folder_email_samples",
            ]
        );
    }

    #[test]
    fn test_to_rig_completion_request_preserves_messages_tools_and_limits() {
        let request = AICompletionRequest {
            messages: vec![
                Message::system("You are a test system."),
                Message::user("Find matching emails."),
            ],
            tools: vec![CompletionTool {
                name: "search_similar_emails".to_string(),
                description: "Find similar messages by query".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }),
            }],
            tool_choice: ToolChoice::Required,
            temperature: 0.35,
            max_tokens: Some(256),
        };

        let rig_request = to_rig_completion_request(request, "test-model").unwrap();

        assert_eq!(rig_request.model.as_deref(), Some("test-model"));
        assert_eq!(rig_request.max_tokens, Some(256));
        assert_eq!(rig_request.tool_choice, Some(RigToolChoice::Required));
        let temperature = rig_request.temperature.expect("temperature should be set");
        assert!((temperature - 0.35).abs() < 1e-6);
        assert_eq!(rig_request.tools.len(), 1);
        assert_eq!(rig_request.tools[0].name, "search_similar_emails");

        let messages: Vec<_> = rig_request.chat_history.iter().collect();
        assert_eq!(messages.len(), 2);
        match messages[0] {
            RigMessage::System { content } => {
                assert_eq!(content, "You are a test system.");
            }
            other => panic!("expected system message, got {other:?}"),
        }
        match messages[1] {
            RigMessage::User { .. } => {}
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn test_to_ai_response_preserves_content_tool_calls_and_usage() {
        let rig_response = RigCompletionResponse {
            choice: RigOneOrMany::many(vec![
                RigAssistantContent::text("analysis summary"),
                RigAssistantContent::tool_call(
                    "call_1".to_string(),
                    "search_similar_emails".to_string(),
                    json!({ "query": "invoice reminder" }),
                ),
            ])
            .unwrap(),
            usage: RigUsage {
                input_tokens: 42,
                output_tokens: 8,
                total_tokens: 50,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: (),
            message_id: None,
        };

        let response = to_ai_response(rig_response);

        assert_eq!(response.content, "analysis summary");
        assert_eq!(response.finish_reason, "tool_calls");
        let tool_calls = response.tool_calls.expect("expected tool calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "search_similar_emails");
        assert_eq!(
            tool_calls[0].arguments,
            json!({ "query": "invoice reminder" })
        );
        assert_eq!(
            response.usage.as_ref().and_then(|u| u.input_tokens),
            Some(42)
        );
        assert_eq!(
            response.usage.as_ref().and_then(|u| u.output_tokens),
            Some(8)
        );
    }

    #[test]
    fn test_to_internal_usage_handles_zero_and_large_counts() {
        assert!(to_internal_usage(RigUsage::new()).is_none());

        let usage = RigUsage {
            input_tokens: u64::from(u32::MAX) + 10,
            output_tokens: 1,
            total_tokens: u64::from(u32::MAX) + 11,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
        };

        let token_usage = to_internal_usage(usage).expect("expected usage");
        assert_eq!(token_usage.input_tokens, Some(u32::MAX));
        assert_eq!(token_usage.output_tokens, Some(1));
    }

    #[test]
    fn test_rate_limit_override_precedence() {
        let mut cfg = AIConfig {
            api_rate_limit_rpm: 60,
            gemini_rate_limit_rpm: Some(20),
            openai_rate_limit_rpm: None,
            anthropic_rate_limit_rpm: Some(15),
            ..AIConfig::default()
        };

        assert_eq!(cfg.rate_limit_for_provider("gemini"), Some(20));
        assert_eq!(cfg.rate_limit_for_provider("openai"), Some(60));
        assert_eq!(cfg.rate_limit_for_provider("anthropic"), Some(15));
        assert_eq!(cfg.rate_limit_for_provider("lmstudio"), None);

        cfg.api_rate_limit_rpm = 0;
        assert_eq!(cfg.rate_limit_for_provider("openai"), None);
    }

    #[test]
    fn test_codex_prompt_includes_mail_subsystem_tool_protocol() {
        let provider = CodexCliProvider {
            bin: "codex".to_string(),
            model: None,
            profile: None,
            sandbox: "read-only".to_string(),
            timeout: Duration::from_secs(30),
        };
        let request = AICompletionRequest {
            messages: vec![
                Message::system("Return JSON only."),
                Message::user("Find matching emails."),
            ],
            tools: vec![CompletionTool {
                name: "search_similar_emails".to_string(),
                description: "Find similar messages by query".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    }
                }),
            }],
            tool_choice: ToolChoice::Auto,
            temperature: 0.2,
            max_tokens: Some(512),
        };

        let prompt = provider.render_prompt(&request);

        assert!(prompt.contains("pure completion backend for MailSubsystem"));
        assert!(prompt.contains(r#"{"action":"use_tool","tool":"<tool_name>","args":{}}"#));
        assert!(prompt.contains("search_similar_emails"));
        assert!(prompt.contains("<system>\nReturn JSON only.\n</system>"));
        assert!(prompt.contains("<user>\nFind matching emails.\n</user>"));
    }

    #[test]
    fn test_codex_provider_is_not_api_rate_limited() {
        let cfg = AIConfig::default();

        assert_eq!(cfg.rate_limit_for_provider("codex"), None);
    }
}
