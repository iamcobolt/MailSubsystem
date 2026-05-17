use std::{str::FromStr, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::agent_catalog::{self, AgentCatalogEntry};
use crate::db::{self, AgentRunStatus};

use super::{
    api_error::{ApiError, ApiJsonResult},
    state::ApiState,
};

const DEFAULT_EMAIL_LIMIT: usize = 50;
const MAX_EMAIL_LIMIT: usize = 200;
const DEFAULT_RUN_LIMIT: usize = 20;
const MAX_RUN_LIMIT: usize = 100;
const DEFAULT_STATS_WINDOW_DAYS: i64 = 30;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EmailsQuery {
    pub category: Option<String>,
    pub email_type: Option<String>,
    pub spam_status: Option<String>,
    pub search: Option<String>,
    pub folder: Option<String>,
    pub sender: Option<String>,
    pub organization: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RunsQuery {
    pub agent: Option<String>,
    pub status: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StatsQuery {
    pub since: Option<DateTime<Utc>>,
    pub window_days: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListEmailsResponse {
    pub emails: Vec<db::EmailRecord>,
    pub limit: usize,
    pub offset: usize,
    pub total_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListRunsResponse {
    pub runs: Vec<db::AgentRunSummary>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderNode {
    pub name: String,
    pub message_count: i32,
    pub children: Vec<FolderNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardStatsResponse {
    pub since: DateTime<Utc>,
    pub window_days: i64,
    pub total_emails: i64,
    pub analyzed_count: i64,
    pub filed_count: i64,
    pub inbox_remaining: i64,
    pub spam_count: i64,
    pub phishing_count: i64,
    pub folder_count: i64,
    pub window: db::DigestWindowStats,
}

pub async fn list_agents() -> ApiJsonResult<Vec<AgentCatalogEntry>> {
    Ok(Json(agent_catalog::user_facing_agents(
        agent_catalog::direct_subagent_chat_enabled(),
    )))
}

pub async fn list_emails(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<EmailsQuery>,
) -> ApiJsonResult<ListEmailsResponse> {
    let limit = normalize_limit(query.limit, DEFAULT_EMAIL_LIMIT, MAX_EMAIL_LIMIT);
    let offset = normalize_offset(query.offset);

    let page = state
        .db
        .list_emails_for_account(
            &state.account_id,
            db::EmailListFilters {
                category: query.category.as_deref(),
                email_type: query.email_type.as_deref(),
                spam_status: query.spam_status.as_deref(),
                search: query.search.as_deref(),
                folder: query.folder.as_deref(),
                sender: query.sender.as_deref(),
                organization: query.organization.as_deref(),
                since: query.since,
            },
            limit,
            offset,
        )
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(ListEmailsResponse {
        emails: page
            .emails
            .into_iter()
            .map(sanitize_email_for_list)
            .collect(),
        limit,
        offset,
        total_count: page.total_count,
    }))
}

pub async fn get_email(
    State(state): State<Arc<ApiState>>,
    Path(message_id): Path<String>,
) -> ApiJsonResult<db::EmailRecord> {
    let email = state
        .db
        .get_active_email_by_message_id_for_account(&state.account_id, &message_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("email '{}' not found", message_id)))?;

    Ok(Json(email))
}

pub async fn list_folders(State(state): State<Arc<ApiState>>) -> ApiJsonResult<Vec<FolderNode>> {
    let folders = state
        .db
        .list_imap_folders_for_account(&state.account_id)
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(build_folder_tree(&folders)))
}

pub async fn list_runs(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<RunsQuery>,
) -> ApiJsonResult<ListRunsResponse> {
    let status = query
        .status
        .as_deref()
        .map(AgentRunStatus::from_str)
        .transpose()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let limit = normalize_limit(query.limit, DEFAULT_RUN_LIMIT, MAX_RUN_LIMIT);

    let runs = state
        .db
        .list_agent_runs_for_account(&state.account_id, limit, status, query.agent.as_deref())
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(ListRunsResponse { runs, limit }))
}

pub async fn get_run(
    State(state): State<Arc<ApiState>>,
    Path(run_id): Path<String>,
) -> ApiJsonResult<db::AgentRunDetail> {
    let run = state
        .db
        .get_agent_run_for_account(&state.account_id, &run_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("run '{}' not found", run_id)))?;

    Ok(Json(run))
}

pub async fn get_stats(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<StatsQuery>,
) -> ApiJsonResult<DashboardStatsResponse> {
    let (since, window_days) = resolve_stats_window(query.since, query.window_days)?;

    let window = state
        .db
        .get_email_stats_for_window_for_account(&state.account_id, since)
        .await
        .map_err(ApiError::internal)?;

    let summary_row = sqlx::query(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE deleted_from_server_at IS NULL)::bigint AS total_emails,
            COUNT(*) FILTER (
                WHERE deleted_from_server_at IS NULL
                  AND analyzed_at IS NOT NULL
            )::bigint AS analyzed_count,
            COUNT(*) FILTER (
                WHERE deleted_from_server_at IS NULL
                  AND LOWER(COALESCE(location, '')) = 'inbox'
            )::bigint AS inbox_remaining
        FROM emails
        WHERE account_id = $1
        "#,
    )
    .bind(&state.account_id)
    .fetch_one(&state.db.pool)
    .await
    .context("query dashboard email summary")
    .map_err(ApiError::internal)?;

    let folder_row = sqlx::query(
        r#"
        SELECT COUNT(*)::bigint AS folder_count
        FROM imap_folders
        WHERE account_id = $1
        "#,
    )
    .bind(&state.account_id)
    .fetch_one(&state.db.pool)
    .await
    .context("query folder count")
    .map_err(ApiError::internal)?;

    Ok(Json(DashboardStatsResponse {
        since,
        window_days,
        total_emails: summary_row.get("total_emails"),
        analyzed_count: summary_row.get("analyzed_count"),
        filed_count: window.filed_count,
        inbox_remaining: summary_row.get("inbox_remaining"),
        spam_count: window.spam_count,
        phishing_count: window.phishing_count,
        folder_count: folder_row.get("folder_count"),
        window,
    }))
}

fn normalize_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

fn normalize_offset(offset: Option<usize>) -> usize {
    offset.unwrap_or(0).clamp(0, 1_000_000)
}

fn sanitize_email_for_list(mut email: db::EmailRecord) -> db::EmailRecord {
    email.raw_email_content = None;
    email.body_text = None;
    email
}

fn resolve_stats_window(
    since: Option<DateTime<Utc>>,
    window_days: Option<i64>,
) -> Result<(DateTime<Utc>, i64), ApiError> {
    let now = Utc::now();

    if let Some(since) = since {
        if since > now {
            return Err(ApiError::bad_request(
                "stats 'since' must not be in the future",
            ));
        }

        if window_days.is_some_and(|days| days <= 0) {
            return Err(ApiError::bad_request(
                "stats 'window_days' must be a positive integer",
            ));
        }

        let inferred_days = (now - since).num_days().max(1);
        return Ok((since, inferred_days));
    }

    let window_days = window_days.unwrap_or(DEFAULT_STATS_WINDOW_DAYS);
    if window_days <= 0 {
        return Err(ApiError::bad_request(
            "stats 'window_days' must be a positive integer",
        ));
    }

    Ok((now - Duration::days(window_days), window_days))
}

fn build_folder_tree(folders: &[db::ImapFolder]) -> Vec<FolderNode> {
    let mut roots = Vec::new();

    for folder in folders {
        let full_paths = folder_full_paths(folder);
        if full_paths.is_empty() {
            continue;
        }
        insert_folder_path(
            &mut roots,
            &full_paths,
            folder.message_count.unwrap_or(0),
            0,
        );
    }

    sort_folder_nodes(&mut roots);
    roots
}

fn folder_full_paths(folder: &db::ImapFolder) -> Vec<String> {
    let delimiter = folder.delimiter.as_deref().unwrap_or("/");
    let mut full_paths = Vec::new();
    let mut current = String::new();

    for segment in folder
        .folder_name
        .split(delimiter)
        .filter(|segment| !segment.is_empty())
    {
        if current.is_empty() {
            current = segment.to_string();
        } else {
            current = format!("{}{}{}", current, delimiter, segment);
        }
        full_paths.push(current.clone());
    }

    if full_paths.is_empty() && !folder.folder_name.is_empty() {
        full_paths.push(folder.folder_name.clone());
    }

    full_paths
}

fn insert_folder_path(
    nodes: &mut Vec<FolderNode>,
    full_paths: &[String],
    message_count: i32,
    depth: usize,
) {
    let name = &full_paths[depth];
    let index = nodes
        .iter()
        .position(|node| node.name == *name)
        .unwrap_or_else(|| {
            nodes.push(FolderNode {
                name: name.clone(),
                message_count: 0,
                children: Vec::new(),
            });
            nodes.len() - 1
        });

    if depth + 1 == full_paths.len() {
        nodes[index].message_count = message_count;
        return;
    }

    insert_folder_path(
        &mut nodes[index].children,
        full_paths,
        message_count,
        depth + 1,
    );
}

fn sort_folder_nodes(nodes: &mut [FolderNode]) {
    nodes.sort_by(|left, right| left.name.cmp(&right.name));
    for node in nodes {
        sort_folder_nodes(&mut node.children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_email() -> db::EmailRecord {
        db::EmailRecord {
            message_id: "msg-1".to_string(),
            subject: Some("Quarterly update".to_string()),
            sender: Some("team@example.com".to_string()),
            received_date: Some(Utc::now()),
            spam_status: "not-spam".to_string(),
            phishing_status: "not-phishing".to_string(),
            marketing_status: "not-marketing".to_string(),
            otp_status: None,
            otp_code: None,
            otp_expires: None,
            threat_level: None,
            threat_indicators: None,
            uid: Some(10),
            uid_validity: Some(20),
            modseq: None,
            ai_summary: Some(json!({"summary": "hello"})),
            human_summary: Some("Summary".to_string()),
            category: Some("work".to_string()),
            subcategory: None,
            organization: Some("Example".to_string()),
            subject_area: None,
            topic: Some("Update".to_string()),
            location: Some("INBOX".to_string()),
            location_recommendation: None,
            offer_expires: None,
            flag_color: None,
            imap_flag_color: None,
            imap_flag_color_updated_at: None,
            llm_recommended_flag_color: None,
            llm_flag_color_updated_at: None,
            related_message_ids: vec!["msg-0".to_string()],
            email_type: Some("notification".to_string()),
            is_read: false,
            raw_email_content: Some("raw mime".to_string()),
            body_text: Some("full body".to_string()),
            body_synced_at: None,
            message_size: Some(1200),
            message_tokens: Some(300),
            analyzed_at: Some(Utc::now()),
            action_status: None,
            action_applied_at: None,
            analysis_attempts: 1,
            analysis_failed_at: None,
            analysis_permanent_failure: false,
            last_analysis_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn sanitize_email_for_list_omits_large_fields_from_json() {
        let email = sanitize_email_for_list(sample_email());
        let value = serde_json::to_value(&email).expect("serialize email");
        assert!(value.get("raw_email_content").is_none());
        assert!(value.get("body_text").is_none());
    }

    #[test]
    fn resolve_stats_window_defaults_to_30_days() {
        let (since, days) = resolve_stats_window(None, None).expect("resolve default window");
        assert_eq!(days, 30);
        assert!((Utc::now() - since).num_days() >= 29);
    }

    #[test]
    fn resolve_stats_window_uses_since_as_the_reported_window() {
        let since = Utc::now() - Duration::days(14);
        let (_, days) =
            resolve_stats_window(Some(since), Some(7)).expect("resolve explicit since window");
        assert_eq!(days, 14);
    }

    #[test]
    fn build_folder_tree_creates_nested_nodes() {
        let folders = vec![
            db::ImapFolder {
                folder_name: "Work".to_string(),
                delimiter: Some("/".to_string()),
                is_noselect: false,
                attributes: Vec::new(),
                last_synced_uid: None,
                last_full_sync_uid: None,
                message_count: Some(45),
                priority: None,
            },
            db::ImapFolder {
                folder_name: "Work/LinkedIn".to_string(),
                delimiter: Some("/".to_string()),
                is_noselect: false,
                attributes: Vec::new(),
                last_synced_uid: None,
                last_full_sync_uid: None,
                message_count: Some(12),
                priority: None,
            },
            db::ImapFolder {
                folder_name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                is_noselect: false,
                attributes: Vec::new(),
                last_synced_uid: None,
                last_full_sync_uid: None,
                message_count: Some(3),
                priority: None,
            },
        ];

        let tree = build_folder_tree(&folders);
        assert_eq!(
            tree,
            vec![
                FolderNode {
                    name: "INBOX".to_string(),
                    message_count: 3,
                    children: Vec::new(),
                },
                FolderNode {
                    name: "Work".to_string(),
                    message_count: 45,
                    children: vec![FolderNode {
                        name: "Work/LinkedIn".to_string(),
                        message_count: 12,
                        children: Vec::new(),
                    }],
                },
            ]
        );
    }
}
