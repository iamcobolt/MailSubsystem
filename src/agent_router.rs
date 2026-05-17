use crate::{
    agent_catalog,
    db::{ConversationMessage, EmailRecord},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    pub visible_agent_name: String,
    pub execution_agent_name: String,
    pub routing_reason: String,
}

impl ExecutionPlan {
    fn new(
        visible_agent_name: &str,
        execution_agent_name: &str,
        routing_reason: impl Into<String>,
    ) -> Self {
        Self {
            visible_agent_name: visible_agent_name.to_string(),
            execution_agent_name: execution_agent_name.to_string(),
            routing_reason: routing_reason.into(),
        }
    }
}

pub fn resolve_execution_plan(
    visible_agent_name: &str,
    user_message: &str,
    history: &[ConversationMessage],
    context_email: Option<&EmailRecord>,
) -> ExecutionPlan {
    if !visible_agent_name.eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID) {
        return ExecutionPlan::new(
            visible_agent_name,
            visible_agent_name,
            "Direct specialist thread keeps the selected agent unchanged.",
        );
    }

    route_mail_assistant_turn(user_message, history, context_email)
}

pub fn infer_digest_window(user_message: &str, history: &[ConversationMessage]) -> &'static str {
    let current_text = normalize_routing_text(user_message);
    if is_daily_digest_window(&current_text) {
        "daily"
    } else if is_weekly_digest_window(&current_text) {
        "weekly"
    } else if should_use_history_for_routing(&current_text) {
        let history_text = build_recent_user_history_text(history);
        if is_daily_digest_window(&history_text) {
            "daily"
        } else {
            "weekly"
        }
    } else {
        "weekly"
    }
}

fn route_mail_assistant_turn(
    user_message: &str,
    history: &[ConversationMessage],
    context_email: Option<&EmailRecord>,
) -> ExecutionPlan {
    let current_text = normalize_routing_text(user_message);
    let has_context_email = context_email.is_some();
    if let Some(plan) = route_mail_assistant_text(&current_text, has_context_email) {
        return plan;
    }

    if should_use_history_for_routing(&current_text) {
        let history_text = build_recent_user_history_text(history);
        if let Some(plan) = route_mail_assistant_text(&history_text, has_context_email) {
            return ExecutionPlan::new(
                agent_catalog::DEFAULT_AGENT_ID,
                &plan.execution_agent_name,
                format!(
                    "Continued earlier Mail Assistant context. {}",
                    plan.routing_reason
                ),
            );
        }
    }

    ExecutionPlan::new(
        agent_catalog::DEFAULT_AGENT_ID,
        agent_catalog::DEFAULT_AGENT_ID,
        "Handled directly by Mail Assistant for a broad conversational turn.",
    )
}

fn normalize_routing_text(text: &str) -> String {
    text.trim().to_ascii_lowercase()
}

fn build_recent_user_history_text(history: &[ConversationMessage]) -> String {
    let recent_user_turns: Vec<_> = history
        .iter()
        .rev()
        .filter(|message| message.role.eq_ignore_ascii_case("user"))
        .take(2)
        .map(|message| normalize_routing_text(&message.content))
        .collect();
    let mut parts = Vec::with_capacity(recent_user_turns.len());
    for turn in recent_user_turns.into_iter().rev() {
        if !turn.is_empty() {
            parts.push(turn);
        }
    }
    parts.join("\n")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn route_mail_assistant_text(routing_text: &str, has_context_email: bool) -> Option<ExecutionPlan> {
    if routing_text.is_empty() {
        return None;
    }

    if has_context_email && is_explicit_escalation_question(routing_text) {
        return Some(ExecutionPlan::new(
            agent_catalog::DEFAULT_AGENT_ID,
            "orchestrator",
            "Routed to Orchestrator for an explicit higher-judgment review request.",
        ));
    }

    if has_context_email && is_single_email_analysis_question(routing_text) {
        return Some(ExecutionPlan::new(
            agent_catalog::DEFAULT_AGENT_ID,
            "email-analyzer",
            "Routed to Email Analyzer for a single-email explanation or safety question.",
        ));
    }

    if has_context_email && is_filing_question(routing_text) {
        return Some(ExecutionPlan::new(
            agent_catalog::DEFAULT_AGENT_ID,
            "location-agent",
            "Routed to Location Agent for a folder-placement question about the attached email.",
        ));
    }

    if is_digest_question(routing_text) {
        return Some(ExecutionPlan::new(
            agent_catalog::DEFAULT_AGENT_ID,
            "digest-agent",
            "Routed to Digest Agent for a digest, recap, or inbox-trend question.",
        ));
    }

    if is_folder_consolidation_question(routing_text) {
        return Some(ExecutionPlan::new(
            agent_catalog::DEFAULT_AGENT_ID,
            "folder-consolidator",
            "Routed to Folder Consolidator for mailbox cleanup or folder-structure guidance.",
        ));
    }

    None
}

fn should_use_history_for_routing(current_text: &str) -> bool {
    contains_any(
        current_text,
        &[
            "what else",
            "anything else",
            "something else",
            "tell me more",
            "more detail",
            "go deeper",
            "keep going",
            "continue",
            "elaborate",
            "expand on that",
            "expand on this",
            "what about",
            "and what about",
            "what next",
            "stands out",
        ],
    ) || (current_text.ends_with('?')
        && contains_any(
            current_text,
            &[
                " this", " that", " it", " them", " those", " these", " else", " more", " next",
            ],
        ))
}

fn is_daily_digest_window(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "today",
            "today's",
            "daily",
            "yesterday",
            "last 24 hours",
            "past 24 hours",
            "this morning",
            "since this morning",
        ],
    )
}

fn is_weekly_digest_window(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "weekly",
            "this week",
            "last week",
            "past week",
            "last 7 days",
            "past 7 days",
            "last seven days",
            "past seven days",
        ],
    )
}

fn is_digest_question(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "digest",
            "recap",
            "top senders",
            "what changed recently",
            "what changed this week",
            "what changed today",
            "inbox summary",
            "summarize my inbox",
            "mailbox summary",
            "weekly summary",
            "daily summary",
            "inbox trend",
            "inbox trends",
            "trend in my inbox",
            "summarize the last week",
        ],
    )
}

fn is_folder_consolidation_question(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "consolidate folders",
            "consolidation",
            "folder cleanup",
            "clean up my folders",
            "clean up folders",
            "duplicate folders",
            "redundant folders",
            "merge folders",
            "reorganize folders",
            "reorganise folders",
            "folder structure",
            "too many folders",
            "folder tree",
        ],
    )
}

fn is_explicit_escalation_question(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "second opinion",
            "escalate this",
            "careful review",
            "review this carefully",
            "higher judgment",
            "high-judgment",
            "adjudicate",
            "conflicting signals",
            "conflict here",
        ],
    )
}

fn is_single_email_analysis_question(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "recap this email",
            "recap this message",
            "recap this attached email",
            "give me a recap of this email",
            "give me a recap of this message",
            "summarize this email",
            "summarise this email",
            "summarize this message",
            "summarise this message",
            "explain this email",
            "explain this message",
            "review this email",
            "review this message",
            "what does this email mean",
            "what does this mean",
            "what action is needed",
            "what should i do",
            "do i need to do anything",
            "is this safe",
            "is this email safe",
            "is this phishing",
            "is this email phishing",
            "is this spam",
            "is this email spam",
            "is this legit",
            "is this email legit",
            "is this legitimate",
            "is this email legitimate",
            "is this risky",
            "is this suspicious",
            "should i trust this",
            "should i reply",
            "safe to reply",
            "important or routine",
            "what kind of email is this",
        ],
    )
}

fn is_filing_question(routing_text: &str) -> bool {
    contains_any(
        routing_text,
        &[
            "where should this be filed",
            "where should this email be filed",
            "where should this go",
            "where should this email go",
            "where does this belong",
            "where does this email belong",
            "which folder should this go in",
            "which folder does this belong in",
            "which folder should this email go in",
            "which folder does this email belong in",
            "file this",
            "folder recommendation",
            "place this email",
            "move this email",
            "file this email",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample_email() -> EmailRecord {
        EmailRecord {
            message_id: "msg-1".to_string(),
            subject: Some("Quarterly update".to_string()),
            sender: Some("updates@example.com".to_string()),
            received_date: Some(Utc.with_ymd_and_hms(2026, 4, 18, 12, 0, 0).unwrap()),
            spam_status: "not-spam".to_string(),
            phishing_status: "not-phishing".to_string(),
            marketing_status: "not-marketing".to_string(),
            otp_status: Some("not-otp".to_string()),
            otp_code: None,
            otp_expires: None,
            threat_level: Some("none".to_string()),
            threat_indicators: None,
            uid: None,
            uid_validity: None,
            modseq: None,
            ai_summary: None,
            human_summary: Some("A normal update".to_string()),
            category: Some("work".to_string()),
            subcategory: Some("project".to_string()),
            organization: Some("Example".to_string()),
            subject_area: None,
            topic: Some("Roadmap".to_string()),
            location: Some("Work/Example".to_string()),
            location_recommendation: None,
            offer_expires: None,
            flag_color: None,
            imap_flag_color: None,
            imap_flag_color_updated_at: None,
            llm_recommended_flag_color: None,
            llm_flag_color_updated_at: None,
            related_message_ids: vec!["thread-a".to_string()],
            email_type: Some("announcement".to_string()),
            is_read: false,
            raw_email_content: Some("Body".to_string()),
            body_text: Some("Body".to_string()),
            body_synced_at: None,
            message_size: Some(512),
            message_tokens: None,
            analyzed_at: None,
            action_status: None,
            action_applied_at: None,
            analysis_attempts: 0,
            analysis_failed_at: None,
            analysis_permanent_failure: false,
            last_analysis_error: None,
            created_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 0, 0).unwrap(),
        }
    }

    fn sample_history(message: &str) -> Vec<ConversationMessage> {
        vec![ConversationMessage {
            message_id: "m-1".to_string(),
            thread_id: "thread-1".to_string(),
            role: "user".to_string(),
            content: message.to_string(),
            agent_name: None,
            agent_run_id: None,
            created_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 1, 0).unwrap(),
        }]
    }

    #[test]
    fn direct_specialist_threads_stay_direct() {
        let plan = resolve_execution_plan("digest-agent", "What changed this week?", &[], None);
        assert_eq!(plan.visible_agent_name, "digest-agent");
        assert_eq!(plan.execution_agent_name, "digest-agent");
    }

    #[test]
    fn mail_assistant_routes_single_email_review_to_email_analyzer() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Is this email phishing or safe to reply to?",
            &[],
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "email-analyzer");
    }

    #[test]
    fn mail_assistant_routes_email_specific_recap_to_email_analyzer() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Recap this email for me.",
            &[],
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "email-analyzer");
    }

    #[test]
    fn mail_assistant_routes_folder_question_to_location_agent() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Which folder should this email go in?",
            &[],
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "location-agent");
    }

    #[test]
    fn mail_assistant_routes_digest_question_to_digest_agent() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Give me a weekly digest with top senders.",
            &[],
            None,
        );
        assert_eq!(plan.execution_agent_name, "digest-agent");
    }

    #[test]
    fn mail_assistant_keeps_mailbox_recap_on_digest_path_even_with_context_email() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Give me a weekly inbox recap.",
            &[],
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "digest-agent");
    }

    #[test]
    fn mail_assistant_routes_cleanup_question_to_folder_consolidator() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Help me clean up duplicate folders in this mailbox.",
            &[],
            None,
        );
        assert_eq!(plan.execution_agent_name, "folder-consolidator");
    }

    #[test]
    fn mail_assistant_routes_explicit_escalation_to_orchestrator() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Please give this a second opinion and review this carefully.",
            &[],
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "orchestrator");
    }

    #[test]
    fn mail_assistant_uses_history_for_follow_on_digest_turns() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "What else stands out?",
            &sample_history("Give me a weekly inbox recap."),
            None,
        );
        assert_eq!(plan.execution_agent_name, "digest-agent");
    }

    #[test]
    fn current_turn_overrides_digest_history_when_intent_changes() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Which folder should this email go in?",
            &sample_history("Give me a weekly inbox recap."),
            Some(&sample_email()),
        );
        assert_eq!(plan.execution_agent_name, "location-agent");
    }

    #[test]
    fn unrelated_turn_does_not_inherit_previous_digest_route() {
        let plan = resolve_execution_plan(
            agent_catalog::DEFAULT_AGENT_ID,
            "Thanks for the help.",
            &sample_history("Give me a weekly inbox recap."),
            None,
        );
        assert_eq!(plan.execution_agent_name, agent_catalog::DEFAULT_AGENT_ID);
    }

    #[test]
    fn digest_window_uses_daily_keywords() {
        assert_eq!(infer_digest_window("Give me today's digest.", &[]), "daily");
        assert_eq!(
            infer_digest_window(
                "What else stands out?",
                &sample_history("Give me a daily summary.")
            ),
            "daily"
        );
        assert_eq!(
            infer_digest_window(
                "Actually, make it a weekly digest.",
                &sample_history("Give me a daily summary.")
            ),
            "weekly"
        );
    }
}
