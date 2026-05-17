use chrono::Utc;
use serde_json::Value;
use sqlx::{postgres::PgRow, types::Json, Row};

use crate::db::{AgentRunSummary, ConversationMessage, EmailRecord, ThreadSummary};

pub(crate) fn json_column(row: &PgRow, column: &str) -> Option<Value> {
    row.try_get::<Json<Value>, _>(column)
        .ok()
        .map(|json| json.0)
}

pub(crate) fn json_column_or_default(row: &PgRow, column: &str, default: Value) -> Value {
    row.try_get::<Option<Json<Value>>, _>(column)
        .ok()
        .flatten()
        .map(|json| json.0)
        .unwrap_or(default)
}

pub(crate) fn email_record_from_row(row: &PgRow) -> EmailRecord {
    EmailRecord {
        message_id: row.get("message_id"),
        subject: row.get("subject"),
        sender: row.get("sender"),
        received_date: row.get("received_date"),
        spam_status: row
            .get::<Option<String>, _>("spam_status")
            .unwrap_or_else(|| "unknown".into()),
        phishing_status: row
            .get::<Option<String>, _>("phishing_status")
            .unwrap_or_else(|| "unknown".into()),
        marketing_status: row
            .get::<Option<String>, _>("marketing_status")
            .unwrap_or_else(|| "unknown".into()),
        otp_status: row.get("otp_status"),
        otp_code: row.try_get("otp_code").ok().flatten(),
        otp_expires: row.get("otp_expires"),
        threat_level: row.try_get("threat_level").ok().flatten(),
        threat_indicators: json_column(row, "threat_indicators"),
        uid: row.get("uid"),
        uid_validity: row.get("uid_validity"),
        modseq: row.try_get("modseq").ok().flatten(),
        ai_summary: json_column(row, "ai_summary"),
        human_summary: row.get("human_summary"),
        category: row.get("category"),
        subcategory: row.get("subcategory"),
        organization: row.get("organization"),
        subject_area: None,
        topic: row.get("topic"),
        location: row.get("location"),
        location_recommendation: row.try_get("location_recommendation").ok().flatten(),
        offer_expires: row.get("offer_expires"),
        flag_color: row.try_get("flag_color").ok().flatten(),
        imap_flag_color: row.try_get("imap_flag_color").ok().flatten(),
        imap_flag_color_updated_at: row.try_get("imap_flag_color_updated_at").ok().flatten(),
        llm_recommended_flag_color: row.try_get("llm_recommended_flag_color").ok().flatten(),
        llm_flag_color_updated_at: row.try_get("llm_flag_color_updated_at").ok().flatten(),
        related_message_ids: row
            .try_get::<Option<Vec<String>>, _>("related_message_ids")
            .ok()
            .flatten()
            .unwrap_or_default(),
        email_type: row.get("email_type"),
        is_read: row.try_get("is_read").ok().unwrap_or(false),
        raw_email_content: row.get("raw_email_content"),
        body_text: row.get("body_text"),
        body_synced_at: row.get("body_synced_at"),
        message_size: row.get("message_size"),
        message_tokens: row.get("message_tokens"),
        analyzed_at: row.try_get("analyzed_at").ok().flatten(),
        action_status: row.try_get("action_status").ok().flatten(),
        action_applied_at: row.try_get("action_applied_at").ok().flatten(),
        analysis_attempts: row
            .try_get::<Option<i32>, _>("analysis_attempts")
            .ok()
            .flatten()
            .unwrap_or(0),
        analysis_failed_at: row.try_get("analysis_failed_at").ok().flatten(),
        analysis_permanent_failure: row
            .try_get::<Option<bool>, _>("analysis_permanent_failure")
            .ok()
            .flatten()
            .unwrap_or(false),
        last_analysis_error: row.try_get("last_analysis_error").ok().flatten(),
        created_at: row.try_get("created_at").ok().unwrap_or_else(Utc::now),
        updated_at: row.try_get("updated_at").ok().unwrap_or_else(Utc::now),
    }
}

pub(crate) fn agent_run_summary_from_row(row: &PgRow) -> AgentRunSummary {
    AgentRunSummary {
        run_id: row.get("run_id"),
        agent_name: row.get("agent_name"),
        task_id: row.get("task_id"),
        status: row.get("status"),
        steps: row.get("steps"),
        llm_calls: row.get("llm_calls"),
        tool_calls: row.get("tool_calls"),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        duration_ms: row.get("duration_ms"),
        started_at: row.get("started_at"),
        error: row.get("error"),
        escalated: row
            .try_get::<Option<bool>, _>("escalated")
            .ok()
            .flatten()
            .unwrap_or(false),
    }
}

pub(crate) fn thread_summary_from_row(row: &PgRow) -> ThreadSummary {
    ThreadSummary {
        thread_id: row.get("thread_id"),
        agent_name: row.get("agent_name"),
        title: row.get("title"),
        context_email_id: row.get("context_email_id"),
        message_count: row.get("message_count"),
        last_message_at: row.get("last_message_at"),
        created_at: row.get("created_at"),
    }
}

pub(crate) fn conversation_message_from_row(row: &PgRow) -> ConversationMessage {
    ConversationMessage {
        message_id: row.get("message_id"),
        thread_id: row.get("thread_id"),
        role: row.get("role"),
        content: row.get("content"),
        agent_name: row.get("agent_name"),
        agent_run_id: row.get("agent_run_id"),
        created_at: row.get("created_at"),
    }
}
