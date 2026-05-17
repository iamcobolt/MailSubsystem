//! Agentic location (filing) analysis via AgentHarness.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::ai::AIProvider;
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{Database, EmailRecord};
use crate::harness::{build_location_tools, resolve_provider, AgentHarness, AgentSpec};
use crate::rag::RAGContextBuilder;

/// Location agent wrapper around the harness execution path.
pub struct LocationAgent {
    pub provider: Arc<dyn AIProvider>,
    pub db: Arc<Database>,
    pub rag: Option<Arc<RAGContextBuilder>>,
    pub max_iterations: usize,
    pub agents_dir: String,
    pub account_id: String,
}

/// Result of location recommendation: path and whether to create the folder if missing.
#[derive(Debug, Clone)]
pub struct LocationRecommendation {
    pub location_recommendation: String,
    pub create_if_missing: bool,
    pub reason: Option<String>,
}

impl LocationAgent {
    pub fn new(provider: Arc<dyn AIProvider>, db: Arc<Database>, max_iterations: usize) -> Self {
        Self {
            provider,
            db,
            rag: None,
            max_iterations,
            agents_dir: "./specs/agents".to_string(),
            account_id: DEFAULT_ACCOUNT_ID.to_string(),
        }
    }

    pub fn with_rag(mut self, rag: Arc<RAGContextBuilder>) -> Self {
        self.rag = Some(rag);
        self
    }

    pub fn with_agent_specs(mut self, agents_dir: impl Into<String>) -> Self {
        self.agents_dir = agents_dir.into();
        self
    }

    pub fn with_account_id(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = account_id.into();
        self
    }

    /// Recommend a folder location using the canonical harness path.
    pub async fn recommend_location(&self, email: &EmailRecord) -> Result<LocationRecommendation> {
        self.recommend_with_harness(email).await
    }

    async fn recommend_with_harness(&self, email: &EmailRecord) -> Result<LocationRecommendation> {
        let spec_path = format!("{}/location-agent.md", self.agents_dir);
        let mut spec = AgentSpec::parse_file(Path::new(&spec_path))
            .with_context(|| format!("load agent spec: {}", spec_path))?;
        spec.execution.max_iterations = self.max_iterations.max(1);
        spec.budget.max_llm_calls = spec.budget.max_llm_calls.max(spec.execution.max_iterations);

        let tools =
            build_location_tools(self.db.clone(), self.rag.clone(), self.account_id.clone());
        let provider = resolve_provider(&spec, None, Some(self.provider.clone()))
            .map_err(|error| anyhow::anyhow!(error))?;

        let mut harness = AgentHarness::new(
            spec,
            self.account_id.as_str(),
            self.db.clone(),
            provider,
            tools,
        );
        let input = serde_json::json!({
            "message_id": email.message_id.as_str(),
            "subject": email.subject.as_deref(),
            "sender": email.sender.as_deref(),
            "category": email.category.as_deref(),
            "subcategory": email.subcategory.as_deref(),
            "email_type": email.email_type.as_deref(),
            "organization": email.organization.as_deref(),
            "topic": email.topic.as_deref(),
            "human_summary": email.human_summary.as_deref(),
            "spam_status": email.spam_status.as_str(),
            "marketing_status": email.marketing_status.as_str(),
        });
        let result = harness.run(&email.message_id, input).await?;

        Ok(LocationRecommendation {
            location_recommendation: result
                .output
                .get("folder_path")
                .and_then(|value| value.as_str())
                .unwrap_or("INBOX")
                .to_string(),
            create_if_missing: result
                .output
                .get("create_if_missing")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            reason: result
                .output
                .get("reasoning")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string()),
        })
    }
}
