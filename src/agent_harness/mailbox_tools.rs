use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::db::{Database, EmailListFilters, ImapFolder};
use crate::rag::RAGContextBuilder;

use super::tools::{AgentTool, ToolRegistry};

pub struct RagTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    rag: Arc<RAGContextBuilder>,
    account_id: String,
}

#[async_trait]
impl AgentTool for RagTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        self.rag
            .execute_tool_call(self.account_id.as_str(), self.name, args)
            .await
    }
}

pub struct ListSubfoldersTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for ListSubfoldersTool {
    fn name(&self) -> &str {
        "list_subfolders"
    }

    fn description(&self) -> &str {
        "List the immediate child folders under a path."
    }

    fn schema(&self) -> Value {
        list_subfolders_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let folders = self
            .db
            .list_imap_folders_for_account(&self.account_id)
            .await?;
        let delimiter = folders
            .iter()
            .find(|folder| folder.folder_name == path || path.is_empty())
            .and_then(|folder| folder.delimiter.as_deref())
            .unwrap_or("/");
        let immediate = immediate_children(&folders, path, delimiter);
        Ok(serde_json::to_string_pretty(&immediate).unwrap_or_else(|_| "[]".to_string()))
    }
}

pub struct GetFolderTreeTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetFolderTreeTool {
    fn name(&self) -> &str {
        "get_folder_tree"
    }

    fn description(&self) -> &str {
        "Return the current IMAP folder tree."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String> {
        let tree = self
            .db
            .get_folder_tree_for_location_for_account(&self.account_id)
            .await?;
        Ok(format_folder_tree(&tree))
    }
}

pub struct GetFolderEmailsSummaryTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetFolderEmailsSummaryTool {
    fn name(&self) -> &str {
        "get_folder_emails_summary"
    }

    fn description(&self) -> &str {
        "Summarize the kinds of emails already stored in a folder."
    }

    fn schema(&self) -> Value {
        get_folder_emails_summary_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(10) as usize;
        let summary = self
            .db
            .get_per_folder_summaries_for_account(&self.account_id, path, limit)
            .await?;
        Ok(serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".to_string()))
    }
}

pub struct GetFolderEmailSamplesTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetFolderEmailSamplesTool {
    fn name(&self) -> &str {
        "get_folder_email_samples"
    }

    fn description(&self) -> &str {
        "Sample recent emails from a specific folder to inspect content types, senders, and categories"
    }

    fn schema(&self) -> Value {
        get_folder_email_samples_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let folder_path = args
            .get("folder_path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(5)
            .min(10) as usize;
        let samples = self
            .db
            .get_folder_email_samples_for_account(&self.account_id, folder_path, limit)
            .await
            .context("get_folder_email_samples tool")?;
        serde_json::to_string_pretty(&samples).context("serialize folder email samples")
    }
}

pub struct GetDigestStatsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetDigestStatsTool {
    fn name(&self) -> &str {
        "get_digest_stats"
    }

    fn description(&self) -> &str {
        "Get aggregate inbox statistics for a time window: email counts by category, top senders, threat/escalation counts, and action summary"
    }

    fn schema(&self) -> Value {
        get_digest_stats_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let since_str = args
            .get("since")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let since: DateTime<Utc> = DateTime::parse_from_rfc3339(since_str)
            .map(|value| value.with_timezone(&Utc))
            .or_else(|_| since_str.parse::<DateTime<Utc>>())
            .context("parse since datetime")?;
        let top_senders_limit = args
            .get("top_senders_limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(10)
            .min(50) as usize;

        let stats = self
            .db
            .get_email_stats_for_window_for_account(&self.account_id, since)
            .await
            .context("get_digest_stats stats")?;
        let top_senders = self
            .db
            .get_top_senders_for_window_for_account(&self.account_id, since, top_senders_limit)
            .await
            .context("get_digest_stats top_senders")?;
        let escalations = self
            .db
            .get_escalation_count_for_window_for_account(&self.account_id, since)
            .await
            .context("get_digest_stats escalations")?;

        let result = json!({
            "stats": stats,
            "top_senders": top_senders
                .into_iter()
                .map(|(address, count)| json!({ "address": address, "count": count }))
                .collect::<Vec<_>>(),
            "escalations": escalations,
        });
        serde_json::to_string_pretty(&result).context("serialize digest stats")
    }
}

pub struct GetBatchResultsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetBatchResultsTool {
    fn name(&self) -> &str {
        "get_batch_results"
    }

    fn description(&self) -> &str {
        "Retrieve worker analysis outputs for all emails currently assigned to a batch"
    }

    fn schema(&self) -> Value {
        get_batch_results_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let batch_id = args
            .get("batch_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if batch_id.is_empty() {
            anyhow::bail!("batch_id is required");
        }

        let results = self
            .db
            .get_batch_results_for_account(&self.account_id, batch_id)
            .await
            .context("get_batch_results tool")?;
        let payload = json!({
            "batch_id": batch_id,
            "results": results
                .into_iter()
                .map(|(message_id, ai_summary)| json!({
                    "message_id": message_id,
                    "ai_summary": ai_summary
                }))
                .collect::<Vec<_>>()
        });
        serde_json::to_string_pretty(&payload).context("serialize batch results")
    }
}

pub struct GetScratchpadStatsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetScratchpadStatsTool {
    fn name(&self) -> &str {
        "get_scratchpad_stats"
    }

    fn description(&self) -> &str {
        "Get scratchpad key counts and storage stats per worker agent"
    }

    fn schema(&self) -> Value {
        get_scratchpad_stats_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let agent_filter = args
            .get("agent_name")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut stats = self
            .db
            .get_scratchpad_stats_for_account(&self.account_id)
            .await
            .context("get_scratchpad_stats tool")?;
        if let Some(agent_name) = agent_filter {
            stats.retain(|entry| entry.agent_name == agent_name);
        }
        serde_json::to_string_pretty(&stats).context("serialize scratchpad stats")
    }
}

pub struct ReadWorkerScratchpadTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for ReadWorkerScratchpadTool {
    fn name(&self) -> &str {
        "read_worker_scratchpad"
    }

    fn description(&self) -> &str {
        "Read a specific scratchpad key for a worker agent"
    }

    fn schema(&self) -> Value {
        read_worker_scratchpad_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let agent_name = args
            .get("agent_name")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        let key = args
            .get("key")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if agent_name.is_empty() || key.is_empty() {
            anyhow::bail!("agent_name and key are required");
        }

        let value = self
            .db
            .get_scratchpad_entry_for_account(&self.account_id, agent_name, key)
            .await
            .context("read_worker_scratchpad tool")?;
        let payload = json!({
            "agent_name": agent_name,
            "key": key,
            "found": value.is_some(),
            "value": value.unwrap_or(Value::Null),
        });
        serde_json::to_string_pretty(&payload).context("serialize worker scratchpad value")
    }
}

pub struct FlagForReanalysisTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for FlagForReanalysisTool {
    fn name(&self) -> &str {
        "flag_for_reanalysis"
    }

    fn description(&self) -> &str {
        "Flag an analyzed email for one bounded re-analysis pass"
    }

    fn schema(&self) -> Value {
        flag_for_reanalysis_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let message_id = args
            .get("message_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if message_id.is_empty() {
            anyhow::bail!("message_id is required");
        }
        let reason = args
            .get("reason")
            .and_then(|value| value.as_str())
            .unwrap_or("orchestrator requested reanalysis")
            .trim();

        self.db
            .flag_for_reanalysis_for_account(&self.account_id, message_id, reason)
            .await
            .context("flag_for_reanalysis tool")?;

        let payload = json!({
            "status": "ok",
            "message_id": message_id,
            "reason": reason,
        });
        serde_json::to_string_pretty(&payload).context("serialize reanalysis flag result")
    }
}

pub struct GetAccountEmailStatsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for GetAccountEmailStatsTool {
    fn name(&self) -> &str {
        "get_account_email_stats"
    }

    fn description(&self) -> &str {
        "Get aggregate account email stats for an optional time window"
    }

    fn schema(&self) -> Value {
        get_account_email_stats_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let since = if let Some(raw_since) = args.get("since").and_then(|value| value.as_str()) {
            DateTime::parse_from_rfc3339(raw_since)
                .map(|value| value.with_timezone(&Utc))
                .or_else(|_| raw_since.parse::<DateTime<Utc>>())
                .context("parse since datetime")?
        } else {
            let window_days = args
                .get("window_days")
                .and_then(|value| value.as_i64())
                .unwrap_or(30)
                .max(1);
            Utc::now() - Duration::days(window_days)
        };
        let top_senders_limit = args
            .get("top_senders_limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(10)
            .min(50) as usize;

        let stats = self
            .db
            .get_email_stats_for_window_for_account(&self.account_id, since)
            .await
            .context("get_account_email_stats stats")?;
        let top_senders = self
            .db
            .get_top_senders_for_window_for_account(&self.account_id, since, top_senders_limit)
            .await
            .context("get_account_email_stats top_senders")?;
        let escalations = self
            .db
            .get_escalation_count_for_window_for_account(&self.account_id, since)
            .await
            .context("get_account_email_stats escalations")?;

        let payload = json!({
            "since": since.to_rfc3339(),
            "stats": stats,
            "top_senders": top_senders
                .into_iter()
                .map(|(address, count)| json!({ "address": address, "count": count }))
                .collect::<Vec<_>>(),
            "escalations": escalations
        });
        serde_json::to_string_pretty(&payload).context("serialize account stats")
    }
}

pub struct CountEmailsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for CountEmailsTool {
    fn name(&self) -> &str {
        "count_emails"
    }

    fn description(&self) -> &str {
        "Count active synced emails currently stored in Postgres, optionally matching sender, organization, folder, category, type, time window, or text filters. Use this before answering user questions like 'how many emails from X?' or 'count emails about X'."
    }

    fn schema(&self) -> Value {
        count_emails_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let since = args
            .get("since")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(|raw_since| {
                DateTime::parse_from_rfc3339(raw_since)
                    .map(|value| value.with_timezone(&Utc))
                    .or_else(|_| raw_since.parse::<DateTime<Utc>>())
                    .context("parse since datetime")
            })
            .transpose()?;
        let sample_limit = args
            .get("sample_limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(5)
            .min(10) as usize;

        let page = self
            .db
            .list_emails_for_account(
                &self.account_id,
                EmailListFilters {
                    category: text_arg(&args, "category"),
                    email_type: text_arg(&args, "email_type"),
                    spam_status: text_arg(&args, "spam_status"),
                    search: text_arg(&args, "query"),
                    folder: text_arg(&args, "folder"),
                    sender: text_arg(&args, "sender"),
                    organization: text_arg(&args, "organization"),
                    since,
                },
                sample_limit,
                0,
            )
            .await
            .context("count_emails list")?;

        let samples = page
            .emails
            .into_iter()
            .map(|email| {
                json!({
                    "message_id": email.message_id,
                    "subject": email.subject,
                    "sender": email.sender,
                    "received_date": email.received_date.map(|date| date.to_rfc3339()),
                    "organization": email.organization,
                    "category": email.category,
                    "email_type": email.email_type,
                    "folder": email.location,
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "total_count": page.total_count,
            "count_scope": "active synced emails currently stored in Postgres",
            "filters": {
                "query": text_arg(&args, "query"),
                "sender": text_arg(&args, "sender"),
                "organization": text_arg(&args, "organization"),
                "category": text_arg(&args, "category"),
                "email_type": text_arg(&args, "email_type"),
                "spam_status": text_arg(&args, "spam_status"),
                "folder": text_arg(&args, "folder"),
                "since": since.map(|value| value.to_rfc3339()),
            },
            "matching": "case-insensitive partial match for text filters",
            "sample_emails": samples,
        });
        serde_json::to_string_pretty(&payload).context("serialize email count")
    }
}

pub struct ListSyncedEmailsTool {
    pub db: Arc<Database>,
    pub account_id: String,
}

#[async_trait]
impl AgentTool for ListSyncedEmailsTool {
    fn name(&self) -> &str {
        "list_synced_emails"
    }

    fn description(&self) -> &str {
        "List active synced emails currently stored in Postgres with safe metadata and current analysis status. Use this for database row, search, sample, or current prepared-data questions."
    }

    fn schema(&self) -> Value {
        list_synced_emails_schema()
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;
        let offset = args
            .get("offset")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            .min(1_000) as usize;

        let page = self
            .db
            .list_emails_for_account(
                &self.account_id,
                EmailListFilters {
                    category: text_arg(&args, "category"),
                    email_type: text_arg(&args, "email_type"),
                    spam_status: text_arg(&args, "spam_status"),
                    search: text_arg(&args, "query"),
                    folder: text_arg(&args, "folder"),
                    sender: text_arg(&args, "sender"),
                    organization: text_arg(&args, "organization"),
                    since: None,
                },
                limit,
                offset,
            )
            .await
            .context("list_synced_emails list")?;

        let emails = page
            .emails
            .into_iter()
            .map(|email| {
                json!({
                    "message_id": email.message_id,
                    "subject": email.subject,
                    "sender": email.sender,
                    "received_date": email.received_date.map(|date| date.to_rfc3339()),
                    "folder": email.location,
                    "category": email.category,
                    "email_type": email.email_type,
                    "organization": email.organization,
                    "topic": email.topic,
                    "summary": email.human_summary,
                    "analysis_status": if email.analyzed_at.is_some() { "analyzed" } else { "pending" },
                    "body_status": if email.body_synced_at.is_some() || email.body_text.is_some() || email.raw_email_content.is_some() {
                        "available"
                    } else {
                        "pending"
                    },
                    "threat_level": email.threat_level,
                    "spam_status": email.spam_status,
                    "phishing_status": email.phishing_status,
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "total_count": page.total_count,
            "count_scope": "active synced emails currently stored in Postgres",
            "order": "received_date desc nulls last, created_at desc, message_id desc",
            "limit": limit,
            "offset": offset,
            "filters": {
                "query": text_arg(&args, "query"),
                "sender": text_arg(&args, "sender"),
                "organization": text_arg(&args, "organization"),
                "category": text_arg(&args, "category"),
                "email_type": text_arg(&args, "email_type"),
                "spam_status": text_arg(&args, "spam_status"),
                "folder": text_arg(&args, "folder"),
            },
            "emails": emails,
            "preparation_note": "Rows may have pending classification, summaries, or filing recommendations while core continues processing.",
        });
        serde_json::to_string_pretty(&payload).context("serialize synced emails")
    }
}

pub fn build_analysis_tools(
    rag: Arc<RAGContextBuilder>,
    account_id: impl Into<String>,
) -> ToolRegistry {
    let account_id = account_id.into();
    let mut registry = ToolRegistry::new();
    registry.register(RagTool {
        name: "get_thread_context",
        description: "Retrieve other emails in the same thread by message_id.",
        schema: thread_context_schema(),
        rag: rag.clone(),
        account_id: account_id.clone(),
    });
    registry.register(RagTool {
        name: "get_sender_history",
        description: "Retrieve past emails and prior classifications for a sender address.",
        schema: sender_history_schema(),
        rag: rag.clone(),
        account_id: account_id.clone(),
    });
    registry.register(RagTool {
        name: "search_similar_emails",
        description: "Semantic search for similar emails already classified in this account.",
        schema: search_similar_emails_schema(),
        rag,
        account_id,
    });
    registry
}

pub fn build_digest_tools(
    rag: Arc<RAGContextBuilder>,
    db: Arc<Database>,
    account_id: impl Into<String>,
) -> ToolRegistry {
    let account_id = account_id.into();
    let mut registry = build_analysis_tools(rag, account_id.clone());
    registry.register(GetDigestStatsTool { db, account_id });
    registry
}

pub fn build_mail_assistant_tools(
    db: Arc<Database>,
    rag: Arc<RAGContextBuilder>,
    account_id: impl Into<String>,
) -> ToolRegistry {
    let account_id = account_id.into();
    let mut registry = build_analysis_tools(rag, account_id.clone());
    registry.register(GetDigestStatsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetAccountEmailStatsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(CountEmailsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(ListSyncedEmailsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(ListSubfoldersTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderTreeTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderEmailsSummaryTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderEmailSamplesTool { db, account_id });
    registry
}

pub fn build_location_tools(
    db: Arc<Database>,
    rag: Option<Arc<RAGContextBuilder>>,
    account_id: impl Into<String>,
) -> ToolRegistry {
    let account_id = account_id.into();
    let mut registry = ToolRegistry::new();
    if let Some(rag) = rag {
        registry.register(RagTool {
            name: "get_thread_context",
            description: "Retrieve other emails in the same thread by message_id.",
            schema: thread_context_schema(),
            rag: rag.clone(),
            account_id: account_id.clone(),
        });
        registry.register(RagTool {
            name: "get_sender_history",
            description: "Retrieve past emails, prior classifications, and folder locations for a sender address.",
            schema: sender_history_schema(),
            rag: rag.clone(),
            account_id: account_id.clone(),
        });
        registry.register(RagTool {
            name: "search_similar_emails",
            description:
                "Semantic search for similar emails already classified and filed in this account.",
            schema: search_similar_emails_schema(),
            rag,
            account_id: account_id.clone(),
        });
    }
    registry.register(ListSubfoldersTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderTreeTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderEmailsSummaryTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetFolderEmailSamplesTool { db, account_id });
    registry
}

pub fn build_orchestrator_tools(
    db: Arc<Database>,
    rag: Arc<RAGContextBuilder>,
    account_id: impl Into<String>,
) -> ToolRegistry {
    let account_id = account_id.into();
    let mut registry = ToolRegistry::new();
    registry.register(RagTool {
        name: "get_thread_context",
        description: "Retrieve other emails in the same thread by message_id.",
        schema: thread_context_schema(),
        rag: rag.clone(),
        account_id: account_id.clone(),
    });
    registry.register(RagTool {
        name: "get_sender_history",
        description: "Retrieve past emails and prior classifications for a sender address.",
        schema: sender_history_schema(),
        rag: rag.clone(),
        account_id: account_id.clone(),
    });
    registry.register(RagTool {
        name: "search_similar_emails",
        description: "Semantic search for similar emails already classified in this account.",
        schema: search_similar_emails_schema(),
        rag,
        account_id: account_id.clone(),
    });
    registry.register(GetBatchResultsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetScratchpadStatsTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(ReadWorkerScratchpadTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(FlagForReanalysisTool {
        db: db.clone(),
        account_id: account_id.clone(),
    });
    registry.register(GetAccountEmailStatsTool { db, account_id });
    registry
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn thread_context_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message_ids": { "type": "array", "items": { "type": "string" } },
            "include_full_body": { "type": "boolean" }
        },
        "required": ["message_ids"]
    })
}

fn sender_history_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sender": { "type": "string" },
            "limit": { "type": "integer", "default": 20 },
            "exclude_message_id": { "type": "string" }
        },
        "required": ["sender"]
    })
}

fn search_similar_emails_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" },
            "limit": { "type": "integer", "default": 10 },
            "max_distance": { "type": "number", "default": 0.35 },
            "sender": { "type": "string" },
            "category": { "type": "string" },
            "email_type": { "type": "string" },
            "organization": { "type": "string" },
            "list_id": { "type": "string" },
            "exclude_message_id": { "type": "string" }
        },
        "required": ["query"]
    })
}

fn count_emails_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sender": {
                "type": "string",
                "description": "Case-insensitive partial sender address/name filter. Use this for 'emails from X'."
            },
            "query": {
                "type": "string",
                "description": "Case-insensitive text search across message id, subject, sender, summary, organization, topic, and body."
            },
            "organization": {
                "type": "string",
                "description": "Case-insensitive partial organization filter."
            },
            "category": { "type": "string" },
            "email_type": { "type": "string" },
            "spam_status": {
                "type": "string",
                "description": "Spam classification filter, usually `spam` or `not-spam`."
            },
            "folder": {
                "type": "string",
                "description": "Current IMAP folder/location path."
            },
            "since": {
                "type": "string",
                "description": "Optional ISO-8601 datetime for a lower received-date bound."
            },
            "sample_limit": {
                "type": "integer",
                "description": "Number of recent matching email samples to return (default 5, max 10)."
            }
        }
    })
}

fn list_synced_emails_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "description": "Number of synced email rows to return (default 5, max 10)."
            },
            "offset": {
                "type": "integer",
                "description": "Offset into the current synced email list (default 0)."
            },
            "query": {
                "type": "string",
                "description": "Optional case-insensitive text search across message id, subject, sender, summary, organization, topic, and body."
            },
            "sender": {
                "type": "string",
                "description": "Optional case-insensitive partial sender address/name filter."
            },
            "organization": {
                "type": "string",
                "description": "Optional case-insensitive partial organization filter."
            },
            "category": { "type": "string" },
            "email_type": { "type": "string" },
            "spam_status": {
                "type": "string",
                "description": "Spam classification filter, usually `spam` or `not-spam`."
            },
            "folder": {
                "type": "string",
                "description": "Optional current IMAP folder/location path."
            }
        }
    })
}

fn list_subfolders_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        }
    })
}

fn get_folder_emails_summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "limit": { "type": "integer", "default": 10 }
        },
        "required": ["path"]
    })
}

fn get_folder_email_samples_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "folder_path": {
                "type": "string",
                "description": "Full folder path (e.g. Receipts/2023)"
            },
            "limit": {
                "type": "integer",
                "description": "Max emails to sample (default 5, max 10)"
            }
        },
        "required": ["folder_path"]
    })
}

fn get_digest_stats_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "since": {
                "type": "string",
                "description": "ISO-8601 datetime for window start"
            },
            "top_senders_limit": {
                "type": "integer",
                "description": "Max top senders to return (default 10)"
            }
        },
        "required": ["since"]
    })
}

fn get_batch_results_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "batch_id": { "type": "string" }
        },
        "required": ["batch_id"]
    })
}

fn get_scratchpad_stats_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_name": {
                "type": "string",
                "description": "Optional worker agent name filter"
            }
        }
    })
}

fn read_worker_scratchpad_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_name": { "type": "string" },
            "key": { "type": "string" }
        },
        "required": ["agent_name", "key"]
    })
}

fn flag_for_reanalysis_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message_id": { "type": "string" },
            "reason": { "type": "string" }
        },
        "required": ["message_id"]
    })
}

fn get_account_email_stats_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "since": {
                "type": "string",
                "description": "Optional ISO-8601 datetime window start"
            },
            "window_days": {
                "type": "integer",
                "description": "Fallback window in days when 'since' is omitted (default 30)"
            },
            "top_senders_limit": {
                "type": "integer",
                "description": "Max top senders to include (default 10)"
            }
        }
    })
}

fn format_folder_tree(tree: &[(String, bool)]) -> String {
    tree.iter()
        .map(|(path, noselect)| {
            if *noselect {
                format!("  {} [NOSELECT]", path)
            } else {
                format!("  {}", path)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn immediate_children(folders: &[ImapFolder], path: &str, delimiter: &str) -> Vec<(String, bool)> {
    let path_segments = path
        .split(delimiter)
        .filter(|segment| !segment.is_empty())
        .count();
    let path_prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{}{}", path.trim_end_matches(delimiter), delimiter)
    };

    folders
        .iter()
        .filter(|folder| {
            if path.is_empty() {
                folder
                    .folder_name
                    .split(delimiter)
                    .filter(|segment| !segment.is_empty())
                    .count()
                    == 1
            } else {
                folder.folder_name.starts_with(&path_prefix)
                    && folder.folder_name != path
                    && folder
                        .folder_name
                        .split(delimiter)
                        .filter(|segment| !segment.is_empty())
                        .count()
                        == path_segments + 1
            }
        })
        .map(|folder| (folder.folder_name.clone(), folder.is_noselect))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::db::Database;

    async fn load_test_database() -> Option<Arc<Database>> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())?;
        let db = Database::new(&url).await.ok()?;
        let _ = sqlx::raw_sql(include_str!("../../schema.sql"))
            .execute(&db.pool)
            .await;
        Some(Arc::new(db))
    }

    #[test]
    fn test_analysis_tool_schemas_are_json_objects() {
        let schemas = [
            thread_context_schema(),
            sender_history_schema(),
            search_similar_emails_schema(),
            count_emails_schema(),
        ];

        for schema in schemas {
            assert!(schema.is_object(), "schema should be an object");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_subfolders_returns_string() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping harness tool test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let tool = ListSubfoldersTool {
            db,
            account_id: crate::config::DEFAULT_ACCOUNT_ID.to_string(),
        };
        let output = tool
            .execute(json!({ "path": "" }))
            .await
            .expect("list subfolders");
        assert!(!output.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_orchestrator_tools_names() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping orchestrator tool test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let rag = Arc::new(RAGContextBuilder::new(db.clone()));
        let registry =
            build_orchestrator_tools(db, rag, crate::config::DEFAULT_ACCOUNT_ID.to_string());
        let mut names: Vec<String> = registry
            .as_completion_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        names.sort();

        assert_eq!(names.len(), 8);
        assert!(names.iter().any(|name| name == "get_batch_results"));
        assert!(names.iter().any(|name| name == "get_scratchpad_stats"));
        assert!(names.iter().any(|name| name == "read_worker_scratchpad"));
        assert!(names.iter().any(|name| name == "flag_for_reanalysis"));
        assert!(names.iter().any(|name| name == "get_account_email_stats"));
        assert!(names.iter().any(|name| name == "search_similar_emails"));
        assert!(names.iter().any(|name| name == "get_thread_context"));
        assert!(names.iter().any(|name| name == "get_sender_history"));
    }

    #[tokio::test]
    #[ignore]
    async fn test_mail_assistant_tools_names() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping mail assistant tool test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let rag = Arc::new(RAGContextBuilder::new(db.clone()));
        let registry =
            build_mail_assistant_tools(db, rag, crate::config::DEFAULT_ACCOUNT_ID.to_string());
        let mut names: Vec<String> = registry
            .as_completion_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        names.sort();

        assert!(names.iter().any(|name| name == "get_account_email_stats"));
        assert!(names.iter().any(|name| name == "count_emails"));
        assert!(names.iter().any(|name| name == "list_synced_emails"));
        assert!(names.iter().any(|name| name == "get_digest_stats"));
        assert!(names.iter().any(|name| name == "get_folder_tree"));
        assert!(names.iter().any(|name| name == "get_folder_emails_summary"));
        assert!(names.iter().any(|name| name == "get_folder_email_samples"));
        assert!(names.iter().any(|name| name == "list_subfolders"));
        assert!(names.iter().any(|name| name == "search_similar_emails"));
        assert!(names.iter().any(|name| name == "get_thread_context"));
        assert!(names.iter().any(|name| name == "get_sender_history"));
    }
}
