use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::ai::{AICompletionRequest, AIProvider, Message, ToolCall, ToolChoice};
use crate::db::{CoreWorkType, Database, DbCompletenessSnapshot};

use super::spec::{AgentSpec, OutputConfig};
use super::state::AgentState;
use super::tools::ToolRegistry;

pub const DB_COMPLETENESS_PENDING_STATUS: &str = "prerequisites_pending";

const API_CHAT_TASK_PREFIX: &str = "api-chat-";
const DEFAULT_INTERACTIVE_DB_COMPLETENESS_MAX_WAIT_SECS: u64 = 15;

#[derive(Debug, Clone)]
pub struct RunResult {
    pub run_id: String,
    pub output: Value,
    pub should_escalate: bool,
    pub escalate_reason: Option<String>,
    pub llm_calls: u32,
    pub tool_calls: u32,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum HarnessEvent {
    RunStarted {
        run_id: String,
        task_id: String,
        agent_name: String,
    },
    StepStarted {
        step: usize,
    },
    ToolCall {
        step: usize,
        tool_name: String,
        arguments: Value,
    },
    ToolResult {
        step: usize,
        tool_name: String,
        result: String,
        latency_ms: u64,
    },
    FinalOutput {
        output: Value,
    },
    Error {
        message: String,
    },
}

pub type HarnessEventCallback = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

#[derive(Debug, Clone)]
struct DbCompletenessPollConfig {
    enabled: bool,
    request_work: bool,
    max_wait: Duration,
    interactive_max_wait: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    stable_polls_required: u32,
}

impl DbCompletenessPollConfig {
    fn from_env() -> Self {
        let enabled = std::env::var("AGENT_DB_COMPLETENESS_ENABLED")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        let mode = std::env::var("AGENT_DB_COMPLETENESS_MODE").unwrap_or_default();
        let request_work = std::env::var("AGENT_DB_COMPLETENESS_REQUEST_WORK")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| !mode.eq_ignore_ascii_case("passive"));
        let max_wait_secs = std::env::var("AGENT_DB_COMPLETENESS_MAX_WAIT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(120)
            .max(1);
        let interactive_max_wait_secs =
            std::env::var("AGENT_DB_COMPLETENESS_INTERACTIVE_MAX_WAIT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_INTERACTIVE_DB_COMPLETENESS_MAX_WAIT_SECS)
                .max(1)
                .min(max_wait_secs);
        let initial_backoff_ms = std::env::var("AGENT_DB_COMPLETENESS_INITIAL_BACKOFF_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(500)
            .max(50);
        let max_backoff_ms = std::env::var("AGENT_DB_COMPLETENESS_MAX_BACKOFF_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(5_000)
            .max(initial_backoff_ms);
        let stable_polls_required = std::env::var("AGENT_DB_COMPLETENESS_STABLE_POLLS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(2)
            .max(1);

        Self {
            enabled,
            request_work,
            max_wait: Duration::from_secs(max_wait_secs),
            interactive_max_wait: Duration::from_secs(interactive_max_wait_secs),
            initial_backoff: Duration::from_millis(initial_backoff_ms),
            max_backoff: Duration::from_millis(max_backoff_ms),
            stable_polls_required,
        }
    }

    fn max_wait_for_task(&self, task_id: &str) -> Duration {
        if should_complete_db_pending_for_task(task_id) {
            self.interactive_max_wait
        } else {
            self.max_wait
        }
    }
}

fn should_gate_db_completeness(agent_name: &str) -> bool {
    let normalized = agent_name.to_ascii_lowercase();
    normalized == "mail-assistant"
}

fn classify_db_prerequisite_work(snapshot: &DbCompletenessSnapshot) -> Vec<CoreWorkType> {
    let mut work = Vec::new();

    if snapshot.folder_count == 0 || snapshot.needs_full_sync_backfill() {
        work.push(CoreWorkType::SyncFull);
    }

    if snapshot.body_missing > 0
        || snapshot.body_sync.pending > 0
        || snapshot.body_sync.failed > 0
        || snapshot.body_sync.processing > 0
    {
        work.push(CoreWorkType::SyncBody);
    }

    if snapshot.analysis_missing > 0 {
        work.push(CoreWorkType::Analyze);
    }

    if snapshot.location_missing > 0 {
        work.push(CoreWorkType::Locate);
    }

    work
}

fn core_work_type_names(work: &[CoreWorkType]) -> Vec<&'static str> {
    work.iter().map(|work_type| work_type.as_str()).collect()
}

fn pending_retry_after_seconds(snapshot: &DbCompletenessSnapshot) -> u64 {
    let backlog = snapshot.body_missing
        + snapshot.analysis_missing
        + snapshot.location_missing
        + snapshot.body_sync.pending
        + snapshot.body_sync.failed
        + snapshot.body_sync.processing;

    if backlog > 100 {
        30
    } else if backlog > 0 {
        15
    } else {
        10
    }
}

fn should_complete_db_pending_for_task(task_id: &str) -> bool {
    task_id.starts_with(API_CHAT_TASK_PREFIX)
}

fn should_use_partial_db_for_interactive_mail_assistant(agent_name: &str, task_id: &str) -> bool {
    agent_name.eq_ignore_ascii_case("mail-assistant")
        && should_complete_db_pending_for_task(task_id)
}

fn mail_assistant_input_text(input: &Value) -> String {
    input
        .get("message")
        .or_else(|| input.get("user_message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn mail_assistant_recent_history_text(input: &Value, max_messages: usize) -> String {
    let Some(history) = input.get("conversation_history").and_then(Value::as_array) else {
        return String::new();
    };

    let mut parts = history
        .iter()
        .rev()
        .filter_map(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|content| !content.is_empty())
        })
        .take(max_messages)
        .map(|content| content.to_ascii_lowercase())
        .collect::<Vec<_>>();
    parts.reverse();
    parts.join("\n")
}

fn is_count_question_text(text: &str) -> bool {
    text.contains("count") || text.contains("how many") || text.contains("number of")
}

fn has_followup_reference(text: &str) -> bool {
    text.contains("those")
        || text.contains("them")
        || text.contains("that")
        || text.contains("these")
        || text.contains("the current list")
        || text.contains("identified")
}

fn is_mailbox_preparation_count_text(text: &str) -> bool {
    (text.contains("synced")
        || text.contains("indexed")
        || text.contains("imported")
        || text.contains("loaded")
        || text.contains("in the database")
        || text.contains("in database")
        || text.contains("database")
        || text.contains("prepared")
        || text.contains("preparing")
        || text.contains("progress")
        || text.contains("status")
        || text.contains("ready"))
        && (text.contains("how many")
            || text.contains("count")
            || text.contains("number of")
            || text.contains("message")
            || text.contains("email")
            || text.contains("mailbox"))
}

fn should_require_synced_email_list_tool(agent_name: &str, input: &Value) -> bool {
    if !agent_name.eq_ignore_ascii_case("mail-assistant") {
        return false;
    }

    let text = mail_assistant_input_text(input);
    if text.is_empty() {
        return false;
    }

    let asks_for_database_rows = (text.contains("first")
        || text.contains("top")
        || text.contains("show")
        || text.contains("list")
        || text.contains("tell me about"))
        && (text.contains("database")
            || text.contains("synced")
            || text.contains("indexed")
            || text.contains("emails")
            || text.contains("messages"));

    let asks_what_is_available = text.contains("what can you tell me")
        || text.contains("what can you answer")
        || text.contains("what do you know")
        || text.contains("tell me what you can")
        || text.contains("what is available");

    asks_for_database_rows || asks_what_is_available
}

fn synced_email_list_tool_args_for_input(input: &Value) -> Value {
    let message = input
        .get("message")
        .or_else(|| input.get("user_message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let mut args = serde_json::Map::new();
    args.insert(
        "limit".to_string(),
        json!(extract_requested_list_limit(message).unwrap_or(5)),
    );
    if let Some(spam_status) = extract_spam_status_filter_from_text(message) {
        args.insert(
            "spam_status".to_string(),
            Value::String(spam_status.to_string()),
        );
    }
    Value::Object(args)
}

fn extract_requested_list_limit(message: &str) -> Option<usize> {
    const WORD_LIMITS: [(&str, usize); 10] = [
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
    ];

    for word in message.split_whitespace() {
        let trimmed = word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
        if let Ok(value) = trimmed.parse::<usize>() {
            return Some(value.clamp(1, 10));
        }
        let lowered = trimmed.to_ascii_lowercase();
        if let Some((_, value)) = WORD_LIMITS.iter().find(|(word, _)| *word == lowered) {
            return Some(*value);
        }
    }

    None
}

fn should_require_count_emails_tool(agent_name: &str, input: &Value) -> bool {
    if !agent_name.eq_ignore_ascii_case("mail-assistant") {
        return false;
    }

    let text = mail_assistant_input_text(input);
    if is_mailbox_preparation_count_text(&text)
        || should_require_synced_email_list_tool(agent_name, input)
    {
        return false;
    }

    let asks_for_count = is_count_question_text(&text);
    let has_sender_filter = extract_sender_filter_from_count_text(&text).is_some();
    let recent_history = mail_assistant_recent_history_text(input, 4);
    let has_mail_context = recent_history.contains("email")
        || recent_history.contains("message")
        || recent_history.contains("mailbox")
        || recent_history.contains("spam");

    text.contains("emails from")
        || text.contains("email from")
        || text.contains("messages from")
        || text.contains("message from")
        || text.contains("mail from")
        || (asks_for_count && has_sender_filter && has_followup_reference(&text))
        || (asks_for_count && has_followup_reference(&text) && has_mail_context)
        || (asks_for_count && extract_spam_status_filter_from_text(&text).is_some())
        || (asks_for_count
            && (text.contains("email")
                || text.contains("message")
                || text.contains("mail")
                || text.contains("sender")
                || text.contains("inbox")))
}

fn count_emails_tool_args_for_input(input: &Value) -> Value {
    let message = input
        .get("message")
        .or_else(|| input.get("user_message"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let mut args = serde_json::Map::new();
    args.insert("sample_limit".to_string(), json!(5));

    if let Some(sender) = extract_sender_filter_from_count_text(message) {
        args.insert("sender".to_string(), Value::String(sender));
    }
    if let Some(spam_status) = extract_spam_status_filter_from_text(message) {
        args.insert(
            "spam_status".to_string(),
            Value::String(spam_status.to_string()),
        );
    } else if let Some(spam_status) = extract_contextual_spam_status_filter_from_input(input) {
        args.insert(
            "spam_status".to_string(),
            Value::String(spam_status.to_string()),
        );
    }

    let has_structured_filter = args.len() > 1;
    if !has_structured_filter && !is_unfiltered_count_text(&message.to_ascii_lowercase()) {
        args.insert("query".to_string(), Value::String(message.to_string()));
    }

    Value::Object(args)
}

fn is_unfiltered_count_text(text: &str) -> bool {
    let asks_for_count =
        text.contains("count") || text.contains("how many") || text.contains("number of");
    let mentions_mail =
        text.contains("email") || text.contains("message") || text.contains("mailbox");
    let has_filter_hint = text.contains(" from ")
        || text.contains(" about ")
        || text.contains(" sender ")
        || text.contains(" category ")
        || text.contains(" folder ")
        || text.contains(" in ")
        || text.contains(" since ")
        || text.contains(" today")
        || text.contains(" yesterday")
        || text.contains(" this week")
        || text.contains(" last week");

    asks_for_count && mentions_mail && !has_filter_hint
}

fn extract_sender_filter_from_count_text(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let marker = " from ";
    let start = lower
        .find(marker)
        .map(|index| index + marker.len())
        .or_else(|| lower.strip_prefix("from ").map(|_| "from ".len()))?;
    let tail = message.get(start..)?.trim();
    let mut words = Vec::new();
    for raw_word in tail.split_whitespace() {
        let word = raw_word.trim_matches(|ch: char| {
            ch.is_ascii_punctuation() && ch != '@' && ch != '.' && ch != '-' && ch != '_'
        });
        if word.is_empty() {
            continue;
        }
        let normalized = word.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "to" | "for"
                | "since"
                | "in"
                | "this"
                | "last"
                | "past"
                | "between"
                | "with"
                | "please"
                | "overall"
                | "total"
                | "today"
                | "yesterday"
        ) {
            break;
        }
        words.push(word.to_string());
    }

    let sender = words.join(" ");
    if sender.is_empty() {
        None
    } else {
        Some(sender)
    }
}

fn extract_spam_status_filter_from_text(message: &str) -> Option<&'static str> {
    let text = message.to_ascii_lowercase();
    if !text.contains("spam") {
        return None;
    }

    if text.contains("not spam")
        || text.contains("not-spam")
        || text.contains("non-spam")
        || text.contains("clean")
    {
        return Some("not-spam");
    }

    Some("spam")
}

fn extract_contextual_spam_status_filter_from_input(input: &Value) -> Option<&'static str> {
    let text = mail_assistant_input_text(input);
    if !has_followup_reference(&text) {
        return None;
    }

    let history = input
        .get("conversation_history")
        .and_then(Value::as_array)?;
    for message in history.iter().rev().take(6) {
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content.contains("not-spam")
            || content.contains("non-spam")
            || content.contains("not spam")
        {
            return Some("not-spam");
        }
        if content.contains("spam") {
            return Some("spam");
        }
    }

    None
}

fn output_promises_deferred_tool_work(output: &Value) -> bool {
    let text = [
        output
            .get("response_markdown")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        output
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ]
    .join("\n")
    .to_ascii_lowercase();

    [
        "i will run",
        "i'll run",
        "i will start",
        "i'll start",
        "please wait",
        "while i retrieve",
        "while i get",
        "run that count",
        "retrieve the exact",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

fn required_mailbox_tool_names(
    requires_count_emails_tool: bool,
    requires_synced_email_list_tool: bool,
) -> Vec<&'static str> {
    let mut names = Vec::new();
    if requires_count_emails_tool {
        names.push("count_emails");
    }
    if requires_synced_email_list_tool {
        names.push("list_synced_emails");
    }
    names
}

#[derive(Debug, Clone)]
struct DbCompletenessPending {
    snapshot: DbCompletenessSnapshot,
    requested_work: Vec<CoreWorkType>,
    stable_polls: u32,
    elapsed: Duration,
    max_wait: Duration,
}

#[derive(Debug, Clone)]
enum DbCompletenessWaitOutcome {
    Ready,
    Pending(DbCompletenessPending),
}

fn db_prerequisites_pending_output(
    account_id: &str,
    agent_name: &str,
    request_work_enabled: bool,
    retry_after_seconds: u64,
    pending: &DbCompletenessPending,
) -> Value {
    let requested_work = core_work_type_names(&pending.requested_work);
    let snapshot = &pending.snapshot;
    let response_markdown = db_prerequisites_pending_markdown(
        request_work_enabled,
        retry_after_seconds,
        snapshot,
        &requested_work,
    );

    json!({
        "status": DB_COMPLETENESS_PENDING_STATUS,
        "response_markdown": response_markdown,
        "summary": format!(
            "Mailbox prerequisites are still pending: {} emails need analysis and {} need filing recommendations.",
            snapshot.analysis_missing,
            snapshot.location_missing
        ),
        "confidence": 1.0,
        "needs_specialist": false,
        "agent_name": agent_name,
        "account_id": account_id,
        "request_work_enabled": request_work_enabled,
        "requested_work": requested_work,
        "retry_after_seconds": retry_after_seconds,
        "db_completeness": {
            "ready": false,
            "folder_count": snapshot.folder_count,
            "selectable_folders_missing_counts": snapshot.selectable_folders_missing_counts,
            "email_count": snapshot.email_count,
            "largest_folder_message_count": snapshot.largest_folder_message_count,
            "missing_message_id": snapshot.missing_message_id,
            "body_missing": snapshot.body_missing,
            "analysis_missing": snapshot.analysis_missing,
            "location_missing": snapshot.location_missing,
            "body_sync": {
                "pending": snapshot.body_sync.pending,
                "failed": snapshot.body_sync.failed,
                "processing": snapshot.body_sync.processing,
                "dead": snapshot.body_sync.dead,
            },
            "stable_polls": pending.stable_polls,
            "elapsed_ms": pending.elapsed.as_millis() as u64,
            "max_wait_seconds": pending.max_wait.as_secs(),
        }
    })
}

fn db_prerequisites_pending_markdown(
    request_work_enabled: bool,
    retry_after_seconds: u64,
    snapshot: &DbCompletenessSnapshot,
    requested_work: &[&str],
) -> String {
    let mut lines = vec![
        "## Mail Assistant".to_string(),
        String::new(),
        "I'm still preparing this mailbox, so I can't answer from the database reliably yet."
            .to_string(),
        String::new(),
    ];

    if request_work_enabled {
        lines.push(
            "Core has requested the missing prerequisite work and is still catching up:"
                .to_string(),
        );
    } else {
        lines.push("Core still needs prerequisite work before this answer can run:".to_string());
    }

    let mut details = Vec::new();
    if snapshot.folder_count == 0 {
        details.push("- Sync: mailbox folders or messages are still loading.".to_string());
    } else if snapshot.selectable_folders_missing_counts > 0 {
        details.push(format!(
            "- Sync: {} selectable folders still need message counts from IMAP.",
            snapshot.selectable_folders_missing_counts
        ));
    } else if snapshot.needs_full_sync_backfill() {
        details.push(format!(
            "- Sync: only {} emails are indexed, but the largest known folder reports {} messages.",
            snapshot.email_count, snapshot.largest_folder_message_count
        ));
    }
    if snapshot.missing_message_id > 0 {
        details.push(format!(
            "- Sync note: {} messages could not be imported because the server did not provide stable message IDs.",
            snapshot.missing_message_id
        ));
    }
    if snapshot.body_missing > 0
        || snapshot.body_sync.pending > 0
        || snapshot.body_sync.failed > 0
        || snapshot.body_sync.processing > 0
    {
        details.push(format!(
            "- Bodies: {} emails need body content; body queue pending/failed/processing/dead is {}/{}/{}/{}.",
            snapshot.body_missing,
            snapshot.body_sync.pending,
            snapshot.body_sync.failed,
            snapshot.body_sync.processing,
            snapshot.body_sync.dead
        ));
    }
    if snapshot.analysis_missing > 0 {
        details.push(format!(
            "- Analysis: {} emails still need classification and safety analysis.",
            snapshot.analysis_missing
        ));
    }
    if snapshot.location_missing > 0 {
        details.push(format!(
            "- Filing: {} emails still need folder recommendations.",
            snapshot.location_missing
        ));
    }
    if details.is_empty() {
        details.push("- Readiness: core still has active work that must settle first.".to_string());
    }

    lines.extend(details);
    lines.push(String::new());
    if requested_work.is_empty() {
        lines.push("- Requested work: no new core work was needed on this poll.".to_string());
    } else {
        lines.push(format!(
            "- Requested work: `{}`.",
            requested_work.join("`, `")
        ));
    }
    lines.push(format!(
        "- Next step: keep core running and try again in about {} seconds.",
        retry_after_seconds
    ));

    lines.join("\n")
}

pub struct AgentHarness {
    spec: AgentSpec,
    account_id: String,
    db: Arc<Database>,
    state: AgentState,
    provider: Arc<dyn AIProvider>,
    tools: ToolRegistry,
    db_completeness: DbCompletenessPollConfig,
    calibrated_threshold: Option<f32>,
}

impl AgentHarness {
    pub fn new(
        spec: AgentSpec,
        account_id: impl Into<String>,
        db: Arc<Database>,
        provider: Arc<dyn AIProvider>,
        tools: ToolRegistry,
    ) -> Self {
        let account_id = account_id.into();
        let db_completeness = DbCompletenessPollConfig::from_env();
        let state = AgentState::new(db.clone(), account_id.clone(), spec.name.clone());
        Self {
            spec,
            account_id,
            db,
            state,
            provider,
            tools,
            db_completeness,
            calibrated_threshold: None,
        }
    }

    fn should_wait_for_db_completeness(&self) -> bool {
        self.db_completeness.enabled && should_gate_db_completeness(&self.spec.name)
    }

    fn is_snapshot_ready(&self, snapshot: &DbCompletenessSnapshot, stable_polls: u32) -> bool {
        !snapshot.has_active_backlog() && stable_polls >= self.db_completeness.stable_polls_required
    }

    async fn log_db_completeness_event(
        &self,
        run_id: &str,
        event: &str,
        snapshot: &DbCompletenessSnapshot,
        requested_work: &[CoreWorkType],
        stable_polls: u32,
        elapsed: Duration,
    ) {
        let args = json!({
            "event": event,
            "account_id": self.account_id,
            "requested_work": core_work_type_names(requested_work),
            "request_work_enabled": self.db_completeness.request_work,
            "folder_count": snapshot.folder_count,
            "selectable_folders_missing_counts": snapshot.selectable_folders_missing_counts,
            "email_count": snapshot.email_count,
            "largest_folder_message_count": snapshot.largest_folder_message_count,
            "missing_message_id": snapshot.missing_message_id,
            "body_missing": snapshot.body_missing,
            "analysis_missing": snapshot.analysis_missing,
            "location_missing": snapshot.location_missing,
            "body_sync_pending": snapshot.body_sync.pending,
            "body_sync_failed": snapshot.body_sync.failed,
            "body_sync_processing": snapshot.body_sync.processing,
            "body_sync_dead": snapshot.body_sync.dead,
            "stable_polls": stable_polls,
            "stable_required": self.db_completeness.stable_polls_required,
            "elapsed_ms": elapsed.as_millis() as u64,
        });
        if let Err(error) = self
            .state
            .log_tool_call(run_id, 0, event, &args, event, elapsed.as_millis() as u64)
            .await
        {
            log::warn!("[harness] failed to log db completeness event: {}", error);
        }
    }

    async fn request_db_prerequisite_work(
        &self,
        run_id: &str,
        task_id: &str,
        snapshot: &DbCompletenessSnapshot,
        missing_work: &[CoreWorkType],
    ) -> Vec<CoreWorkType> {
        if !self.db_completeness.request_work || missing_work.is_empty() {
            return Vec::new();
        }

        let mut requested = Vec::new();
        for work_type in missing_work {
            let idempotency_key = format!("agent-db-completeness:{}", work_type.as_str());
            let payload = json!({
                "reason": "agent_db_completeness_prerequisite",
                "requested_by": "agent_harness",
                "agent_name": self.spec.name,
                "run_id": run_id,
                "task_id": task_id,
                "snapshot": {
                    "folder_count": snapshot.folder_count,
                    "selectable_folders_missing_counts": snapshot.selectable_folders_missing_counts,
                    "email_count": snapshot.email_count,
                    "largest_folder_message_count": snapshot.largest_folder_message_count,
                    "missing_message_id": snapshot.missing_message_id,
                    "body_missing": snapshot.body_missing,
                    "analysis_missing": snapshot.analysis_missing,
                    "location_missing": snapshot.location_missing,
                    "body_sync_pending": snapshot.body_sync.pending,
                    "body_sync_failed": snapshot.body_sync.failed,
                    "body_sync_processing": snapshot.body_sync.processing,
                    "body_sync_dead": snapshot.body_sync.dead,
                },
            });

            match self
                .db
                .enqueue_core_work_for_account(
                    &self.account_id,
                    *work_type,
                    &idempotency_key,
                    payload,
                )
                .await
            {
                Ok(_) => {
                    crate::metrics::counter("agent_db_completeness_work_request_total", 1, &[]);
                    requested.push(*work_type);
                }
                Err(error) => {
                    crate::metrics::counter(
                        "agent_db_completeness_work_request_failed_total",
                        1,
                        &[],
                    );
                    log::warn!(
                        "[harness] failed to request db prerequisite work task={} account={} work_type={}: {}",
                        task_id,
                        self.account_id,
                        work_type.as_str(),
                        error
                    );
                    requested.push(*work_type);
                }
            }
        }

        requested
    }

    async fn attach_mailbox_preparation_context(
        &self,
        run_id: &str,
        task_id: &str,
        input: &mut Value,
    ) -> Result<()> {
        if !should_use_partial_db_for_interactive_mail_assistant(&self.spec.name, task_id) {
            return Ok(());
        }

        let snapshot = self
            .db
            .db_completeness_snapshot_for_account(&self.account_id)
            .await
            .context("load mailbox preparation snapshot")?;
        let missing_work = classify_db_prerequisite_work(&snapshot);
        let requested_work = self
            .request_db_prerequisite_work(run_id, task_id, &snapshot, &missing_work)
            .await;

        crate::metrics::counter("agent_db_completeness_partial_context_total", 1, &[]);
        self.log_db_completeness_event(
            run_id,
            "db_completeness_partial_context",
            &snapshot,
            &requested_work,
            1,
            Duration::ZERO,
        )
        .await;

        let object = input
            .as_object_mut()
            .context("mail assistant chat input must be an object")?;
        object.insert(
            "mailbox_preparation".to_string(),
            json!({
                "ready": !snapshot.has_active_backlog(),
                "answer_policy": "Use the normal Mail Assistant tools and answer from prepared Postgres evidence. Do not refuse the entire turn just because preparation is incomplete. If a requested field is null, pending, or absent, say that specific detail has not been prepared yet.",
                "count_scope": "Counts and lists from tools are active synced emails currently stored in Postgres unless the tool says otherwise.",
                "folder_count": snapshot.folder_count,
                "synced_email_count": snapshot.email_count,
                "largest_folder_message_count": snapshot.largest_folder_message_count,
                "missing_message_id": snapshot.missing_message_id,
                "body_missing": snapshot.body_missing,
                "analysis_missing": snapshot.analysis_missing,
                "location_missing": snapshot.location_missing,
                "filing_pending": snapshot.filing_pending,
                "body_sync": {
                    "pending": snapshot.body_sync.pending,
                    "failed": snapshot.body_sync.failed,
                    "processing": snapshot.body_sync.processing,
                    "dead": snapshot.body_sync.dead,
                },
                "requested_work": core_work_type_names(&requested_work),
            }),
        );

        Ok(())
    }

    async fn wait_for_db_completeness(
        &self,
        run_id: &str,
        task_id: &str,
    ) -> Result<DbCompletenessWaitOutcome> {
        if !self.should_wait_for_db_completeness() {
            return Ok(DbCompletenessWaitOutcome::Ready);
        }
        if should_use_partial_db_for_interactive_mail_assistant(&self.spec.name, task_id) {
            return Ok(DbCompletenessWaitOutcome::Ready);
        }

        let started = Instant::now();
        let max_wait = self.db_completeness.max_wait_for_task(task_id);
        let mut delay = self.db_completeness.initial_backoff;
        let mut prev_email_count: Option<i64> = None;
        let mut stable_polls = 0u32;
        let mut requested_work: Vec<CoreWorkType> = Vec::new();

        loop {
            let snapshot = self
                .db
                .db_completeness_snapshot_for_account(&self.account_id)
                .await
                .context("load db completeness snapshot")?;

            stable_polls = if prev_email_count == Some(snapshot.email_count) {
                stable_polls.saturating_add(1)
            } else {
                1
            };
            prev_email_count = Some(snapshot.email_count);

            crate::metrics::gauge(
                "agent_db_completeness_folder_count",
                snapshot.folder_count as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_email_count",
                snapshot.email_count as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_largest_folder_message_count",
                snapshot.largest_folder_message_count as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_missing_message_id",
                snapshot.missing_message_id as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_body_missing",
                snapshot.body_missing as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_analysis_missing",
                snapshot.analysis_missing as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_location_missing",
                snapshot.location_missing as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_body_sync_pending",
                snapshot.body_sync.pending as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_body_sync_failed",
                snapshot.body_sync.failed as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_body_sync_processing",
                snapshot.body_sync.processing as f64,
                &[],
            );
            crate::metrics::gauge(
                "agent_db_completeness_stable_polls",
                stable_polls as f64,
                &[],
            );

            if self.is_snapshot_ready(&snapshot, stable_polls) {
                crate::metrics::counter("agent_db_completeness_proceed_total", 1, &[]);
                self.log_db_completeness_event(
                    run_id,
                    "db_completeness_ready",
                    &snapshot,
                    &requested_work,
                    stable_polls,
                    started.elapsed(),
                )
                .await;
                log::info!(
                    "[harness] db completeness ready for task={} account={} after {}ms",
                    task_id,
                    self.account_id,
                    started.elapsed().as_millis()
                );
                return Ok(DbCompletenessWaitOutcome::Ready);
            }

            let missing_work = classify_db_prerequisite_work(&snapshot)
                .into_iter()
                .filter(|work_type| !requested_work.contains(work_type))
                .collect::<Vec<_>>();
            let newly_requested = self
                .request_db_prerequisite_work(run_id, task_id, &snapshot, &missing_work)
                .await;
            requested_work.extend(newly_requested);

            if started.elapsed() >= max_wait {
                let pending = DbCompletenessPending {
                    snapshot,
                    requested_work,
                    stable_polls,
                    elapsed: started.elapsed(),
                    max_wait,
                };

                if should_complete_db_pending_for_task(task_id) {
                    crate::metrics::counter("agent_db_completeness_pending_response_total", 1, &[]);
                    self.log_db_completeness_event(
                        run_id,
                        "db_completeness_pending",
                        &pending.snapshot,
                        &pending.requested_work,
                        pending.stable_polls,
                        pending.elapsed,
                    )
                    .await;
                    log::info!(
                        "[harness] db completeness pending for task={} account={} after {}ms requested_work={:?}",
                        task_id,
                        self.account_id,
                        pending.elapsed.as_millis(),
                        core_work_type_names(&pending.requested_work)
                    );
                    return Ok(DbCompletenessWaitOutcome::Pending(pending));
                }

                crate::metrics::counter("agent_db_completeness_timeout_total", 1, &[]);
                self.log_db_completeness_event(
                    run_id,
                    "db_completeness_timeout",
                    &pending.snapshot,
                    &pending.requested_work,
                    pending.stable_polls,
                    pending.elapsed,
                )
                .await;
                bail!(
                    "db completeness wait timed out after {}s for task={} (requested_work={:?}, folder_count={}, selectable_folders_missing_counts={}, email_count={}, largest_folder_message_count={}, missing_message_id={}, body_missing={}, analysis_missing={}, location_missing={}, body_sync pending/failed/processing/dead={}/{}/{}/{})",
                    pending.max_wait.as_secs(),
                    task_id,
                    core_work_type_names(&pending.requested_work),
                    pending.snapshot.folder_count,
                    pending.snapshot.selectable_folders_missing_counts,
                    pending.snapshot.email_count,
                    pending.snapshot.largest_folder_message_count,
                    pending.snapshot.missing_message_id,
                    pending.snapshot.body_missing,
                    pending.snapshot.analysis_missing,
                    pending.snapshot.location_missing,
                    pending.snapshot.body_sync.pending,
                    pending.snapshot.body_sync.failed,
                    pending.snapshot.body_sync.processing,
                    pending.snapshot.body_sync.dead
                );
            }

            crate::metrics::counter("agent_db_completeness_wait_total", 1, &[]);
            self.log_db_completeness_event(
                run_id,
                "db_completeness_wait",
                &snapshot,
                &requested_work,
                stable_polls,
                started.elapsed(),
            )
            .await;
            log::info!(
                "[harness] waiting for db completeness task={} account={} stable={}/{} requested_work={:?} folder_count={} selectable_folders_missing_counts={} email_count={} largest_folder_message_count={} missing_message_id={} body_missing={} analysis_missing={} location_missing={} body_sync pending/failed/processing={}/{}/{} sleep_ms={}",
                task_id,
                self.account_id,
                stable_polls,
                self.db_completeness.stable_polls_required,
                core_work_type_names(&requested_work),
                snapshot.folder_count,
                snapshot.selectable_folders_missing_counts,
                snapshot.email_count,
                snapshot.largest_folder_message_count,
                snapshot.missing_message_id,
                snapshot.body_missing,
                snapshot.analysis_missing,
                snapshot.location_missing,
                snapshot.body_sync.pending,
                snapshot.body_sync.failed,
                snapshot.body_sync.processing,
                delay.as_millis()
            );

            tokio::time::sleep(delay).await;
            delay = delay
                .saturating_mul(2)
                .min(self.db_completeness.max_backoff);
        }
    }

    pub async fn run(&mut self, task_id: &str, input: Value) -> Result<RunResult> {
        self.run_with_callback(task_id, input, None).await
    }

    pub async fn run_with_callback(
        &mut self,
        task_id: &str,
        mut input: Value,
        callback: Option<HarnessEventCallback>,
    ) -> Result<RunResult> {
        let started_at = Instant::now();
        let run_id = self.state.begin_run(&self.spec, task_id).await?;
        Self::emit_event(
            callback.as_ref(),
            HarnessEvent::RunStarted {
                run_id: run_id.clone(),
                task_id: task_id.to_string(),
                agent_name: self.spec.name.clone(),
            },
        );

        if let Err(error) = self
            .attach_mailbox_preparation_context(&run_id, task_id, &mut input)
            .await
        {
            Self::emit_event(
                callback.as_ref(),
                HarnessEvent::Error {
                    message: error.to_string(),
                },
            );
            self.state
                .fail_run(&run_id, started_at, &error.to_string())
                .await?;
            return Err(error);
        }

        match self.wait_for_db_completeness(&run_id, task_id).await {
            Ok(DbCompletenessWaitOutcome::Ready) => {}
            Ok(DbCompletenessWaitOutcome::Pending(pending)) => {
                let output = db_prerequisites_pending_output(
                    &self.account_id,
                    &self.spec.name,
                    self.db_completeness.request_work,
                    pending_retry_after_seconds(&pending.snapshot),
                    &pending,
                );
                Self::emit_event(
                    callback.as_ref(),
                    HarnessEvent::FinalOutput {
                        output: output.clone(),
                    },
                );
                self.state
                    .finish_run(&run_id, started_at, &output, false, Some(1.0))
                    .await?;
                return Ok(RunResult {
                    run_id,
                    output,
                    should_escalate: false,
                    escalate_reason: None,
                    llm_calls: 0,
                    tool_calls: 0,
                    input_tokens: None,
                    output_tokens: None,
                });
            }
            Err(error) => {
                Self::emit_event(
                    callback.as_ref(),
                    HarnessEvent::Error {
                        message: error.to_string(),
                    },
                );
                self.state
                    .fail_run(&run_id, started_at, &error.to_string())
                    .await?;
                return Err(error);
            }
        }

        // Load calibrated escalation threshold from scratchpad (if available)
        let calibration_key = format!("calibration_{}", self.spec.name);
        self.calibrated_threshold = self
            .state
            .read_scratchpad(&calibration_key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.get("threshold").and_then(|t| t.as_f64()))
            .map(|t| t as f32);

        let mut total_input_tokens: u32 = 0;
        let mut total_output_tokens: u32 = 0;
        let mut total_llm_calls: u32 = 0;
        let mut total_tool_calls: u32 = 0;
        let mut used_tool_names: Vec<String> = Vec::new();
        let requires_count_emails_tool = should_require_count_emails_tool(&self.spec.name, &input);
        let requires_synced_email_list_tool =
            should_require_synced_email_list_tool(&self.spec.name, &input);

        let mut messages = self.build_initial_messages(&input).await?;

        if let Some(checkpoint) = self.state.latest_checkpoint(&run_id).await? {
            log::info!(
                "[harness] resuming run {} from checkpoint at step {}",
                run_id,
                checkpoint.step
            );
            messages = checkpoint.messages;
        }

        if requires_count_emails_tool && !used_tool_names.iter().any(|name| name == "count_emails")
        {
            let count_args = count_emails_tool_args_for_input(&input);
            let t0 = Instant::now();
            Self::emit_event(
                callback.as_ref(),
                HarnessEvent::ToolCall {
                    step: 0,
                    tool_name: "count_emails".to_string(),
                    arguments: count_args.clone(),
                },
            );
            let result = self
                .tools
                .execute("count_emails", count_args.clone())
                .await
                .context("auto count_emails prerequisite")?;
            let latency_ms = t0.elapsed().as_millis() as u64;
            self.state
                .log_tool_call(&run_id, 0, "count_emails", &count_args, &result, latency_ms)
                .await?;
            self.state.record_tool_calls(&run_id, 1).await?;
            Self::emit_event(
                callback.as_ref(),
                HarnessEvent::ToolResult {
                    step: 0,
                    tool_name: "count_emails".to_string(),
                    result: result.clone(),
                    latency_ms,
                },
            );
            used_tool_names.push("count_emails".to_string());
            total_tool_calls += 1;
            messages.push(Message::user(format!(
                "Required mailbox count result, already fetched with `count_emails`. Use this result for the final answer and do not invent a different count.\n\n[count_emails]: {}",
                result
            )));
        }

        if requires_synced_email_list_tool
            && !used_tool_names
                .iter()
                .any(|name| name == "list_synced_emails")
        {
            let list_args = synced_email_list_tool_args_for_input(&input);
            let t0 = Instant::now();
            Self::emit_event(
                callback.as_ref(),
                HarnessEvent::ToolCall {
                    step: 0,
                    tool_name: "list_synced_emails".to_string(),
                    arguments: list_args.clone(),
                },
            );
            let result = self
                .tools
                .execute("list_synced_emails", list_args.clone())
                .await
                .context("auto list_synced_emails prerequisite")?;
            let latency_ms = t0.elapsed().as_millis() as u64;
            self.state
                .log_tool_call(
                    &run_id,
                    0,
                    "list_synced_emails",
                    &list_args,
                    &result,
                    latency_ms,
                )
                .await?;
            self.state.record_tool_calls(&run_id, 1).await?;
            Self::emit_event(
                callback.as_ref(),
                HarnessEvent::ToolResult {
                    step: 0,
                    tool_name: "list_synced_emails".to_string(),
                    result: result.clone(),
                    latency_ms,
                },
            );
            used_tool_names.push("list_synced_emails".to_string());
            total_tool_calls += 1;
            messages.push(Message::user(format!(
                "Required synced email list, already fetched with `list_synced_emails`. Use only this current Postgres data for the final answer. Clearly say that the mailbox is still being prepared if rows have pending analysis, summaries, or filing data.\n\n[list_synced_emails]: {}",
                result
            )));
        }

        let timeout_duration = Duration::from_secs(self.spec.execution.timeout_secs);
        let loop_callback = callback.clone();
        let loop_result = tokio::time::timeout(timeout_duration, async {
            for step in 0..self.spec.execution.max_iterations {
                Self::emit_event(
                    loop_callback.as_ref(),
                    HarnessEvent::StepStarted { step },
                );

                if total_llm_calls >= self.spec.budget.max_llm_calls as u32 {
                    bail!(
                        "budget exceeded: max_llm_calls ({}) reached",
                        self.spec.budget.max_llm_calls
                    );
                }

                self.state.record_step(&run_id).await?;

                let tools = if self.provider.supports_tool_calling() {
                    self.tools.as_completion_tools()
                } else {
                    vec![]
                };
                let request = AICompletionRequest {
                    messages: messages.clone(),
                    tools,
                    tool_choice: ToolChoice::Auto,
                    temperature: self.spec.execution.temperature,
                    max_tokens: Some(self.spec.execution.max_output_tokens),
                };

                let response = self
                    .provider
                    .complete_with_request(request)
                    .await
                    .with_context(|| format!("LLM call failed at step {}", step))?;

                total_llm_calls += 1;
                if let Some(usage) = &response.usage {
                    total_input_tokens += usage.input_tokens.unwrap_or(0);
                    total_output_tokens += usage.output_tokens.unwrap_or(0);
                }
                self.state
                    .record_llm_call(
                        &run_id,
                        response.usage.as_ref().and_then(|u| u.input_tokens),
                        response.usage.as_ref().and_then(|u| u.output_tokens),
                    )
                    .await?;

                let tool_calls_to_execute = if let Some(calls) = response
                    .tool_calls
                    .as_ref()
                    .filter(|calls| !calls.is_empty())
                {
                    calls.clone()
                } else if let Some(call) = Self::parse_json_tool_call(&response.content) {
                    vec![call]
                } else {
                    vec![]
                };

                if !tool_calls_to_execute.is_empty() {
                    if total_tool_calls + tool_calls_to_execute.len() as u32
                        > self.spec.budget.max_tool_calls as u32
                    {
                        bail!(
                            "budget exceeded: max_tool_calls ({}) reached",
                            self.spec.budget.max_tool_calls
                        );
                    }

                    let mut tool_result_parts = Vec::new();
                    for call in &tool_calls_to_execute {
                        let t0 = Instant::now();
                        Self::emit_event(
                            loop_callback.as_ref(),
                            HarnessEvent::ToolCall {
                                step,
                                tool_name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        );
                        let result = self.tools.execute(&call.name, call.arguments.clone()).await?;
                        let latency_ms = t0.elapsed().as_millis() as u64;

                        self.state
                            .log_tool_call(
                                &run_id,
                                step,
                                &call.name,
                                &call.arguments,
                                &result,
                                latency_ms,
                            )
                            .await?;

                        Self::emit_event(
                            loop_callback.as_ref(),
                            HarnessEvent::ToolResult {
                                step,
                                tool_name: call.name.clone(),
                                result: result.clone(),
                                latency_ms,
                            },
                        );
                        tool_result_parts.push(format!("[{}]: {}", call.name, result));
                        used_tool_names.push(call.name.clone());
                        total_tool_calls += 1;
                    }
                    self.state
                        .record_tool_calls(&run_id, tool_calls_to_execute.len())
                        .await?;

                    messages.push(Message::assistant(response.content.clone()));
                    messages.push(Message::user(tool_result_parts.join("\n\n")));

                    if (step + 1) % self.spec.execution.checkpoint_every == 0 {
                        if let Err(error) = self.state.save_checkpoint(&run_id, step, &messages).await {
                            log::warn!(
                                "[harness] checkpoint save failed at step {}: {}",
                                step,
                                error
                            );
                        }
                    }

                    continue;
                }

                if let Some(action) = Self::parse_scratchpad_action(&response.content) {
                    let scratchpad_result = self.handle_scratchpad_action(action).await?;
                    messages.push(Message::assistant(response.content.clone()));
                    messages.push(Message::user(scratchpad_result));

                    if (step + 1) % self.spec.execution.checkpoint_every == 0 {
                        if let Err(error) = self.state.save_checkpoint(&run_id, step, &messages).await {
                            log::warn!(
                                "[harness] checkpoint save failed at step {}: {}",
                                step,
                                error
                            );
                        }
                    }

                    continue;
                }

                match Self::validate_output(&response.content, &self.spec.output) {
                    Ok(output) => {
                        if requires_count_emails_tool
                            && !used_tool_names.iter().any(|name| name == "count_emails")
                        {
                            log::warn!(
                                "[harness] step {}: count request attempted final output before count_emails tool",
                                step
                            );
                            messages.push(Message::assistant(response.content.clone()));
                            messages.push(Message::user(
                                "This user request asks for a mailbox email count. You must call the `count_emails` tool before the final JSON answer. For 'emails from X', call `count_emails` with the `sender` filter set to X. Then answer with the returned total_count."
                                    .to_string(),
                            ));
                            continue;
                        }

                        if requires_synced_email_list_tool
                            && !used_tool_names
                                .iter()
                                .any(|name| name == "list_synced_emails")
                        {
                            log::warn!(
                                "[harness] step {}: list request attempted final output before list_synced_emails tool",
                                step
                            );
                            messages.push(Message::assistant(response.content.clone()));
                            messages.push(Message::user(
                                "This user request asks about currently synced database rows. You must call the `list_synced_emails` tool before the final JSON answer, then answer from the returned emails and note any pending preparation limits."
                                    .to_string(),
                            ));
                            continue;
                        }

                        if (requires_count_emails_tool || requires_synced_email_list_tool)
                            && output_promises_deferred_tool_work(&output)
                        {
                            log::warn!(
                                "[harness] step {}: mailbox tool request returned deferred-tool promise after tools={:?}",
                                step,
                                used_tool_names
                            );
                            messages.push(Message::assistant(response.content.clone()));
                            messages.push(Message::user(format!(
                                "Do not promise to run mailbox tools later. You already have the required tool result(s): {}. Return the final JSON answer now using those results, including the exact count or rows available.",
                                required_mailbox_tool_names(
                                    requires_count_emails_tool,
                                    requires_synced_email_list_tool,
                                )
                                .join(", ")
                            )));
                            continue;
                        }

                        Self::emit_event(
                            loop_callback.as_ref(),
                            HarnessEvent::FinalOutput {
                                output: output.clone(),
                            },
                        );
                        let (should_escalate, escalate_reason) = self.check_escalation(&output);
                        let output_confidence = output
                            .get("confidence")
                            .and_then(|v| v.as_f64())
                            .map(|v| v as f32);
                        self.state.finish_run(&run_id, started_at, &output, should_escalate, output_confidence).await?;

                        return Ok(RunResult {
                            run_id: run_id.clone(),
                            output,
                            should_escalate,
                            escalate_reason,
                            llm_calls: total_llm_calls,
                            tool_calls: total_tool_calls,
                            input_tokens: if total_input_tokens > 0 {
                                Some(total_input_tokens)
                            } else {
                                None
                            },
                            output_tokens: if total_output_tokens > 0 {
                                Some(total_output_tokens)
                            } else {
                                None
                            },
                        });
                    }
                    Err(validation_error) => {
                        log::warn!("[harness] step {}: invalid output: {}", step, validation_error);
                        let repair_hint = output_validation_repair_hint(&self.spec.output);
                        messages.push(Message::assistant(response.content.clone()));
                        messages.push(Message::user(format!(
                            "Your output was invalid: {}. Return a JSON object with these required fields: {}. No markdown, no prose.{}",
                            validation_error,
                            self.spec.output.required_fields.join(", "),
                            repair_hint,
                        )));
                    }
                }
            }

            bail!(
                "max_iterations ({}) reached without a valid final answer",
                self.spec.execution.max_iterations
            )
        })
        .await;

        match loop_result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                Self::emit_event(
                    callback.as_ref(),
                    HarnessEvent::Error {
                        message: error.to_string(),
                    },
                );
                self.state
                    .fail_run(&run_id, started_at, &error.to_string())
                    .await?;
                Err(error)
            }
            Err(_) => {
                let message = format!(
                    "agent {} timed out after {}s on task {}",
                    self.spec.name, self.spec.execution.timeout_secs, task_id
                );
                Self::emit_event(
                    callback.as_ref(),
                    HarnessEvent::Error {
                        message: message.clone(),
                    },
                );
                self.state.timeout_run(&run_id, started_at).await?;
                bail!(message)
            }
        }
    }

    fn emit_event(callback: Option<&HarnessEventCallback>, event: HarnessEvent) {
        if let Some(callback) = callback {
            callback(event);
        }
    }

    async fn build_initial_messages(&self, input: &Value) -> Result<Vec<Message>> {
        let _ = self.state.prune_expired_scratchpad().await?;

        let mut scratchpad_lines = Vec::new();
        for key in &self.spec.state.schema {
            if let Some(value) = self.state.read_scratchpad(key).await? {
                if value.is_null() {
                    continue;
                }
                if matches!(&value, Value::Object(map) if map.is_empty()) {
                    continue;
                }
                let serialized =
                    serde_json::to_string(&value).context("serialize scratchpad value")?;
                scratchpad_lines.push(format!("{}: {}", key, serialized));
            }
        }

        let mut system_prompt = self.spec.system_prompt.clone();
        if !scratchpad_lines.is_empty() {
            system_prompt
                .push_str("\n\n--- Scratchpad (your persistent memory for this account) ---\n");
            system_prompt.push_str(&scratchpad_lines.join("\n"));
            system_prompt.push_str("\n--- End Scratchpad ---");
        }

        let input_json = serde_json::to_string_pretty(input).context("serialize task input")?;

        Ok(vec![
            Message::system(system_prompt),
            Message::user(input_json),
        ])
    }

    fn parse_json_tool_call(content: &str) -> Option<ToolCall> {
        let parsed = parse_embedded_json(content)?;
        let action = parsed.get("action")?.as_str()?;
        if action != "use_tool" {
            return None;
        }
        let name = parsed.get("tool")?.as_str()?.to_string();
        let arguments = parsed
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        Some(ToolCall {
            id: format!("fallback-{}", name),
            name,
            arguments,
        })
    }

    fn validate_output(content: &str, config: &OutputConfig) -> Result<Value> {
        let stripped = strip_markdown_fences(content).trim();
        let mut parsed: Value = serde_json::from_str(stripped)
            .or_else(|_| {
                let candidate = extract_json_object(stripped).unwrap_or_default();
                serde_json::from_str(&candidate)
            })
            .with_context(|| "response was not valid JSON".to_string())?;

        normalize_common_output_enums(&mut parsed, config);
        config.validate(&parsed)?;
        Ok(parsed)
    }

    fn check_escalation(&self, output: &Value) -> (bool, Option<String>) {
        evaluate_escalation_with_threshold(&self.spec.escalation, output, self.calibrated_threshold)
    }

    async fn handle_scratchpad_action(&self, action: ScratchpadAction) -> Result<String> {
        match action {
            ScratchpadAction::Read { key } => {
                self.ensure_allowed_scratchpad_key(&key)?;
                let value = self
                    .state
                    .read_scratchpad(&key)
                    .await?
                    .unwrap_or(Value::Null);
                Ok(serde_json::json!({
                    "action": "read_scratchpad_result",
                    "key": key,
                    "account_id": self.account_id,
                    "value": value,
                })
                .to_string())
            }
            ScratchpadAction::Write { key, value } => {
                self.ensure_allowed_scratchpad_key(&key)?;
                self.state
                    .write_scratchpad(&key, value, Some(self.spec.state.ttl_hours))
                    .await?;
                Ok(serde_json::json!({
                    "action": "write_scratchpad_result",
                    "key": key,
                    "account_id": self.account_id,
                    "status": "ok",
                })
                .to_string())
            }
        }
    }

    fn ensure_allowed_scratchpad_key(&self, key: &str) -> Result<()> {
        if self.spec.state.schema.iter().any(|allowed| allowed == key) {
            return Ok(());
        }
        bail!("scratchpad key '{}' is not declared in state.schema", key)
    }

    fn parse_scratchpad_action(content: &str) -> Option<ScratchpadAction> {
        let parsed = parse_embedded_json(content)?;
        let action = parsed.get("action")?.as_str()?;
        match action {
            "read_scratchpad" => Some(ScratchpadAction::Read {
                key: parsed.get("key")?.as_str()?.to_string(),
            }),
            "write_scratchpad" => Some(ScratchpadAction::Write {
                key: parsed.get("key")?.as_str()?.to_string(),
                value: parsed.get("value").cloned().unwrap_or(Value::Null),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum ScratchpadAction {
    Read { key: String },
    Write { key: String, value: Value },
}

fn strip_markdown_fences(content: &str) -> &str {
    let trimmed = content.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    trimmed.strip_suffix("```").unwrap_or(trimmed).trim()
}

fn extract_json_object(content: &str) -> Option<String> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end < start {
        return None;
    }
    Some(content[start..=end].to_string())
}

fn normalize_common_output_enums(output: &mut Value, config: &OutputConfig) {
    let Some(object) = output.as_object_mut() else {
        return;
    };

    for (field, validation) in &config.validation {
        let Some(enum_values) = validation.enum_values.as_ref() else {
            continue;
        };
        let Some(value) = object.get_mut(field) else {
            continue;
        };
        let Some(raw) = value.as_str() else {
            continue;
        };
        if enum_values.iter().any(|allowed| allowed == raw) {
            continue;
        }

        if let Some(normalized) = normalize_known_enum_value(field, raw, enum_values) {
            *value = Value::String(normalized);
        }
    }
}

fn normalize_known_enum_value(_field: &str, raw: &str, allowed: &[String]) -> Option<String> {
    let canonical = canonicalize_enum_value(raw);
    allowed
        .iter()
        .find(|candidate| canonicalize_enum_value(candidate) == canonical)
        .cloned()
}

fn canonicalize_enum_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', '-'], " ")
}

fn output_validation_repair_hint(config: &OutputConfig) -> &'static str {
    let has_category_enum = config
        .validation
        .get("category")
        .and_then(|validation| validation.enum_values.as_ref())
        .is_some();
    let has_subcategory = config
        .required_fields
        .iter()
        .any(|field| field == "subcategory");
    let has_email_type_enum = config
        .validation
        .get("email_type")
        .and_then(|validation| validation.enum_values.as_ref())
        .is_some();

    match (has_category_enum && has_subcategory, has_email_type_enum) {
        (true, true) => " For category, choose one allowed top-level enum value exactly. If your intended category is novel or more specific than the enum, keep the nearest allowed top-level value in category and move the novel/specific label into subcategory. For email_type, choose one allowed enum value by meaning; do not invent values such as marketing, promotional, personal, or security.",
        (true, false) => " For category, choose one allowed top-level enum value exactly. If your intended category is novel or more specific than the enum, keep the nearest allowed top-level value in category and move the novel/specific label into subcategory.",
        (false, true) => " For email_type, choose one allowed enum value by meaning; do not invent values such as marketing, promotional, personal, or security.",
        (false, false) => "",
    }
}

fn parse_embedded_json(content: &str) -> Option<Value> {
    let stripped = strip_markdown_fences(content);
    serde_json::from_str(stripped)
        .or_else(|_| {
            let candidate = extract_json_object(stripped).unwrap_or_default();
            serde_json::from_str(&candidate)
        })
        .ok()
}

/// Evaluate escalation using the spec threshold (no calibration override).
#[cfg(test)]
pub(crate) fn evaluate_escalation(
    config: &super::spec::EscalationConfig,
    output: &Value,
) -> (bool, Option<String>) {
    evaluate_escalation_with_threshold(config, output, None)
}

/// Evaluate escalation with an optional calibrated threshold override.
/// If `calibrated_threshold` is Some, it overrides the spec's `confidence_threshold`.
pub(crate) fn evaluate_escalation_with_threshold(
    config: &super::spec::EscalationConfig,
    output: &Value,
    calibrated_threshold: Option<f32>,
) -> (bool, Option<String>) {
    let effective_threshold = calibrated_threshold.unwrap_or(config.confidence_threshold);
    let threshold_source = if calibrated_threshold.is_some() {
        "calibrated"
    } else {
        "spec default"
    };

    if let Some(confidence) = output.get("confidence").and_then(|value| value.as_f64()) {
        if confidence < f64::from(effective_threshold) {
            return (
                true,
                Some(format!(
                    "confidence {:.3} below threshold {:.3} ({})",
                    confidence, effective_threshold, threshold_source
                )),
            );
        }
    }

    if config.always_escalate_on_phishing
        && output
            .get("phishing_status")
            .and_then(|value| value.as_str())
            == Some("phishing")
    {
        return (
            true,
            Some("phishing result requires escalation".to_string()),
        );
    }

    if let Some(threat_level) = output.get("threat_level").and_then(|value| value.as_str()) {
        if config
            .always_escalate_on_threat
            .iter()
            .any(|candidate| candidate == threat_level)
        {
            return (
                true,
                Some(format!(
                    "threat_level '{}' requires escalation",
                    threat_level
                )),
            );
        }
    }

    (false, None)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use sqlx::Row;

    use super::*;
    use crate::ai::{AIResponse, CompletionTool, TokenUsage};
    use crate::config::DEFAULT_ACCOUNT_ID;
    use crate::harness::spec::{
        BudgetConfig, EscalationConfig, ExecutionConfig, FieldValidation, OutputConfig,
        ProviderConfig, ProviderTier, StateConfig,
    };
    use crate::harness::tools::EchoTool;

    struct MockProvider {
        turn: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AIProvider for MockProvider {
        async fn complete(&self, _messages: Vec<Message>) -> Result<AIResponse> {
            bail!("complete() should not be called directly in harness tests")
        }

        async fn complete_with_request(&self, request: AICompletionRequest) -> Result<AIResponse> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            let _tools: Vec<CompletionTool> = request.tools;
            match turn {
                0 => Ok(AIResponse {
                    content: String::new(),
                    confidence: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        arguments: serde_json::json!({ "text": "hello" }),
                    }]),
                    finish_reason: "tool_calls".to_string(),
                    usage: Some(TokenUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(2),
                    }),
                }),
                _ => Ok(AIResponse {
                    content: serde_json::json!({
                        "status": "ok",
                        "confidence": 0.95
                    })
                    .to_string(),
                    confidence: None,
                    tool_calls: None,
                    finish_reason: "stop".to_string(),
                    usage: Some(TokenUsage {
                        input_tokens: Some(8),
                        output_tokens: Some(3),
                    }),
                }),
            }
        }

        fn supports_tool_calling(&self) -> bool {
            true
        }
    }

    fn minimal_spec() -> AgentSpec {
        let mut validation = std::collections::HashMap::new();
        validation.insert(
            "status".to_string(),
            FieldValidation {
                enum_values: Some(vec!["ok".to_string(), "bad".to_string()]),
                field_type: None,
                min: None,
                max: None,
            },
        );
        validation.insert(
            "confidence".to_string(),
            FieldValidation {
                enum_values: None,
                field_type: Some("number".to_string()),
                min: Some(0.0),
                max: Some(1.0),
            },
        );

        AgentSpec {
            name: "test-agent".to_string(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            skills: Vec::new(),
            execution: ExecutionConfig {
                max_iterations: 4,
                temperature: 0.2,
                max_output_tokens: 256,
                checkpoint_every: 1,
                timeout_secs: 30,
            },
            budget: BudgetConfig {
                max_llm_calls: 4,
                max_tool_calls: 4,
            },
            state: StateConfig {
                schema: vec!["sender_patterns".to_string()],
                ttl_hours: 24,
            },
            output: OutputConfig {
                required_fields: vec!["status".to_string(), "confidence".to_string()],
                validation,
            },
            provider: ProviderConfig {
                tier: ProviderTier::Worker,
                prefer: "local".to_string(),
                fallback: "frontier".to_string(),
            },
            escalation: EscalationConfig {
                confidence_threshold: 0.75,
                always_escalate_on_phishing: false,
                always_escalate_on_threat: Vec::new(),
            },
            system_prompt: "You are a test agent.".to_string(),
        }
    }

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

    #[tokio::test]
    #[ignore]
    async fn test_harness_run_completes_after_tool_call() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping harness integration test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let provider: Arc<dyn AIProvider> = Arc::new(MockProvider {
            turn: AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool);
        let spec = minimal_spec();
        let mut harness = AgentHarness::new(spec, DEFAULT_ACCOUNT_ID, db.clone(), provider, tools);

        let result = harness
            .run(
                "task-tool-complete",
                serde_json::json!({ "message_id": "x" }),
            )
            .await
            .expect("run");

        assert_eq!(result.output["status"], "ok");
        assert_eq!(result.tool_calls, 1);

        let run_row = sqlx::query("SELECT status FROM agent_runs WHERE run_id = $1")
            .bind(&result.run_id)
            .fetch_one(&db.pool)
            .await
            .expect("fetch agent run");
        assert_eq!(run_row.get::<String, _>("status"), "completed");

        let tool_log_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_tool_log WHERE run_id = $1")
                .bind(&result.run_id)
                .fetch_one(&db.pool)
                .await
                .expect("fetch tool log count");
        assert_eq!(tool_log_count, 1);
    }

    #[tokio::test]
    #[ignore]
    async fn test_harness_resume_from_checkpoint() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping harness resume test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let spec = minimal_spec();
        let state = AgentState::new(db.clone(), DEFAULT_ACCOUNT_ID, spec.name.clone());
        let run_id = state.begin_run(&spec, "resume-task").await.expect("begin");
        state
            .save_checkpoint(
                &run_id,
                1,
                &[
                    Message::system("System prompt"),
                    Message::user("{\"message_id\":\"resume\"}"),
                    Message::assistant(
                        "{\"action\":\"use_tool\",\"tool\":\"echo\",\"args\":{\"text\":\"hi\"}}",
                    ),
                ],
            )
            .await
            .expect("save checkpoint");

        let provider: Arc<dyn AIProvider> = Arc::new(MockProvider {
            turn: AtomicUsize::new(1),
        });
        let mut tools = ToolRegistry::new();
        tools.register(EchoTool);
        let mut harness = AgentHarness::new(spec, DEFAULT_ACCOUNT_ID, db.clone(), provider, tools);
        let result = harness
            .run("resume-task", serde_json::json!({ "message_id": "resume" }))
            .await
            .expect("resume run");

        assert_eq!(result.run_id, run_id);
    }

    #[test]
    fn test_validate_output_valid() {
        let spec = minimal_spec();
        let out =
            AgentHarness::validate_output(r#"{"status":"ok","confidence":0.95}"#, &spec.output)
                .expect("valid output");
        assert_eq!(out["status"], "ok");
    }

    #[test]
    fn test_validate_output_missing_field() {
        let spec = minimal_spec();
        let error = AgentHarness::validate_output(r#"{"status":"ok"}"#, &spec.output)
            .expect_err("missing field should fail");
        assert!(error.to_string().contains("confidence"));
    }

    #[test]
    fn test_validate_output_bad_enum() {
        let spec = minimal_spec();
        let error =
            AgentHarness::validate_output(r#"{"status":"maybe","confidence":0.95}"#, &spec.output)
                .expect_err("bad enum should fail");
        assert!(error.to_string().contains("invalid enum value"));
    }

    #[test]
    fn test_validate_output_normalizes_common_analysis_enums_without_category_guessing() {
        let mut validation = std::collections::HashMap::new();
        validation.insert(
            "category".to_string(),
            FieldValidation {
                enum_values: Some(vec![
                    "personal".to_string(),
                    "work".to_string(),
                    "financial".to_string(),
                    "shopping".to_string(),
                    "travel".to_string(),
                ]),
                field_type: None,
                min: None,
                max: None,
            },
        );
        validation.insert(
            "email_type".to_string(),
            FieldValidation {
                enum_values: Some(vec![
                    "newsletter".to_string(),
                    "notification".to_string(),
                    "transactional".to_string(),
                    "receipt".to_string(),
                    "conversation".to_string(),
                ]),
                field_type: None,
                min: None,
                max: None,
            },
        );
        let config = OutputConfig {
            required_fields: vec!["category".to_string(), "email_type".to_string()],
            validation,
        };

        let error = AgentHarness::validate_output(
            r#"{"category":"professional","email_type":"promotional"}"#,
            &config,
        )
        .expect_err("category should not be guessed from hard-coded aliases");
        assert!(error.to_string().contains("invalid enum value"));

        let out = AgentHarness::validate_output(
            r#"{"category":"WORK","email_type":"NOTIFICATION"}"#,
            &config,
        )
        .expect("exact category and email_type casing should normalize");

        assert_eq!(out["category"], "work");
        assert_eq!(out["email_type"], "notification");

        let error = AgentHarness::validate_output(
            r#"{"category":"WORK","email_type":"promotional"}"#,
            &config,
        )
        .expect_err("email_type should not be guessed from hard-coded aliases");
        assert!(error.to_string().contains("invalid enum value"));

        let error = AgentHarness::validate_output(
            r#"{"category":"personal","email_type":"personal"}"#,
            &config,
        )
        .expect_err("personal email_type should not normalize to conversation");
        assert!(error.to_string().contains("invalid enum value"));
    }

    #[test]
    fn test_validate_output_strips_markdown_fences() {
        let spec = minimal_spec();
        let out = AgentHarness::validate_output(
            "```json\n{\"status\":\"ok\",\"confidence\":0.95}\n```",
            &spec.output,
        )
        .expect("strip fences");
        assert_eq!(out["status"], "ok");
    }

    #[test]
    fn test_check_escalation_low_confidence() {
        let config = EscalationConfig {
            confidence_threshold: 0.75,
            always_escalate_on_phishing: false,
            always_escalate_on_threat: Vec::new(),
        };
        let (should_escalate, reason) =
            evaluate_escalation(&config, &serde_json::json!({ "confidence": 0.5 }));
        assert!(should_escalate);
        assert!(reason.unwrap_or_default().contains("below threshold"));
    }

    #[test]
    fn test_check_escalation_phishing_flag() {
        let config = EscalationConfig {
            confidence_threshold: 0.75,
            always_escalate_on_phishing: true,
            always_escalate_on_threat: Vec::new(),
        };
        let (should_escalate, reason) = evaluate_escalation(
            &config,
            &serde_json::json!({
                "confidence": 0.95,
                "phishing_status": "phishing"
            }),
        );
        assert!(should_escalate);
        assert!(reason.unwrap_or_default().contains("phishing"));
    }

    #[test]
    fn test_parse_json_tool_call() {
        let call = AgentHarness::parse_json_tool_call(
            r#"{"action":"use_tool","tool":"echo","args":{"text":"hello"}}"#,
        )
        .expect("tool call");
        assert_eq!(call.id, "fallback-echo");
        assert_eq!(call.name, "echo");
        assert_eq!(call.arguments["text"], "hello");
    }

    #[test]
    fn test_should_gate_db_completeness_for_user_facing_mail_assistant_only() {
        assert!(should_gate_db_completeness("mail-assistant"));
        assert!(!should_gate_db_completeness("email-analyzer"));
        assert!(!should_gate_db_completeness("inbox-analyzer"));
        assert!(!should_gate_db_completeness("location-router"));
    }

    #[test]
    fn test_should_complete_db_pending_for_api_chat_tasks_only() {
        assert!(should_complete_db_pending_for_task(
            "api-chat-mail-assistant-thread-1"
        ));
        assert!(!should_complete_db_pending_for_task("message-id-1"));
    }

    #[test]
    fn test_mail_assistant_count_requests_require_count_tool() {
        assert!(should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "count of all emails from facebook to start" })
        ));
        assert!(should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "user_message": "How many emails are from GitHub?" })
        ));
        assert!(should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "How many messages are from GitHub?" })
        ));
        assert!(should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "How many emails total?" })
        ));
        assert!(should_require_count_emails_tool(
            "mail-assistant",
            &json!({
                "message": "great! how many of those are from google",
                "conversation_history": [
                    {
                        "role": "user",
                        "content": "can you give me a count of the current list of identified spam emails?"
                    },
                    {
                        "role": "agent",
                        "content": "There are 115 identified spam emails in the current synced data."
                    }
                ]
            })
        ));
        assert!(!should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "How many messages have been synced as of right now?" })
        ));
        assert!(!should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "ok can you tell me about the first 5 in the database?" })
        ));
        assert!(!should_require_count_emails_tool(
            "mail-assistant",
            &json!({ "message": "Summarize my inbox this week" })
        ));
        assert!(!should_require_count_emails_tool(
            "digest-agent",
            &json!({ "message": "How many emails are from GitHub?" })
        ));
    }

    #[test]
    fn test_count_emails_tool_args_extracts_sender_filter() {
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "count of all emails from facebook to start" })
            )["sender"],
            "facebook"
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "How many emails are from GitHub?" })
            )["sender"],
            "GitHub"
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "Count emails about account security" })
            )["query"],
            "Count emails about account security"
        );
        assert!(
            count_emails_tool_args_for_input(&json!({ "message": "How many emails total?" }))
                .get("query")
                .is_none()
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "can you give me a count of the current list of identified spam emails?" })
            )["spam_status"],
            "spam"
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "How many non-spam emails are currently identified?" })
            )["spam_status"],
            "not-spam"
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "How many spam emails are from Facebook?" })
            )["sender"],
            "Facebook"
        );
        assert_eq!(
            count_emails_tool_args_for_input(
                &json!({ "message": "How many spam emails are from Facebook?" })
            )["spam_status"],
            "spam"
        );
        let followup_args = count_emails_tool_args_for_input(&json!({
            "message": "great! how many of those are from google",
            "conversation_history": [
                {
                    "role": "user",
                    "content": "can you give me a count of the current list of identified spam emails?"
                },
                {
                    "role": "agent",
                    "content": "There are 115 identified spam emails in the current synced data."
                }
            ]
        }));
        assert_eq!(followup_args["sender"], "google");
        assert_eq!(followup_args["spam_status"], "spam");
        assert!(followup_args.get("query").is_none());

        let followup_with_older_not_spam = count_emails_tool_args_for_input(&json!({
            "message": "how many of those are from google?",
            "conversation_history": [
                {
                    "role": "agent",
                    "content": "The first five database rows are all marked not-spam."
                },
                {
                    "role": "user",
                    "content": "how many spam emails are currently identified?"
                },
                {
                    "role": "agent",
                    "content": "There are currently 115 spam emails identified in your synced mailbox."
                }
            ]
        }));
        assert_eq!(followup_with_older_not_spam["sender"], "google");
        assert_eq!(followup_with_older_not_spam["spam_status"], "spam");
        assert!(followup_with_older_not_spam.get("query").is_none());
    }

    #[test]
    fn test_mail_assistant_database_sample_requests_require_list_tool() {
        assert!(should_require_synced_email_list_tool(
            "mail-assistant",
            &json!({ "message": "ok can you tell me about the first 5 in the database?" })
        ));
        assert!(should_require_synced_email_list_tool(
            "mail-assistant",
            &json!({ "message": "ok what can you tell me?" })
        ));
        assert!(!should_require_synced_email_list_tool(
            "mail-assistant",
            &json!({ "message": "How many emails are from GitHub?" })
        ));
        assert!(!should_require_synced_email_list_tool(
            "digest-agent",
            &json!({ "message": "ok what can you tell me?" })
        ));
    }

    #[test]
    fn test_synced_email_list_tool_args_extracts_limit() {
        assert_eq!(
            synced_email_list_tool_args_for_input(
                &json!({ "message": "tell me about the first 5 in the database" })
            )["limit"],
            5
        );
        assert_eq!(
            synced_email_list_tool_args_for_input(
                &json!({ "message": "show me the first ten synced messages" })
            )["limit"],
            10
        );
        assert_eq!(
            synced_email_list_tool_args_for_input(&json!({ "message": "what can you tell me?" }))
                ["limit"],
            5
        );
        assert_eq!(
            synced_email_list_tool_args_for_input(
                &json!({ "message": "show me the first 5 identified spam emails" })
            )["spam_status"],
            "spam"
        );
    }

    #[test]
    fn test_deferred_tool_promise_detection() {
        assert!(output_promises_deferred_tool_work(&json!({
            "response_markdown": "I will run that count now. Please wait while I retrieve it.",
            "summary": "Deferred count",
        })));
        assert!(!output_promises_deferred_tool_work(&json!({
            "response_markdown": "There are 0 matching spam emails from Google in the current synced data.",
            "summary": "Answered count",
        })));
    }

    #[test]
    fn test_interactive_mail_assistant_uses_partial_db_context() {
        assert!(should_use_partial_db_for_interactive_mail_assistant(
            "mail-assistant",
            "api-chat-mail-assistant-thread-1"
        ));
        assert!(!should_use_partial_db_for_interactive_mail_assistant(
            "mail-assistant",
            "message-id-1"
        ));
        assert!(!should_use_partial_db_for_interactive_mail_assistant(
            "email-analyzer",
            "api-chat-email-analyzer-thread-1"
        ));
    }

    fn ready_db_snapshot() -> DbCompletenessSnapshot {
        DbCompletenessSnapshot {
            folder_count: 4,
            selectable_folders_missing_counts: 0,
            largest_folder_message_count: 42,
            email_count: 42,
            missing_message_id: 0,
            body_missing: 0,
            analysis_missing: 0,
            embedding_missing: 0,
            location_missing: 0,
            filing_pending: 0,
            body_sync: Default::default(),
        }
    }

    #[test]
    fn test_classify_db_prerequisite_work_for_empty_db() {
        let snapshot = DbCompletenessSnapshot {
            folder_count: 0,
            email_count: 0,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::SyncFull]
        );
    }

    #[test]
    fn test_classify_db_prerequisite_work_allows_observed_empty_mailbox() {
        let snapshot = DbCompletenessSnapshot {
            folder_count: 6,
            largest_folder_message_count: 0,
            email_count: 0,
            ..ready_db_snapshot()
        };

        assert!(classify_db_prerequisite_work(&snapshot).is_empty());
    }

    #[test]
    fn test_classify_db_prerequisite_work_requires_observed_folder_counts() {
        let snapshot = DbCompletenessSnapshot {
            folder_count: 6,
            selectable_folders_missing_counts: 2,
            largest_folder_message_count: 0,
            email_count: 0,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::SyncFull]
        );
    }

    #[test]
    fn test_classify_db_prerequisite_work_for_partial_sync() {
        let snapshot = DbCompletenessSnapshot {
            largest_folder_message_count: 1000,
            email_count: 29,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::SyncFull]
        );
    }

    #[test]
    fn test_classify_db_prerequisite_work_ignores_missing_message_id_anomalies() {
        let snapshot = DbCompletenessSnapshot {
            missing_message_id: 1,
            ..ready_db_snapshot()
        };

        assert!(classify_db_prerequisite_work(&snapshot).is_empty());
    }

    #[test]
    fn test_classify_db_prerequisite_work_for_missing_body() {
        let snapshot = DbCompletenessSnapshot {
            body_missing: 3,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::SyncBody]
        );
    }

    #[test]
    fn test_classify_db_prerequisite_work_for_missing_analysis() {
        let snapshot = DbCompletenessSnapshot {
            analysis_missing: 2,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::Analyze]
        );
    }

    #[test]
    fn test_classify_db_prerequisite_work_for_missing_location() {
        let snapshot = DbCompletenessSnapshot {
            location_missing: 1,
            ..ready_db_snapshot()
        };

        assert_eq!(
            classify_db_prerequisite_work(&snapshot),
            vec![CoreWorkType::Locate]
        );
    }

    #[test]
    fn test_db_prerequisites_pending_output_is_chat_safe() {
        let pending = DbCompletenessPending {
            snapshot: DbCompletenessSnapshot {
                folder_count: 283,
                email_count: 939,
                missing_message_id: 1,
                analysis_missing: 931,
                location_missing: 4,
                ..ready_db_snapshot()
            },
            requested_work: vec![CoreWorkType::Analyze, CoreWorkType::Locate],
            stable_polls: 2,
            elapsed: Duration::from_secs(15),
            max_wait: Duration::from_secs(15),
        };

        let output = db_prerequisites_pending_output(
            "default",
            "mail-assistant",
            true,
            pending_retry_after_seconds(&pending.snapshot),
            &pending,
        );

        assert_eq!(output["status"], DB_COMPLETENESS_PENDING_STATUS);
        assert_eq!(output["needs_specialist"], false);
        assert_eq!(output["confidence"], 1.0);
        assert_eq!(output["db_completeness"]["analysis_missing"], 931);
        assert_eq!(output["requested_work"][0], "analyze");
        let markdown = output["response_markdown"].as_str().unwrap_or_default();
        assert!(markdown.contains("931 emails still need classification"));
        assert!(markdown.contains("try again in about 30 seconds"));
    }
}
