//! Email analysis orchestration: agent harness execution, schema repair, DB update.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use crate::ai::{self, AnalysisResult, HybridRouter, Message};
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{Database, EmailRecord};
use crate::harness::{
    build_analysis_tools, build_orchestrator_tools, resolve_provider, AgentHarness, AgentSpec,
    RunResult,
};
use crate::rag::{RAGContextBuilder, SimilarSearchHints};

mod classification_eval;
mod harness_io;
mod result_normalization;
#[cfg(test)]
mod tests;

pub(crate) use classification_eval::run_classification_eval;
pub use result_normalization::apply_analysis_result_for_account;

use harness_io::{
    email_to_harness_input_with_worker_instruction, harness_output_to_analysis_result,
    orchestrator_output_to_analysis_result, truncate_str,
};
use result_normalization::{
    apply_classification_reflection_result, apply_schema_alignment_result,
    needs_classification_reflection, needs_schema_alignment_review, CATEGORY_ALLOWED,
    EMAIL_TYPE_ALLOWED,
};

pub struct EmailAnalyzer {
    pub router: Option<HybridRouter>,
    pub frontier_provider: Arc<dyn ai::AIProvider>,
    pub rag_builder: Arc<RAGContextBuilder>,
    /// Analysis mode: "standard" (single-pass) or "thinking" (iterative tool loop).
    pub analysis_mode: String,
    /// Max iterations for thinking mode.
    pub max_iterations: usize,
    pub agents_dir: String,
    pub db: Option<Arc<Database>>,
    pub account_id: String,
}

impl EmailAnalyzer {
    pub fn new(
        router: Option<HybridRouter>,
        frontier_provider: Arc<dyn ai::AIProvider>,
        rag_builder: Arc<RAGContextBuilder>,
    ) -> Self {
        Self {
            router,
            frontier_provider,
            rag_builder,
            analysis_mode: "standard".to_string(),
            max_iterations: 5,
            agents_dir: "./specs/agents".to_string(),
            db: None,
            account_id: DEFAULT_ACCOUNT_ID.to_string(),
        }
    }

    pub fn with_analysis_mode(
        mut self,
        analysis_mode: impl Into<String>,
        max_iterations: usize,
    ) -> Self {
        self.analysis_mode = analysis_mode.into();
        self.max_iterations = max_iterations.max(1);
        self
    }

    pub fn with_agent_specs(
        mut self,
        agents_dir: impl Into<String>,
        db: Option<Arc<Database>>,
    ) -> Self {
        self.agents_dir = agents_dir.into();
        self.db = db;
        self
    }

    pub fn with_account_id(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = account_id.into();
        self
    }

    fn is_thinking_mode(&self) -> bool {
        self.analysis_mode.eq_ignore_ascii_case("thinking")
            || self.analysis_mode.eq_ignore_ascii_case("iterative")
    }

    fn load_email_harness_spec(&self) -> Result<AgentSpec> {
        let spec_path = format!("{}/email-analyzer.md", self.agents_dir);
        let mut spec = AgentSpec::parse_file(Path::new(&spec_path))
            .with_context(|| format!("load agent spec: {}", spec_path))?;

        if self.is_thinking_mode() {
            // Thinking mode is a harness variant of the same agent: more iterative budget,
            // with escalation and output contracts unchanged.
            let target_iterations = self.max_iterations.max(spec.execution.max_iterations);
            spec.execution.max_iterations = target_iterations;
            spec.budget.max_llm_calls = spec.budget.max_llm_calls.max(target_iterations);
            spec.budget.max_tool_calls = spec.budget.max_tool_calls.max(target_iterations);
        }

        Ok(spec)
    }

    fn load_orchestrator_spec(&self) -> Result<AgentSpec> {
        let spec_path = format!("{}/orchestrator.md", self.agents_dir);
        AgentSpec::parse_file(Path::new(&spec_path))
            .with_context(|| format!("load orchestrator spec: {}", spec_path))
    }

    /// Analyze one email: try local (if hybrid) then frontier, return result.
    pub async fn analyze(&self, email: &EmailRecord) -> Result<AnalysisResult> {
        self.analyze_with_worker_instruction(email, None).await
    }

    pub async fn analyze_with_worker_instruction(
        &self,
        email: &EmailRecord,
        worker_instruction: Option<&str>,
    ) -> Result<AnalysisResult> {
        let result = self.analyze_with_harness(email, worker_instruction).await?;
        let result = self.repair_schema_alignment(email, result).await;
        Ok(self.reflect_classification_if_needed(email, result).await)
    }

    async fn analyze_with_harness(
        &self,
        email: &EmailRecord,
        worker_instruction: Option<&str>,
    ) -> Result<AnalysisResult> {
        let spec = self.load_email_harness_spec()?;

        let db = self.db.as_ref().context("harness requires db")?;
        let tools = build_analysis_tools(self.rag_builder.clone(), self.account_id.clone());
        let provider = resolve_provider(
            &spec,
            self.router
                .as_ref()
                .and_then(|router| router.local_provider.clone()),
            Some(self.frontier_provider.clone()),
        )
        .map_err(|error| anyhow::anyhow!(error))?;

        let mut harness =
            AgentHarness::new(spec, self.account_id.as_str(), db.clone(), provider, tools);
        let result = harness
            .run(
                &email.message_id,
                email_to_harness_input_with_worker_instruction(email, worker_instruction),
            )
            .await?;

        if result.should_escalate {
            log::info!(
                "[harness] escalating {} to frontier: {}",
                email.message_id,
                result.escalate_reason.as_deref().unwrap_or("unknown")
            );
            if let Some(orchestrator_result) = self
                .analyze_with_orchestrator_escalation(email, &result, worker_instruction)
                .await?
            {
                return Ok(orchestrator_result);
            }
            return self
                .analyze_with_harness_frontier(email, &result, worker_instruction)
                .await;
        }

        harness_output_to_analysis_result(result, email)
    }

    async fn analyze_with_harness_frontier(
        &self,
        email: &EmailRecord,
        local_result: &RunResult,
        worker_instruction: Option<&str>,
    ) -> Result<AnalysisResult> {
        let spec = self.load_email_harness_spec()?;

        let db = self.db.as_ref().context("harness requires db")?;
        let tools = build_analysis_tools(self.rag_builder.clone(), self.account_id.clone());
        let provider = self.frontier_provider.clone();

        let mut harness =
            AgentHarness::new(spec, self.account_id.as_str(), db.clone(), provider, tools);
        let mut input = email_to_harness_input_with_worker_instruction(email, worker_instruction);
        let reason = local_result.escalate_reason.clone().unwrap_or_default();
        let input_object = input
            .as_object_mut()
            .context("email harness input must be an object")?;
        input_object.insert(
            "_local_model_output".to_string(),
            local_result.output.clone(),
        );
        input_object.insert("_escalation_reason".to_string(), Value::String(reason));

        let result = harness
            .run(&format!("{}-frontier", email.message_id), input)
            .await?;

        harness_output_to_analysis_result(result, email)
    }

    async fn analyze_with_orchestrator_escalation(
        &self,
        email: &EmailRecord,
        worker_result: &RunResult,
        worker_instruction: Option<&str>,
    ) -> Result<Option<AnalysisResult>> {
        let spec = match self.load_orchestrator_spec() {
            Ok(spec) => spec,
            Err(error) => {
                log::debug!(
                    "[harness] orchestrator escalation skipped for {}: {}",
                    email.message_id,
                    error
                );
                return Ok(None);
            }
        };
        let db = self.db.as_ref().context("harness requires db")?;
        let tools = build_orchestrator_tools(
            db.clone(),
            self.rag_builder.clone(),
            self.account_id.clone(),
        );
        let provider = match resolve_provider(
            &spec,
            self.router
                .as_ref()
                .and_then(|router| router.local_provider.clone()),
            Some(self.frontier_provider.clone()),
        ) {
            Ok(provider) => provider,
            Err(error) => {
                log::debug!(
                    "[harness] orchestrator escalation provider unavailable for {}: {}",
                    email.message_id,
                    error
                );
                return Ok(None);
            }
        };

        let scratchpad_context = Self::load_email_analyzer_scratchpad_context(
            db.as_ref(),
            &self.account_id,
            email.sender.as_deref(),
        )
        .await;
        let input = Self::build_orchestrator_escalation_input(
            &self.account_id,
            email,
            worker_result,
            worker_instruction,
            scratchpad_context,
        );

        let mut harness =
            AgentHarness::new(spec, self.account_id.as_str(), db.clone(), provider, tools);
        let task_id = format!("{}-orchestrator-escalation", email.message_id);
        let run_result = match harness.run(&task_id, input).await {
            Ok(result) => result,
            Err(error) => {
                log::warn!(
                    "[harness] orchestrator escalation failed for {}: {}",
                    email.message_id,
                    error
                );
                return Ok(None);
            }
        };

        let task_type = run_result
            .output
            .get("task_type")
            .and_then(|value| value.as_str());
        if task_type != Some("escalation_review") {
            log::warn!(
                "[harness] orchestrator escalation malformed task_type for {}",
                email.message_id
            );
            return Ok(None);
        }
        if !run_result
            .output
            .get("result")
            .is_some_and(|value| value.is_object())
        {
            log::warn!(
                "[harness] orchestrator escalation missing result object for {}",
                email.message_id
            );
            return Ok(None);
        }

        match orchestrator_output_to_analysis_result(run_result, email) {
            Ok(analysis) => Ok(Some(analysis)),
            Err(error) => {
                log::warn!(
                    "[harness] orchestrator escalation parse failed for {}: {}",
                    email.message_id,
                    error
                );
                Ok(None)
            }
        }
    }

    pub async fn load_email_analyzer_scratchpad_context(
        db: &Database,
        account_id: &str,
        sender: Option<&str>,
    ) -> Value {
        json!({
            "sender_patterns": db
                .get_scratchpad_entry_for_account(account_id, "email-analyzer", "sender_patterns")
                .await
                .ok()
                .flatten(),
            "domain_threat_history": db
                .get_scratchpad_entry_for_account(
                    account_id,
                    "email-analyzer",
                    "domain_threat_history",
                )
                .await
                .ok()
                .flatten(),
            "sender": sender,
        })
    }

    pub fn build_orchestrator_escalation_input(
        account_id: &str,
        email: &EmailRecord,
        worker_result: &RunResult,
        worker_instruction: Option<&str>,
        scratchpad_context: Value,
    ) -> Value {
        json!({
            "task_type": "escalation_review",
            "account_id": account_id,
            "message_id": email.message_id,
            "email": {
                "message_id": email.message_id,
                "subject": email.subject,
                "sender": email.sender,
                "received_date": email.received_date.as_ref().map(|date| date.to_rfc3339()),
                "body_text": email
                    .body_text
                    .as_deref()
                    .map(|body| truncate_str(body, 8_000).to_string()),
                "raw_email_content": email
                    .raw_email_content
                    .as_deref()
                    .map(|raw| truncate_str(raw, 8_000).to_string()),
                "worker_instruction": worker_instruction,
            },
            "worker_output": worker_result.output.clone(),
            "worker_run_id": worker_result.run_id.clone(),
            "worker_llm_calls": worker_result.llm_calls,
            "worker_tool_calls": worker_result.tool_calls,
            "escalation_reason": worker_result.escalate_reason.clone(),
            "scratchpad_context": scratchpad_context,
        })
    }

    /// Run frontier-only analysis (used when processing frontier queue).
    pub async fn analyze_frontier_only(&self, email: &EmailRecord) -> Result<AnalysisResult> {
        let spec = self.load_email_harness_spec()?;
        let db = self.db.as_ref().context("harness requires db")?;
        let tools = build_analysis_tools(self.rag_builder.clone(), self.account_id.clone());
        let mut harness = AgentHarness::new(
            spec,
            self.account_id.as_str(),
            db.clone(),
            self.frontier_provider.clone(),
            tools,
        );
        let result = harness
            .run(
                &format!("{}-frontier", email.message_id),
                email_to_harness_input_with_worker_instruction(email, None),
            )
            .await?;
        let analysis = harness_output_to_analysis_result(result, email)?;
        let analysis = self.repair_schema_alignment(email, analysis).await;
        Ok(self.reflect_classification_if_needed(email, analysis).await)
    }

    async fn reflect_classification_if_needed(
        &self,
        email: &EmailRecord,
        mut result: AnalysisResult,
    ) -> AnalysisResult {
        if !needs_classification_reflection(&result) {
            return result;
        }

        let prompt = self
            .build_classification_reflection_prompt(email, &result)
            .await;
        let response = match self
            .frontier_provider
            .complete(vec![Message::user(prompt)])
            .await
        {
            Ok(response) => response,
            Err(error) => {
                log::warn!(
                    "Classification reflection failed for {}: {}",
                    email.message_id,
                    error
                );
                return result;
            }
        };

        let reflection = match ai::parse_analysis_response(&response.content, true) {
            Ok(reflection) => reflection,
            Err(error) => {
                log::warn!(
                    "Classification reflection returned invalid JSON for {}: {}",
                    email.message_id,
                    error
                );
                return result;
            }
        };

        if apply_classification_reflection_result(&mut result, &reflection) {
            log::debug!(
                "Classification reflection reviewed {} to spam={:?}, phishing={:?}, marketing={:?}, otp={:?}, threat={:?}, category={:?}, email_type={:?}",
                email.message_id,
                result.spam_status,
                result.phishing_status,
                result.marketing_status,
                result.otp_status,
                result.threat_level,
                result.category,
                result.email_type
            );
        }
        result
    }

    async fn build_classification_reflection_prompt(
        &self,
        email: &EmailRecord,
        result: &AnalysisResult,
    ) -> String {
        let analysis_json =
            serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string());
        let body = email
            .body_text
            .as_deref()
            .or(email.raw_email_content.as_deref())
            .unwrap_or("");
        let body_excerpt = truncate_str(body, 8_000);
        let raw_excerpt = email
            .raw_email_content
            .as_deref()
            .map(|raw| truncate_str(raw, 8_000))
            .unwrap_or("");
        let context = self
            .build_classification_reflection_context(email, result)
            .await;
        let context_json =
            serde_json::to_string_pretty(&context).unwrap_or_else(|_| "{}".to_string());
        let allowed_categories = CATEGORY_ALLOWED.join(", ");
        let allowed_email_types = EMAIL_TYPE_ALLOWED.join(", ");

        format!(
            r#"You are doing a classification reflection for MailSubsystem.

Purpose:
- Review the current email analysis for contextual mistakes before mailbox actions run.
- This is not a keyword filter. Use full context: sender identity, authentication/header evidence, body intent, links/domains, thread/RAG history, current analysis rationale, and the recipient workflow.
- Review every classification dimension: spam, phishing, marketing, OTP/auth flow, threat level, category, subcategory, email_type, summaries, organization, and topic.
- Change a field only when the current analysis is contradicted by the evidence or misses important context. Otherwise keep the current value.

Allowed categories: {allowed_categories}
Allowed email_type values: {allowed_email_types}
Allowed status values:
- spam_status: spam | not-spam
- phishing_status: phishing | not-phishing
- marketing_status: marketing | not-marketing
- otp_status: otp | magic_link | password_reset | not_otp
- threat_level: none | low | medium | high | critical

Reflection policy:
- Do not classify educational or newsletter content as phishing merely because it discusses fraud, identity theft, scams, security, or prevention. Require actual deception, credential theft, payment redirection, malware, coercive fake compromise/breach claims, suspicious links, or unsafe attachments.
- Apple Hide My Email aliases are not spoofing evidence by themselves. If `X-ICLOUD-HME` maps the alias to the claimed sender and DKIM/DMARC align for the service domain, treat that as context for legitimacy rather than random attacker infrastructure.
- Delivery/order lures are phishing only when paired with suspicious domains, credential/payment collection, unexpected fees, malware, broken authentication/alignment, or other concrete malicious behavior. Authenticated order/tracking details are transactional evidence.
- User-configured alerts, saved searches, watch lists, job alerts, account alerts, and preference-based notifications are not generic newsletters when the evidence says the recipient created or can manage that alert.
- Marketing status describes the direct payload, not merely a static signature/footer. Signature-only promotion is not marketing; payload/direct-note promotion is marketing.
- OTP/auth flows should be classified as otp, magic_link, or password_reset only when the message actually provides a one-time code, sign-in link, or password reset/recovery flow. Otherwise use not_otp.
- If phishing_status is phishing, threat_level must be high or critical. If threat_level is high or critical but phishing_status is not-phishing, explain the non-phishing threat clearly.
- Use confidence as your certainty across all dimensions. Include field-level confidences.

Return exactly one JSON object, no markdown:
{{
  "spam_status": "...",
  "phishing_status": "...",
  "marketing_status": "...",
  "otp_status": "...",
  "threat_level": "...",
  "category": "...",
  "subcategory": "...",
  "email_type": "...",
  "organization": "...",
  "topic": "...",
  "ai_summary": "3-5 evidence-backed sentences explaining the final classification and any correction.",
  "human_summary": "2 concise user-facing sentences with action/no-action status.",
  "threat_indicators": [],
  "spam_confidence": 0.0,
  "phishing_confidence": 0.0,
  "marketing_confidence": 0.0,
  "category_confidence": 0.0
}}

Email evidence:
Subject: {subject}
From: {sender}
Date: {date}
Current folder/location: {location}
Body excerpt:
{body_excerpt}

Top-level raw/header excerpt:
{raw_excerpt}

Account/RAG context:
{context_json}

Current analysis JSON:
{analysis_json}
"#,
            subject = email.subject.as_deref().unwrap_or(""),
            sender = email.sender.as_deref().unwrap_or(""),
            date = email
                .received_date
                .map(|date| date.to_rfc3339())
                .unwrap_or_default(),
            location = email.location.as_deref().unwrap_or(""),
        )
    }

    async fn build_classification_reflection_context(
        &self,
        email: &EmailRecord,
        result: &AnalysisResult,
    ) -> Value {
        let query = [
            email.subject.as_deref(),
            email.sender.as_deref(),
            result.organization.as_deref(),
            result.topic.as_deref(),
            result.human_summary.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

        let hints = SimilarSearchHints {
            sender: email.sender.as_deref(),
            category: result.category.as_deref(),
            email_type: result.email_type.as_deref(),
            organization: result.organization.as_deref(),
            list_id: None,
            exclude_message_id: Some(email.message_id.as_str()),
        };

        match self
            .rag_builder
            .build_initial_context(
                &self.account_id,
                &email.related_message_ids,
                email.sender.as_deref(),
                Some(query.as_str()),
                hints,
            )
            .await
        {
            Ok(context) => json!({
                "sender_history": context.sender_history,
                "sender_intent_profile": context.sender_intent_profile,
                "thread_summaries": context.thread_summaries,
                "thread_raw_messages": context.thread_raw_messages,
                "similar_emails": context.similar_emails,
            }),
            Err(error) => {
                log::warn!(
                    "Classification reflection context failed for {}: {}",
                    email.message_id,
                    error
                );
                json!({})
            }
        }
    }

    async fn repair_schema_alignment(
        &self,
        email: &EmailRecord,
        mut result: AnalysisResult,
    ) -> AnalysisResult {
        if !needs_schema_alignment_review(&result) {
            return result;
        }

        let provider = self.schema_alignment_provider(&result);
        let prompt = self.build_schema_alignment_prompt(email, &result);
        let response = match provider.complete(vec![Message::user(prompt)]).await {
            Ok(response) => response,
            Err(error) => {
                log::warn!(
                    "Schema alignment repair failed for {}: {}",
                    email.message_id,
                    error
                );
                return result;
            }
        };

        let alignment = match ai::parse_analysis_response(&response.content, false) {
            Ok(alignment) => alignment,
            Err(error) => {
                log::warn!(
                    "Schema alignment repair returned invalid JSON for {}: {}",
                    email.message_id,
                    error
                );
                return result;
            }
        };

        if apply_schema_alignment_result(&mut result, &alignment) {
            log::debug!(
                "Schema alignment repaired {} to category={:?}, subcategory={:?}, email_type={:?}",
                email.message_id,
                result.category,
                result.subcategory,
                result.email_type
            );
        }
        result
    }

    fn schema_alignment_provider(&self, result: &AnalysisResult) -> Arc<dyn ai::AIProvider> {
        let analyzed_by = result.analyzed_by.as_deref().unwrap_or_default();
        if analyzed_by.contains("frontier") || analyzed_by.starts_with("orchestrator:") {
            return self.frontier_provider.clone();
        }

        self.router
            .as_ref()
            .and_then(|router| router.local_provider.clone())
            .unwrap_or_else(|| self.frontier_provider.clone())
    }

    fn build_schema_alignment_prompt(
        &self,
        email: &EmailRecord,
        result: &AnalysisResult,
    ) -> String {
        let analysis_json =
            serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string());
        let body = email
            .body_text
            .as_deref()
            .or(email.raw_email_content.as_deref())
            .unwrap_or("");
        let body_excerpt = truncate_str(body, 2_500);
        let allowed_categories = CATEGORY_ALLOWED.join(", ");
        let allowed_email_types = EMAIL_TYPE_ALLOWED.join(", ");

        format!(
            r#"You are doing only schema alignment for MailSubsystem email analysis.

Existing top-level category list: {allowed_categories}
Existing email_type list: {allowed_email_types}

Top-level category meanings:
- personal: individual correspondence and personal life administration, including property management, landlord/tenant, housing, household utilities, government/legal, safety-compliance, personal security/account notices, and automotive notices.
- work: employer, client, project, meeting, recruiting, interview, resume, career, professional community, or job-alert messages.
- volunteering: charity, community service, nonprofit, club, or unpaid organizer work.
- financial: banks, cards, loans, taxes, investing, billing, invoices, statements, credit monitoring, and account balances.
- shopping: ecommerce orders, receipts, shipping, returns, retail promotions, marketplace alerts, and purchase feedback.
- social: social network activity, connections, follows, comments, events, entertainment, or community notifications that are not primarily work.
- travel: flights, hotels, rail, rides, trips, bookings, itineraries, and travel promotions.
- health: healthcare, appointments, fitness, medication, supplements, wellness, insurance care, and health reminders.
- education: courses, learning, research, editorial newsletters, guides, and reference content.

Email type meanings:
- newsletter: recurring digest, editorial, or multi-item campaign content.
- announcement: one-way organizational/product/service update where no user response is expected.
- notification: automated alert/status update about a monitored item, account, event, preference, or system state.
- actionable: asks the recipient to decide, approve, respond, RSVP, provide information, review, or complete a task.
- conversation: direct person-to-person thread, reply, forward, or ongoing correspondence.
- transactional: account/service lifecycle, security/account event, booking/ticket lifecycle, subscription, or service confirmation.
- receipt: proof of purchase, payment, order, invoice, delivery, dispatch, or refund.
- reference: informational material primarily meant for later lookup rather than immediate action.

Task:
- Compare the current category and email_type against the existing schema lists.
- If the category is already the best aligned top-level value, keep it.
- If the category is missing, clearly too generic, or not in the list, choose the nearest existing top-level category.
- Keep personal when the evidence is life admin, property/housing, household/legal/government, personal account/security, or person-to-person correspondence; do not move those to work unless the email clearly belongs to an employer, client, business project, or career/job context.
- If the email's real kind is novel or more specific than the top-level list, create/preserve that specific element in subcategory using a short snake_case label.
- For email_type, first decide whether the current value is plausible by meaning. Change it only when the current value clearly conflicts with the email's role in the recipient's workflow and another enum is substantially better. If multiple values are plausible, keep the current email_type to avoid churn.
- Do not infer email_type from keywords alone; use the email's role in the recipient's workflow.
- Do not change spam_status, phishing_status, marketing_status, otp_status, summaries, organization, topic, or offer_expires.
- Return only JSON with keys: category, subcategory, email_type.

Email evidence:
Subject: {subject}
From: {sender}
Date: {date}
Body excerpt: {body_excerpt}

Current analysis JSON:
{analysis_json}
"#,
            subject = email.subject.as_deref().unwrap_or(""),
            sender = email.sender.as_deref().unwrap_or(""),
            date = email
                .received_date
                .map(|date| date.to_rfc3339())
                .unwrap_or_default(),
        )
    }
}
