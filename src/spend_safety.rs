//! Spend-safety configuration and audit event contracts.
//!
//! This module intentionally stops at data contracts. Runtime enforcement,
//! provider disable persistence, and approval prompts should consume these
//! types once the CLI/TUI/API surfaces are wired.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SPEND_AUDIT_LOG_PATH: &str = "logs/spend_approval_audit.jsonl";
pub const DEFAULT_SPEND_CURRENCY: &str = "USD";
pub const DEFAULT_PANIC_WINDOW_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendApprovalMode {
    /// Record estimates and decisions, but do not block requests.
    AuditOnly,
    /// Interactive surfaces should ask before continuing.
    Prompt,
    /// Non-interactive callers must provide a prior approval token.
    Enforce,
}

impl Default for SpendApprovalMode {
    fn default() -> Self {
        Self::AuditOnly
    }
}

impl FromStr for SpendApprovalMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match normalized_value(value).as_str() {
            "audit_only" | "audit" => Ok(Self::AuditOnly),
            "prompt" | "confirm" => Ok(Self::Prompt),
            "enforce" | "required" => Ok(Self::Enforce),
            other => anyhow::bail!(
                "invalid spend approval mode '{}'; expected audit_only, prompt, or enforce",
                other
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendApprovalSurface {
    Cli,
    Tui,
    Api,
    Core,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendApprovalDecision {
    Approved,
    Denied,
    Expired,
    AutoApproved,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendAuditEventType {
    ApprovalRequested,
    ApprovalGranted,
    ApprovalDenied,
    BudgetExceeded,
    PanicThresholdTriggered,
    ProviderAutoDisabled,
    ProviderReenabled,
    SpendRecorded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendEstimate {
    pub provider: String,
    pub model: Option<String>,
    pub currency: String,
    pub estimated_cost_cents: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl SpendEstimate {
    pub fn new(provider: impl Into<String>, estimated_cost_cents: u64) -> Self {
        Self {
            provider: provider.into(),
            model: None,
            currency: DEFAULT_SPEND_CURRENCY.to_string(),
            estimated_cost_cents,
            input_tokens: None,
            output_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPanicThreshold {
    pub window_secs: u64,
    pub spend_cents: Option<u64>,
    pub auto_disable: bool,
}

impl ProviderPanicThreshold {
    pub fn should_auto_disable(&self, window_spend_cents: u64) -> bool {
        if !self.auto_disable {
            return false;
        }
        match self.spend_cents {
            Some(threshold) => window_spend_cents >= threshold,
            None => false,
        }
    }
}

impl Default for ProviderPanicThreshold {
    fn default() -> Self {
        Self {
            window_secs: DEFAULT_PANIC_WINDOW_SECS,
            spend_cents: None,
            auto_disable: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendSafetyConfig {
    pub enabled: bool,
    pub approval_mode: SpendApprovalMode,
    pub confirmation_threshold_cents: Option<u64>,
    pub daily_budget_cents: Option<u64>,
    pub monthly_budget_cents: Option<u64>,
    pub panic: ProviderPanicThreshold,
    pub audit_log_path: String,
}

impl SpendSafetyConfig {
    pub fn load_from_env() -> Result<Self> {
        Self::from_env_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_env_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self> {
        let mut config = Self::default();

        config.enabled = parse_bool_env(&mut get, "SPEND_SAFETY_ENABLED", config.enabled)?;

        if let Some(value) = env_value(&mut get, "SPEND_APPROVAL_MODE") {
            config.approval_mode = value.parse().with_context(|| "parse SPEND_APPROVAL_MODE")?;
        }

        config.confirmation_threshold_cents =
            parse_optional_cents_env(&mut get, "SPEND_CONFIRMATION_THRESHOLD_CENTS")?;
        config.daily_budget_cents = parse_optional_cents_env(&mut get, "SPEND_DAILY_BUDGET_CENTS")?;
        config.monthly_budget_cents =
            parse_optional_cents_env(&mut get, "SPEND_MONTHLY_BUDGET_CENTS")?;
        config.panic.spend_cents =
            parse_optional_cents_env(&mut get, "SPEND_PANIC_THRESHOLD_CENTS")?;
        config.panic.window_secs = parse_u64_env(
            &mut get,
            "SPEND_PANIC_WINDOW_SECS",
            config.panic.window_secs,
        )?;
        config.panic.auto_disable = parse_bool_env(
            &mut get,
            "SPEND_PANIC_AUTO_DISABLE",
            config.panic.auto_disable,
        )?;

        if let Some(path) = env_value(&mut get, "SPEND_AUDIT_LOG_PATH") {
            config.audit_log_path = path;
        }

        Ok(config)
    }

    pub fn requires_confirmation(&self, estimate: &SpendEstimate) -> bool {
        if !self.enabled || self.approval_mode == SpendApprovalMode::AuditOnly {
            return false;
        }
        match self.confirmation_threshold_cents {
            Some(threshold) => estimate.estimated_cost_cents >= threshold,
            None => false,
        }
    }
}

impl Default for SpendSafetyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            approval_mode: SpendApprovalMode::AuditOnly,
            confirmation_threshold_cents: None,
            daily_budget_cents: None,
            monthly_budget_cents: None,
            panic: ProviderPanicThreshold::default(),
            audit_log_path: DEFAULT_SPEND_AUDIT_LOG_PATH.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendAuditEvent {
    pub event_type: SpendAuditEventType,
    pub occurred_at: DateTime<Utc>,
    pub surface: SpendApprovalSurface,
    pub provider: String,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub actor: Option<String>,
    pub request_id: Option<String>,
    pub approval_id: Option<String>,
    pub estimate: Option<SpendEstimate>,
    pub actual_cost_cents: Option<u64>,
    pub decision: Option<SpendApprovalDecision>,
    pub reason: Option<String>,
}

impl SpendAuditEvent {
    pub fn new(
        event_type: SpendAuditEventType,
        surface: SpendApprovalSurface,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            event_type,
            occurred_at: Utc::now(),
            surface,
            provider: provider.into(),
            model: None,
            account_id: None,
            actor: None,
            request_id: None,
            approval_id: None,
            estimate: None,
            actual_cost_cents: None,
            decision: None,
            reason: None,
        }
    }

    pub fn with_estimate(mut self, estimate: SpendEstimate) -> Self {
        self.model = estimate.model.clone();
        self.provider = estimate.provider.clone();
        self.estimate = Some(estimate);
        self
    }

    pub fn with_decision(mut self, decision: SpendApprovalDecision) -> Self {
        self.decision = Some(decision);
        self
    }
}

fn normalized_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn env_value(get: &mut impl FnMut(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_bool_env(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &str,
    default: bool,
) -> Result<bool> {
    let Some(value) = env_value(get, key) else {
        return Ok(default);
    };
    match normalized_value(&value).as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("{} must be a boolean value", key),
    }
}

fn parse_u64_env(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &str,
    default: u64,
) -> Result<u64> {
    let Some(value) = env_value(get, key) else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .with_context(|| format!("parse {} as unsigned integer", key))
}

fn parse_optional_cents_env(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &str,
) -> Result<Option<u64>> {
    let value = parse_u64_env(get, key, 0)?;
    if value == 0 {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_from_pairs(pairs: &[(&str, &str)]) -> SpendSafetyConfig {
        let values = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        SpendSafetyConfig::from_env_lookup(|key| values.get(key).cloned()).expect("config")
    }

    #[test]
    fn default_config_is_audit_only_and_does_not_require_confirmation() {
        let config = SpendSafetyConfig::default();
        let estimate = SpendEstimate::new("gemini", 500);

        assert!(config.enabled);
        assert_eq!(config.approval_mode, SpendApprovalMode::AuditOnly);
        assert!(!config.requires_confirmation(&estimate));
        assert!(!config.panic.should_auto_disable(1_000));
    }

    #[test]
    fn env_config_parses_thresholds_and_panic_policy() {
        let config = config_from_pairs(&[
            ("SPEND_APPROVAL_MODE", "prompt"),
            ("SPEND_CONFIRMATION_THRESHOLD_CENTS", "50"),
            ("SPEND_DAILY_BUDGET_CENTS", "500"),
            ("SPEND_MONTHLY_BUDGET_CENTS", "5000"),
            ("SPEND_PANIC_THRESHOLD_CENTS", "120"),
            ("SPEND_PANIC_WINDOW_SECS", "60"),
            ("SPEND_PANIC_AUTO_DISABLE", "true"),
            ("SPEND_AUDIT_LOG_PATH", "/tmp/spend-audit.jsonl"),
        ]);

        assert_eq!(config.approval_mode, SpendApprovalMode::Prompt);
        assert_eq!(config.confirmation_threshold_cents, Some(50));
        assert_eq!(config.daily_budget_cents, Some(500));
        assert_eq!(config.monthly_budget_cents, Some(5_000));
        assert_eq!(config.panic.window_secs, 60);
        assert_eq!(config.audit_log_path, "/tmp/spend-audit.jsonl");
        assert!(!config.requires_confirmation(&SpendEstimate::new("openai", 49)));
        assert!(config.requires_confirmation(&SpendEstimate::new("openai", 50)));
        assert!(!config.panic.should_auto_disable(119));
        assert!(config.panic.should_auto_disable(120));
    }

    #[test]
    fn invalid_approval_mode_is_reported() {
        let error = SpendSafetyConfig::from_env_lookup(|key| {
            (key == "SPEND_APPROVAL_MODE").then(|| "surprise".to_string())
        })
        .expect_err("invalid mode");

        assert!(error.to_string().contains("SPEND_APPROVAL_MODE"));
    }

    #[test]
    fn audit_event_serializes_with_stable_enum_names() {
        let mut estimate = SpendEstimate::new("anthropic", 75);
        estimate.model = Some("claude-example".to_string());
        estimate.input_tokens = Some(100);
        estimate.output_tokens = Some(20);

        let event = SpendAuditEvent::new(
            SpendAuditEventType::ApprovalRequested,
            SpendApprovalSurface::Cli,
            "anthropic",
        )
        .with_estimate(estimate)
        .with_decision(SpendApprovalDecision::Approved);

        let json = serde_json::to_string(&event).expect("serialize event");
        assert!(json.contains("\"event_type\":\"approval_requested\""));
        assert!(json.contains("\"surface\":\"cli\""));
        assert!(json.contains("\"decision\":\"approved\""));

        let decoded: SpendAuditEvent = serde_json::from_str(&json).expect("deserialize event");
        assert_eq!(decoded.event_type, SpendAuditEventType::ApprovalRequested);
        assert_eq!(decoded.surface, SpendApprovalSurface::Cli);
        assert_eq!(decoded.decision, Some(SpendApprovalDecision::Approved));
        assert_eq!(decoded.model.as_deref(), Some("claude-example"));
    }
}
