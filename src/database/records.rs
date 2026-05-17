use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailRecord {
    pub message_id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_date: Option<DateTime<Utc>>,
    pub spam_status: String,
    pub phishing_status: String,
    pub marketing_status: String,
    pub otp_status: Option<String>,
    pub otp_code: Option<String>,
    pub otp_expires: Option<DateTime<Utc>>,
    pub threat_level: Option<String>,
    pub threat_indicators: Option<Value>,
    pub uid: Option<i32>,
    pub uid_validity: Option<i32>,
    pub modseq: Option<i64>,
    pub ai_summary: Option<Value>,
    pub human_summary: Option<String>,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub organization: Option<String>,
    pub subject_area: Option<String>,
    pub topic: Option<String>,
    pub location: Option<String>,
    pub location_recommendation: Option<String>,
    pub offer_expires: Option<DateTime<Utc>>,
    pub flag_color: Option<String>,
    pub imap_flag_color: Option<String>,
    pub imap_flag_color_updated_at: Option<DateTime<Utc>>,
    pub llm_recommended_flag_color: Option<String>,
    pub llm_flag_color_updated_at: Option<DateTime<Utc>>,
    pub related_message_ids: Vec<String>,
    pub email_type: Option<String>,
    pub is_read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_email_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    pub body_synced_at: Option<DateTime<Utc>>,
    pub message_size: Option<i32>,
    pub message_tokens: Option<i32>,
    pub analyzed_at: Option<DateTime<Utc>>,
    pub action_status: Option<String>,
    pub action_applied_at: Option<DateTime<Utc>>,
    pub analysis_attempts: i32,
    pub analysis_failed_at: Option<DateTime<Utc>>,
    pub analysis_permanent_failure: bool,
    pub last_analysis_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoreEmailInput<'a> {
    pub message_id: &'a str,
    pub subject: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub received_date: Option<DateTime<Utc>>,
    pub ai_summary: Option<&'a Value>,
    pub location: Option<&'a str>,
    pub raw_email_content: Option<&'a str>,
    pub body_text: Option<&'a str>,
    pub modseq: Option<i64>,
    pub uid: Option<i32>,
    pub uid_validity: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub agent_name: String,
    pub title: Option<String>,
    pub context_email_id: Option<String>,
    pub message_count: i64,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMessage {
    pub message_id: String,
    pub thread_id: String,
    pub role: String,
    pub content: String,
    pub agent_name: Option<String>,
    pub agent_run_id: Option<String>,
    pub created_at: DateTime<Utc>,
}
