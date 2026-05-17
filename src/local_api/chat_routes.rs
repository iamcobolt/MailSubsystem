use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::extract::{
    ws::{WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent_catalog;
use crate::agent_router::{self, ExecutionPlan};
use crate::agent_runtime;
use crate::ai_analysis;
use crate::api::state::ApiState;
use crate::db::{ConversationMessage, ConversationMessageInsert, EmailRecord, ThreadSummary};
use crate::harness::{HarnessEvent, HarnessEventCallback, RunResult};

use super::api_error::{ApiError, ApiResult};
#[cfg(test)]
use super::chat_formatting::format_agent_response;
use super::chat_formatting::{
    extract_chat_confidence, format_chat_agent_response, is_db_prerequisites_pending_output,
};
use super::chat_streaming::{
    emit_stream_event, receive_stream_request, send_ws_event, ChatEventSender,
    MAX_CHAT_STREAM_EVENTS_BUFFER,
};
#[cfg(test)]
use super::chat_streaming::{parse_stream_request_message, MAX_CHAT_STREAM_REQUEST_BYTES};
#[cfg(test)]
use crate::harness::DB_COMPLETENESS_PENDING_STATUS;
#[cfg(test)]
use axum::extract::ws::Message as WsMessage;

const DEFAULT_THREAD_LIST_LIMIT: usize = 50;
const DEFAULT_MESSAGE_LIST_LIMIT: usize = 200;
const MAX_LIST_LIMIT: usize = 500;
const CHAT_HISTORY_LIMIT: usize = 100;
const MAX_AGENT_NAME_CHARS: usize = 64;
const MAX_THREAD_ID_CHARS: usize = 128;
const MAX_CONTEXT_EMAIL_ID_CHARS: usize = 512;
const MAX_CHAT_MESSAGE_CHARS: usize = 20_000;
const MAX_CHAT_EXECUTION_DURATION: Duration = Duration::from_secs(600);

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub thread_id: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    pub message: String,
    pub context_email_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub thread_id: String,
    pub message_id: String,
    pub agent_response: String,
    pub agent_run_id: Option<String>,
    pub confidence: Option<f64>,
    pub visible_agent_name: String,
    pub execution_agent_name: String,
    pub routing_reason: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatStreamEvent {
    Ready,
    ThreadReady {
        thread_id: String,
        user_message_id: String,
    },
    RunStarted {
        run_id: String,
        task_id: String,
        agent_name: String,
        visible_agent_name: Option<String>,
        routing_reason: Option<String>,
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
        preview: String,
        latency_ms: u64,
    },
    AssistantDelta {
        delta: String,
    },
    AssistantCompleted {
        thread_id: String,
        message_id: String,
        agent_response: String,
        agent_run_id: Option<String>,
        confidence: Option<f64>,
        visible_agent_name: Option<String>,
        execution_agent_name: Option<String>,
        routing_reason: Option<String>,
    },
    Error {
        message: String,
    },
    Done,
}

#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct MessageListQuery {
    pub limit: Option<usize>,
}

impl ChatRequest {
    pub(super) fn validate(&self) -> ApiResult<()> {
        validate_optional_field(
            "agent_name",
            self.agent_name.as_deref(),
            MAX_AGENT_NAME_CHARS,
        )?;
        validate_required_field("message", &self.message, MAX_CHAT_MESSAGE_CHARS)?;
        validate_optional_field("thread_id", self.thread_id.as_deref(), MAX_THREAD_ID_CHARS)?;
        validate_optional_field(
            "context_email_id",
            self.context_email_id.as_deref(),
            MAX_CONTEXT_EMAIL_ID_CHARS,
        )?;
        Ok(())
    }
}

pub async fn post_chat(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<ChatRequest>,
) -> ApiResult<Json<ChatResponse>> {
    request.validate()?;
    if !state.allow_chat_request() {
        return Err(ApiError::too_many_requests(
            "too many chat requests; try again shortly",
        ));
    }
    execute_chat_request_with_timeout(state, request, None)
        .await
        .map(Json)
}

pub async fn ws_chat_stream(ws: WebSocketUpgrade, State(state): State<Arc<ApiState>>) -> Response {
    if !state.allow_chat_request() {
        return ApiError::too_many_requests("too many chat requests; try again shortly")
            .into_response();
    }

    let Some(permit) = state.try_acquire_chat_stream() else {
        return ApiError::too_many_requests("too many active chat streams").into_response();
    };

    ws.on_upgrade(move |socket| handle_chat_stream_socket(socket, state, permit))
}

async fn handle_chat_stream_socket(
    mut socket: WebSocket,
    state: Arc<ApiState>,
    _permit: crate::api::state::ChatStreamPermit,
) {
    if send_ws_event(&mut socket, &ChatStreamEvent::Ready)
        .await
        .is_err()
    {
        return;
    }

    let request = match receive_stream_request(&mut socket).await {
        Ok(request) => request,
        Err(error) => {
            let _ = send_ws_event(
                &mut socket,
                &ChatStreamEvent::Error {
                    message: error.message,
                },
            )
            .await;
            let _ = socket.close().await;
            return;
        }
    };

    let (tx, mut rx) = mpsc::channel::<ChatStreamEvent>(MAX_CHAT_STREAM_EVENTS_BUFFER);
    let state_for_task = state.clone();
    let tx_for_task = tx.clone();
    tokio::spawn(async move {
        match execute_chat_request_with_timeout(state_for_task, request, Some(tx_for_task.clone()))
            .await
        {
            Ok(response) => {
                let _ = tx_for_task
                    .send(ChatStreamEvent::AssistantCompleted {
                        thread_id: response.thread_id,
                        message_id: response.message_id,
                        agent_response: response.agent_response,
                        agent_run_id: response.agent_run_id,
                        confidence: response.confidence,
                        visible_agent_name: Some(response.visible_agent_name),
                        execution_agent_name: Some(response.execution_agent_name),
                        routing_reason: Some(response.routing_reason),
                    })
                    .await;
                let _ = tx_for_task.send(ChatStreamEvent::Done).await;
            }
            Err(error) => {
                let _ = tx_for_task
                    .send(ChatStreamEvent::Error {
                        message: error.message,
                    })
                    .await;
            }
        }
    });

    while let Some(event) = rx.recv().await {
        if send_ws_event(&mut socket, &event).await.is_err() {
            break;
        }
        if matches!(event, ChatStreamEvent::Done | ChatStreamEvent::Error { .. }) {
            break;
        }
    }

    let _ = socket.close().await;
}

async fn execute_chat_request_with_timeout(
    state: Arc<ApiState>,
    request: ChatRequest,
    stream: Option<ChatEventSender>,
) -> ApiResult<ChatResponse> {
    tokio::time::timeout(
        MAX_CHAT_EXECUTION_DURATION,
        execute_chat_request(state, request, stream),
    )
    .await
    .map_err(|_| ApiError::request_timeout("chat request timed out"))?
}

pub async fn list_threads(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<PaginationQuery>,
) -> ApiResult<Json<Vec<ThreadSummary>>> {
    let limit = normalize_limit(query.limit.unwrap_or(DEFAULT_THREAD_LIST_LIMIT));
    let offset = query.offset.unwrap_or(0);
    let threads = state
        .db
        .list_threads_for_account(&state.account_id, limit, offset)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(threads))
}

pub async fn list_thread_messages(
    State(state): State<Arc<ApiState>>,
    Path(thread_id): Path<String>,
    Query(query): Query<MessageListQuery>,
) -> ApiResult<Json<Vec<ConversationMessage>>> {
    let thread_id = thread_id.trim().to_string();
    if thread_id.is_empty() {
        return Err(ApiError::bad_request("thread_id must not be empty"));
    }

    let exists = state
        .db
        .get_thread_for_account(&state.account_id, &thread_id)
        .await
        .map_err(ApiError::internal)?
        .is_some();
    if !exists {
        return Err(ApiError::not_found(format!(
            "thread not found: {}",
            thread_id
        )));
    }

    let limit = normalize_limit(query.limit.unwrap_or(DEFAULT_MESSAGE_LIST_LIMIT));
    let messages = state
        .db
        .get_thread_messages_for_account(&state.account_id, &thread_id, limit)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(messages))
}

pub async fn delete_thread(
    State(state): State<Arc<ApiState>>,
    Path(thread_id): Path<String>,
) -> ApiResult<StatusCode> {
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return Err(ApiError::bad_request("thread_id must not be empty"));
    }

    state
        .db
        .delete_thread_for_account(&state.account_id, thread_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn execute_chat_request(
    state: Arc<ApiState>,
    request: ChatRequest,
    stream: Option<ChatEventSender>,
) -> ApiResult<ChatResponse> {
    request.validate()?;

    let account_id = state.account_id.clone();
    let requested_agent_name = trim_optional(&request.agent_name);

    let user_message = request.message.trim().to_string();
    let requested_thread_id = trim_optional(&request.thread_id);
    let requested_context_email_id = trim_optional(&request.context_email_id);
    let mut context_email = match requested_context_email_id.as_deref() {
        Some(message_id) => Some(load_context_email(&state, message_id).await?),
        None => None,
    };

    let (thread_id, visible_agent_name) = if let Some(thread_id) = requested_thread_id {
        let existing = state
            .db
            .get_thread_for_account(&account_id, &thread_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("thread not found: {}", thread_id)))?;
        let visible_agent_name =
            resolve_thread_visible_agent(&existing, requested_agent_name.as_deref())?;

        if context_email.is_none() {
            if let Some(message_id) = existing.context_email_id.as_deref() {
                context_email = Some(load_context_email(&state, message_id).await?);
            }
        } else if let Some(existing_message_id) = existing.context_email_id.as_deref() {
            if context_email
                .as_ref()
                .map(|email| email.message_id.as_str())
                != Some(existing_message_id)
            {
                return Err(ApiError::bad_request(
                    "context_email_id does not match the existing thread context",
                ));
            }
        }

        if existing.title.is_none() {
            if let Some(title) = default_thread_title(&user_message, context_email.as_ref()) {
                state
                    .db
                    .update_thread_title_for_account(&account_id, &thread_id, Some(&title))
                    .await
                    .map_err(ApiError::internal)?;
            }
        }

        (thread_id, visible_agent_name)
    } else {
        let requested_agent_name = requested_agent_name
            .as_deref()
            .unwrap_or(agent_catalog::DEFAULT_AGENT_ID);
        let requested_agent = agent_catalog::find_agent(requested_agent_name).ok_or_else(|| {
            ApiError::bad_request(format!("unknown agent '{}'", requested_agent_name))
        })?;
        if requested_agent.advanced_only && !direct_subagent_chat_enabled() {
            return Err(ApiError::bad_request(
                "Mail Assistant is the only user-facing agent; sub-agents are internal workers",
            ));
        }
        let visible_agent_name = requested_agent.id;
        let thread_id = Uuid::new_v4().to_string();
        let title = default_thread_title(&user_message, context_email.as_ref());
        state
            .db
            .create_thread_for_account(
                &account_id,
                &thread_id,
                &visible_agent_name,
                title.as_deref(),
                context_email
                    .as_ref()
                    .map(|email| email.message_id.as_str()),
            )
            .await
            .map_err(ApiError::internal)?;
        (thread_id, visible_agent_name)
    };

    let user_message_id = Uuid::new_v4().to_string();
    state
        .db
        .add_message_for_account(ConversationMessageInsert {
            account_id: &account_id,
            message_id: &user_message_id,
            thread_id: &thread_id,
            role: "user",
            content: &user_message,
            agent_name: None,
            agent_run_id: None,
        })
        .await
        .map_err(ApiError::internal)?;

    emit_stream_event(
        stream.as_ref(),
        ChatStreamEvent::ThreadReady {
            thread_id: thread_id.clone(),
            user_message_id: user_message_id.clone(),
        },
    )
    .await;

    let history = state
        .db
        .get_thread_messages_for_account(&account_id, &thread_id, CHAT_HISTORY_LIMIT)
        .await
        .map_err(ApiError::internal)?;
    let prior_history = history_without_message(&history, &user_message_id);
    let execution_plan = agent_router::resolve_execution_plan(
        &visible_agent_name,
        &user_message,
        &prior_history,
        context_email.as_ref(),
    );
    let harness_callback = stream
        .as_ref()
        .map(|sender| build_harness_stream_callback(sender.clone(), execution_plan.clone()));
    let run_result = run_chat_execution_plan(ChatExecutionRequest {
        state: &state,
        account_id: &account_id,
        thread_id: &thread_id,
        execution_plan: &execution_plan,
        user_message: &user_message,
        history: &prior_history,
        context_email: context_email.as_ref(),
        harness_callback,
    })
    .await
    .map_err(ApiError::internal)?;
    let agent_response = format_chat_agent_response(&execution_plan, &run_result.output)
        .map_err(ApiError::internal)?;
    let agent_message_id = Uuid::new_v4().to_string();

    state
        .db
        .add_message_for_account(ConversationMessageInsert {
            account_id: &account_id,
            message_id: &agent_message_id,
            thread_id: &thread_id,
            role: "agent",
            content: &agent_response,
            agent_name: Some(&visible_agent_name),
            agent_run_id: Some(&run_result.run_id),
        })
        .await
        .map_err(ApiError::internal)?;

    Ok(ChatResponse {
        thread_id,
        message_id: agent_message_id,
        agent_response,
        agent_run_id: Some(run_result.run_id),
        confidence: extract_chat_confidence(&run_result.output),
        visible_agent_name: execution_plan.visible_agent_name.clone(),
        execution_agent_name: response_execution_agent_name(&execution_plan),
        routing_reason: response_routing_reason(&execution_plan),
    })
}

fn resolve_thread_visible_agent(
    thread: &ThreadSummary,
    requested_agent_name: Option<&str>,
) -> ApiResult<String> {
    let Some(requested_agent_name) = requested_agent_name else {
        if !direct_subagent_chat_enabled()
            && agent_catalog::find_agent(&thread.agent_name)
                .is_some_and(|agent| agent.advanced_only)
        {
            return Ok(agent_catalog::DEFAULT_AGENT_ID.to_string());
        }
        return Ok(thread.agent_name.clone());
    };
    let normalized = agent_catalog::find_agent(requested_agent_name).ok_or_else(|| {
        ApiError::bad_request(format!("unknown agent '{}'", requested_agent_name))
    })?;
    if normalized.advanced_only && !direct_subagent_chat_enabled() {
        return Err(ApiError::bad_request(
            "Mail Assistant is the only user-facing agent; sub-agents are internal workers",
        ));
    }
    let normalized = normalized.id;

    if thread
        .agent_name
        .eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID)
    {
        if normalized.eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID) {
            return Ok(thread.agent_name.clone());
        }
        return Err(ApiError::bad_request(format!(
            "thread '{}' uses visible agent '{}'; continue it through Mail Assistant instead of requesting '{}'",
            thread.thread_id, thread.agent_name, normalized
        )));
    }

    if !direct_subagent_chat_enabled()
        && agent_catalog::find_agent(&thread.agent_name).is_some_and(|agent| agent.advanced_only)
        && normalized.eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID)
    {
        return Ok(agent_catalog::DEFAULT_AGENT_ID.to_string());
    }

    if thread.agent_name.eq_ignore_ascii_case(&normalized) {
        return Ok(thread.agent_name.clone());
    }

    Err(ApiError::bad_request(format!(
        "thread '{}' belongs to agent '{}', not '{}'",
        thread.thread_id, thread.agent_name, normalized
    )))
}

fn direct_subagent_chat_enabled() -> bool {
    agent_catalog::direct_subagent_chat_enabled()
}

fn build_harness_stream_callback(
    sender: ChatEventSender,
    execution_plan: ExecutionPlan,
) -> HarnessEventCallback {
    Arc::new(move |event| match event {
        HarnessEvent::RunStarted {
            run_id,
            task_id,
            agent_name,
        } => {
            let _ = sender.try_send(ChatStreamEvent::RunStarted {
                run_id,
                task_id,
                agent_name,
                visible_agent_name: Some(execution_plan.visible_agent_name.clone()),
                routing_reason: Some(execution_plan.routing_reason.clone()),
            });
        }
        HarnessEvent::StepStarted { step } => {
            let _ = sender.try_send(ChatStreamEvent::StepStarted { step });
        }
        HarnessEvent::ToolCall {
            step,
            tool_name,
            arguments,
        } => {
            let _ = sender.try_send(ChatStreamEvent::ToolCall {
                step,
                tool_name,
                arguments,
            });
        }
        HarnessEvent::ToolResult {
            step,
            tool_name,
            result,
            latency_ms,
        } => {
            let _ = sender.try_send(ChatStreamEvent::ToolResult {
                step,
                tool_name,
                preview: truncate_str(&result, 500).to_string(),
                latency_ms,
            });
        }
        HarnessEvent::FinalOutput { output } => {
            if let Ok(formatted) = format_chat_agent_response(&execution_plan, &output) {
                for delta in split_stream_text(&formatted) {
                    let _ = sender.try_send(ChatStreamEvent::AssistantDelta { delta });
                }
            }
        }
        HarnessEvent::Error { .. } => {}
    })
}

async fn load_context_email(state: &ApiState, message_id: &str) -> ApiResult<EmailRecord> {
    state
        .db
        .get_email_by_message_id_for_account(&state.account_id, message_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("context email not found: {}", message_id)))
}

fn should_synthesize_with_mail_assistant(execution_plan: &ExecutionPlan) -> bool {
    execution_plan
        .visible_agent_name
        .eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID)
        && !execution_plan
            .execution_agent_name
            .eq_ignore_ascii_case(agent_catalog::DEFAULT_AGENT_ID)
}

fn response_execution_agent_name(execution_plan: &ExecutionPlan) -> String {
    if should_synthesize_with_mail_assistant(execution_plan) {
        agent_catalog::DEFAULT_AGENT_ID.to_string()
    } else {
        execution_plan.execution_agent_name.clone()
    }
}

fn response_routing_reason(execution_plan: &ExecutionPlan) -> String {
    if should_synthesize_with_mail_assistant(execution_plan) {
        format!(
            "Mail Assistant handled the user response after internal specialist work. {}",
            execution_plan.routing_reason
        )
    } else {
        execution_plan.routing_reason.clone()
    }
}

fn add_specialist_context_to_mail_assistant_input(
    input: &mut Value,
    execution_plan: &ExecutionPlan,
    specialist_result: &RunResult,
) -> Result<()> {
    let specialist_result_markdown =
        format_chat_agent_response(execution_plan, &specialist_result.output).ok();
    let object = input
        .as_object_mut()
        .context("mail assistant synthesis input must be an object")?;

    object.insert(
        "head_agent_instruction".to_string(),
        Value::String(
            "You are the user-facing Mail Assistant. Use internal_specialist as private evidence, synthesize the answer in your own voice, and do not expose backend agent topology unless the user explicitly asks about architecture."
                .to_string(),
        ),
    );
    object.insert(
        "internal_specialist".to_string(),
        json!({
            "agent_name": execution_plan.execution_agent_name,
            "routing_reason": execution_plan.routing_reason,
            "run_id": specialist_result.run_id,
            "confidence": extract_chat_confidence(&specialist_result.output),
            "result": specialist_result.output,
            "result_markdown": specialist_result_markdown,
        }),
    );
    Ok(())
}

struct ChatExecutionRequest<'a> {
    state: &'a ApiState,
    account_id: &'a str,
    thread_id: &'a str,
    execution_plan: &'a ExecutionPlan,
    user_message: &'a str,
    history: &'a [ConversationMessage],
    context_email: Option<&'a EmailRecord>,
    harness_callback: Option<HarnessEventCallback>,
}

impl<'a> ChatExecutionRequest<'a> {
    fn without_callback(&self) -> Self {
        Self {
            state: self.state,
            account_id: self.account_id,
            thread_id: self.thread_id,
            execution_plan: self.execution_plan,
            user_message: self.user_message,
            history: self.history,
            context_email: self.context_email,
            harness_callback: None,
        }
    }
}

async fn run_chat_execution_plan(request: ChatExecutionRequest<'_>) -> Result<RunResult> {
    if should_synthesize_with_mail_assistant(request.execution_plan) {
        return run_mail_assistant_head_synthesis(request).await;
    }

    if request.execution_plan.execution_agent_name == "orchestrator" {
        return run_chat_orchestrator_review(request).await;
    }

    run_chat_specialist(request).await
}

async fn run_mail_assistant_head_synthesis(request: ChatExecutionRequest<'_>) -> Result<RunResult> {
    let specialist_request = request.without_callback();
    let specialist_result = if request.execution_plan.execution_agent_name == "orchestrator" {
        run_chat_orchestrator_review(specialist_request).await?
    } else {
        run_chat_specialist(specialist_request).await?
    };

    if is_db_prerequisites_pending_output(&specialist_result.output) {
        return Ok(specialist_result);
    }

    let head_plan = ExecutionPlan {
        visible_agent_name: agent_catalog::DEFAULT_AGENT_ID.to_string(),
        execution_agent_name: agent_catalog::DEFAULT_AGENT_ID.to_string(),
        routing_reason: format!(
            "Mail Assistant synthesized internal {} output. {}",
            request.execution_plan.execution_agent_name, request.execution_plan.routing_reason
        ),
    };
    let mut task_input = build_chat_task_input(
        request.account_id,
        request.thread_id,
        &head_plan,
        request.user_message,
        request.history,
        request.context_email,
    );
    add_specialist_context_to_mail_assistant_input(
        &mut task_input,
        request.execution_plan,
        &specialist_result,
    )?;

    let task_id = format!(
        "api-chat-{}-head-{}-{}",
        agent_catalog::DEFAULT_AGENT_ID,
        request.thread_id,
        Utc::now().timestamp_millis()
    );
    agent_runtime::run_named_agent_with_callback(
        request.state.db.clone(),
        request.account_id,
        agent_catalog::DEFAULT_AGENT_ID,
        &task_id,
        task_input,
        request.harness_callback,
    )
    .await
}

async fn run_chat_specialist(request: ChatExecutionRequest<'_>) -> Result<RunResult> {
    let task_input = build_chat_task_input(
        request.account_id,
        request.thread_id,
        request.execution_plan,
        request.user_message,
        request.history,
        request.context_email,
    );
    let task_id = format!(
        "api-chat-{}-{}-{}",
        request.execution_plan.execution_agent_name,
        request.thread_id,
        Utc::now().timestamp_millis()
    );
    agent_runtime::run_named_agent_with_callback(
        request.state.db.clone(),
        request.account_id,
        &request.execution_plan.execution_agent_name,
        &task_id,
        task_input,
        request.harness_callback,
    )
    .await
}

async fn run_chat_orchestrator_review(request: ChatExecutionRequest<'_>) -> Result<RunResult> {
    let context_email = request
        .context_email
        .context("orchestrator chat route requires a context email")?;
    let worker_plan = ExecutionPlan {
        visible_agent_name: request.execution_plan.visible_agent_name.clone(),
        execution_agent_name: "email-analyzer".to_string(),
        routing_reason: "Prepared single-email analysis for explicit higher-judgment review."
            .to_string(),
    };
    let worker_input = build_chat_task_input(
        request.account_id,
        request.thread_id,
        &worker_plan,
        request.user_message,
        request.history,
        Some(context_email),
    );
    let worker_task_id = format!(
        "api-chat-email-analyzer-prep-{}-{}",
        request.thread_id,
        Utc::now().timestamp_millis()
    );
    let worker_result = agent_runtime::run_named_agent(
        request.state.db.clone(),
        request.account_id,
        "email-analyzer",
        &worker_task_id,
        worker_input,
    )
    .await?;
    if is_db_prerequisites_pending_output(&worker_result.output) {
        return Ok(worker_result);
    }

    let mut orchestrator_input = ai_analysis::EmailAnalyzer::build_orchestrator_escalation_input(
        request.account_id,
        context_email,
        &worker_result,
        Some(&format!(
            "User requested an explicit higher-judgment review in chat: {}",
            request.user_message
        )),
        ai_analysis::EmailAnalyzer::load_email_analyzer_scratchpad_context(
            request.state.db.as_ref(),
            request.account_id,
            context_email.sender.as_deref(),
        )
        .await,
    );
    if let Some(object) = orchestrator_input.as_object_mut() {
        object.insert(
            "thread_id".to_string(),
            Value::String(request.thread_id.to_string()),
        );
        object.insert(
            "visible_agent_name".to_string(),
            Value::String(request.execution_plan.visible_agent_name.clone()),
        );
        object.insert(
            "execution_agent_name".to_string(),
            Value::String(request.execution_plan.execution_agent_name.clone()),
        );
        object.insert(
            "routing_reason".to_string(),
            Value::String(request.execution_plan.routing_reason.clone()),
        );
    }
    if let Some(scratchpad_context) = orchestrator_input
        .get_mut("scratchpad_context")
        .and_then(Value::as_object_mut)
    {
        scratchpad_context.insert(
            "conversation_history".to_string(),
            Value::Array(serialize_conversation_history(request.history)),
        );
        scratchpad_context.insert(
            "thread_id".to_string(),
            Value::String(request.thread_id.to_string()),
        );
        scratchpad_context.insert(
            "requested_by".to_string(),
            Value::String("mail-assistant-chat".to_string()),
        );
    }

    let task_id = format!(
        "api-chat-{}-{}-{}",
        request.execution_plan.execution_agent_name,
        request.thread_id,
        Utc::now().timestamp_millis()
    );
    agent_runtime::run_named_agent_with_callback(
        request.state.db.clone(),
        request.account_id,
        &request.execution_plan.execution_agent_name,
        &task_id,
        orchestrator_input,
        request.harness_callback,
    )
    .await
}

fn build_chat_task_input(
    account_id: &str,
    thread_id: &str,
    execution_plan: &ExecutionPlan,
    user_message: &str,
    history: &[ConversationMessage],
    context_email: Option<&EmailRecord>,
) -> Value {
    let mut input = context_email
        .map(chat_input_from_email)
        .unwrap_or_else(|| json!({}));
    let object = input
        .as_object_mut()
        .expect("chat task input must always be an object");

    object.insert(
        "account_id".to_string(),
        Value::String(account_id.to_string()),
    );
    object.insert(
        "thread_id".to_string(),
        Value::String(thread_id.to_string()),
    );
    object.insert(
        "agent_name".to_string(),
        Value::String(execution_plan.execution_agent_name.clone()),
    );
    object.insert(
        "visible_agent_name".to_string(),
        Value::String(execution_plan.visible_agent_name.clone()),
    );
    object.insert(
        "execution_agent_name".to_string(),
        Value::String(execution_plan.execution_agent_name.clone()),
    );
    object.insert(
        "routing_reason".to_string(),
        Value::String(execution_plan.routing_reason.clone()),
    );
    object.insert(
        "message".to_string(),
        Value::String(user_message.to_string()),
    );
    object.insert(
        "user_message".to_string(),
        Value::String(user_message.to_string()),
    );
    object.insert(
        "conversation_history".to_string(),
        Value::Array(serialize_conversation_history(history)),
    );
    if let Some(email) = context_email {
        object.insert(
            "context_email_id".to_string(),
            Value::String(email.message_id.clone()),
        );
    }
    if execution_plan.execution_agent_name == "digest-agent" {
        let window = agent_router::infer_digest_window(user_message, history);
        let since = if window == "daily" {
            Utc::now() - ChronoDuration::days(1)
        } else {
            Utc::now() - ChronoDuration::days(7)
        };
        object.insert("window".to_string(), Value::String(window.to_string()));
        object.insert("since".to_string(), Value::String(since.to_rfc3339()));
    }

    input
}

fn serialize_conversation_history(history: &[ConversationMessage]) -> Vec<Value> {
    history
        .iter()
        .map(|message| {
            json!({
                "message_id": message.message_id,
                "role": message.role,
                "content": message.content,
                "agent_name": message.agent_name,
                "agent_run_id": message.agent_run_id,
                "created_at": message.created_at.to_rfc3339(),
            })
        })
        .collect()
}

fn history_without_message(
    history: &[ConversationMessage],
    message_id: &str,
) -> Vec<ConversationMessage> {
    history
        .iter()
        .filter(|message| message.message_id != message_id)
        .cloned()
        .collect()
}

fn chat_input_from_email(email: &EmailRecord) -> Value {
    json!({
        "message_id": email.message_id,
        "subject": email.subject.as_deref(),
        "sender": email.sender.as_deref(),
        "received_date": email.received_date.as_ref().map(|date| date.to_rfc3339()),
        "body_text": email
            .body_text
            .as_deref()
            .map(|body| truncate_str(body, 12_000).to_string()),
        "message_size": email.message_size,
        "thread_ids": &email.related_message_ids,
        "is_read": email.is_read,
        "list_id": extract_header_value(email.raw_email_content.as_deref(), "list-id"),
        "list_unsubscribe": extract_header_value(
            email.raw_email_content.as_deref(),
            "list-unsubscribe",
        ),
        "x_priority": extract_header_value(email.raw_email_content.as_deref(), "x-priority"),
        "reply_to": extract_header_value(email.raw_email_content.as_deref(), "reply-to"),
        "spam_status": email.spam_status,
        "phishing_status": email.phishing_status,
        "marketing_status": email.marketing_status,
        "otp_status": email.otp_status,
        "otp_code": email.otp_code,
        "threat_level": email.threat_level,
        "threat_indicators": email.threat_indicators,
        "human_summary": email.human_summary,
        "category": email.category,
        "subcategory": email.subcategory,
        "organization": email.organization,
        "topic": email.topic,
        "location": email.location,
        "location_recommendation": email.location_recommendation,
        "email_type": email.email_type,
    })
}

fn default_thread_title(user_message: &str, context_email: Option<&EmailRecord>) -> Option<String> {
    let trimmed = user_message.trim();
    if !trimmed.is_empty() {
        return Some(truncate_str(trimmed.lines().next().unwrap_or(trimmed), 80).to_string());
    }

    context_email
        .and_then(|email| email.subject.as_deref())
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(|subject| truncate_str(subject, 80).to_string())
}

fn normalize_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIST_LIMIT)
}

fn validate_required_field(field: &str, value: &str, max_chars: usize) -> ApiResult<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request(format!("{field} must not be empty")));
    }
    validate_char_limit(field, trimmed, max_chars)
}

fn validate_optional_field(field: &str, value: Option<&str>, max_chars: usize) -> ApiResult<()> {
    if let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) {
        validate_char_limit(field, trimmed, max_chars)?;
    }
    Ok(())
}

fn validate_char_limit(field: &str, value: &str, max_chars: usize) -> ApiResult<()> {
    let char_count = value.chars().count();
    if char_count > max_chars {
        return Err(ApiError::payload_too_large(format!(
            "{field} exceeds the {max_chars} character limit"
        )));
    }
    Ok(())
}

fn trim_optional(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn truncate_str(input: &str, max_chars: usize) -> &str {
    let end = input
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(input.len());
    &input[..end]
}

fn split_stream_text(input: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_is_whitespace: Option<bool> = None;

    for ch in input.chars() {
        let is_whitespace = ch.is_whitespace();
        if current_is_whitespace != Some(is_whitespace) && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current_is_whitespace = Some(is_whitespace);
        current.push(ch);
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

fn extract_header_value(raw: Option<&str>, header_name: &str) -> Option<String> {
    let raw = raw?;
    let needle = header_name.to_ascii_lowercase();
    let mut current_name: Option<String> = None;
    let mut current_value = String::new();

    let flush = |name: &Option<String>, value: &str| -> Option<String> {
        if name
            .as_ref()
            .map(|current| current.eq_ignore_ascii_case(&needle))
            .unwrap_or(false)
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    };

    for line in raw.lines() {
        if line.trim().is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            current_value.push(' ');
            current_value.push_str(line.trim());
            continue;
        }
        if let Some(found) = flush(&current_name, &current_value) {
            return Some(found);
        }
        if let Some((name, value)) = line.split_once(':') {
            current_name = Some(name.trim().to_string());
            current_value.clear();
            current_value.push_str(value.trim());
        } else {
            current_name = None;
            current_value.clear();
        }
    }

    flush(&current_name, &current_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
            raw_email_content: Some(
                "List-Id: updates.example.com\nReply-To: help@example.com\n\nBody".to_string(),
            ),
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

    #[test]
    fn build_chat_task_input_includes_history_and_context_email() {
        let email = sample_email();
        let history = vec![ConversationMessage {
            message_id: "chat-1".to_string(),
            thread_id: "thread-1".to_string(),
            role: "user".to_string(),
            content: "Summarize this".to_string(),
            agent_name: None,
            agent_run_id: None,
            created_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 1, 0).unwrap(),
        }];

        let input = build_chat_task_input(
            "default",
            "thread-1",
            &ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "email-analyzer".to_string(),
                routing_reason: "single email explanation".to_string(),
            },
            "Summarize this",
            &history,
            Some(&email),
        );

        assert_eq!(input["account_id"], "default");
        assert_eq!(input["thread_id"], "thread-1");
        assert_eq!(input["agent_name"], "email-analyzer");
        assert_eq!(input["visible_agent_name"], "mail-assistant");
        assert_eq!(input["execution_agent_name"], "email-analyzer");
        assert_eq!(input["message"], "Summarize this");
        assert_eq!(input["context_email_id"], "msg-1");
        assert_eq!(input["subject"], "Quarterly update");
        assert_eq!(
            input["conversation_history"][0]["content"],
            "Summarize this"
        );
        assert_eq!(input["reply_to"], "help@example.com");
    }

    #[test]
    fn history_without_message_excludes_the_current_turn() {
        let history = vec![
            ConversationMessage {
                message_id: "earlier".to_string(),
                thread_id: "thread-1".to_string(),
                role: "user".to_string(),
                content: "Earlier turn".to_string(),
                agent_name: None,
                agent_run_id: None,
                created_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 0, 0).unwrap(),
            },
            ConversationMessage {
                message_id: "current".to_string(),
                thread_id: "thread-1".to_string(),
                role: "user".to_string(),
                content: "Current turn".to_string(),
                agent_name: None,
                agent_run_id: None,
                created_at: Utc.with_ymd_and_hms(2026, 4, 18, 12, 1, 0).unwrap(),
            },
        ];

        let filtered = history_without_message(&history, "current");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message_id, "earlier");
    }

    #[test]
    fn default_thread_title_prefers_user_message() {
        let email = sample_email();
        let title = default_thread_title("  What changed this quarter?  ", Some(&email));
        assert_eq!(title.as_deref(), Some("What changed this quarter?"));
    }

    #[test]
    fn extract_header_value_handles_folded_headers() {
        let raw = "Subject: hi\nList-Unsubscribe: <mailto:test@example.com>,\n <https://example.com>\n\nBody";
        let value = extract_header_value(Some(raw), "list-unsubscribe");
        assert_eq!(
            value.as_deref(),
            Some("<mailto:test@example.com>, <https://example.com>")
        );
    }

    #[test]
    fn split_stream_text_preserves_original_content() {
        let input = "{\n  \"status\": \"ok\"\n}\n";
        let chunks = split_stream_text(input);
        assert_eq!(chunks.concat(), input);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn format_agent_response_prefers_response_markdown() {
        let formatted = format_agent_response(&json!({
            "response_markdown": "## Mail Assistant\n\nHere is the answer.",
            "confidence": 0.82
        }))
        .expect("format markdown response");
        assert_eq!(formatted, "## Mail Assistant\n\nHere is the answer.");
    }

    #[test]
    fn format_agent_response_falls_back_to_json() {
        let formatted = format_agent_response(&json!({
            "summary": "Fallback payload"
        }))
        .expect("format json response");
        assert!(formatted.contains("\"summary\": \"Fallback payload\""));
    }

    #[test]
    fn build_chat_task_input_adds_digest_window_for_digest_agent() {
        let input = build_chat_task_input(
            "default",
            "thread-1",
            &ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "digest-agent".to_string(),
                routing_reason: "digest".to_string(),
            },
            "Give me a weekly inbox digest.",
            &[],
            None,
        );

        assert_eq!(input["window"], "weekly");
        assert!(input["since"].as_str().is_some());
    }

    #[test]
    fn routed_mail_assistant_turn_reports_mail_assistant_as_response_agent() {
        let plan = ExecutionPlan {
            visible_agent_name: "mail-assistant".to_string(),
            execution_agent_name: "digest-agent".to_string(),
            routing_reason: "digest".to_string(),
        };

        assert!(should_synthesize_with_mail_assistant(&plan));
        assert_eq!(response_execution_agent_name(&plan), "mail-assistant");
        assert!(response_routing_reason(&plan).contains("internal specialist work"));
    }

    #[test]
    fn direct_specialist_turn_reports_specialist_as_response_agent() {
        let plan = ExecutionPlan {
            visible_agent_name: "digest-agent".to_string(),
            execution_agent_name: "digest-agent".to_string(),
            routing_reason: "direct".to_string(),
        };

        assert!(!should_synthesize_with_mail_assistant(&plan));
        assert_eq!(response_execution_agent_name(&plan), "digest-agent");
        assert_eq!(response_routing_reason(&plan), "direct");
    }

    #[test]
    fn mail_assistant_synthesis_input_embeds_specialist_result_privately() {
        let plan = ExecutionPlan {
            visible_agent_name: "mail-assistant".to_string(),
            execution_agent_name: "email-analyzer".to_string(),
            routing_reason: "single email explanation".to_string(),
        };
        let result = RunResult {
            run_id: "run-specialist-1".to_string(),
            output: json!({
                "human_summary": "This is a password reset email.",
                "spam_status": "not-spam",
                "phishing_status": "not-phishing",
                "marketing_status": "not-marketing",
                "otp_status": "otp",
                "otp_code": "123456",
                "threat_level": "none",
                "category": "work",
                "email_type": "actionable",
                "confidence": 0.91
            }),
            should_escalate: false,
            escalate_reason: None,
            llm_calls: 1,
            tool_calls: 0,
            input_tokens: None,
            output_tokens: None,
        };
        let mut input = json!({
            "agent_name": "mail-assistant",
            "message": "Is this safe?"
        });

        add_specialist_context_to_mail_assistant_input(&mut input, &plan, &result)
            .expect("add specialist context");

        assert_eq!(input["internal_specialist"]["agent_name"], "email-analyzer");
        assert_eq!(input["internal_specialist"]["run_id"], "run-specialist-1");
        assert_eq!(input["internal_specialist"]["confidence"], 0.91);
        assert!(input["head_agent_instruction"]
            .as_str()
            .unwrap_or_default()
            .contains("user-facing Mail Assistant"));
        assert!(input["internal_specialist"]["result_markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("## Mail Assistant"));
    }

    #[test]
    fn format_chat_agent_response_rewrites_email_analyzer_output_for_mail_assistant() {
        let formatted = format_chat_agent_response(
            &ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "email-analyzer".to_string(),
                routing_reason: "single email explanation".to_string(),
            },
            &json!({
                "human_summary": "This is a password reset email.",
                "spam_status": "not-spam",
                "phishing_status": "not-phishing",
                "marketing_status": "not-marketing",
                "otp_status": "otp",
                "otp_code": "123456",
                "threat_level": "none",
                "category": "work",
                "email_type": "actionable"
            }),
        )
        .expect("format routed email analyzer output");

        assert!(formatted.contains("## Mail Assistant"));
        assert!(formatted.contains("password reset"));
        assert!(formatted.contains("123456"));
    }

    #[test]
    fn format_chat_agent_response_preserves_head_agent_markdown_for_routed_turn() {
        let formatted = format_chat_agent_response(
            &ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "email-analyzer".to_string(),
                routing_reason: "single email explanation".to_string(),
            },
            &json!({
                "response_markdown": "## Mail Assistant\n\nThis is the final head-agent answer.",
                "summary": "Final answer",
                "confidence": 0.86,
                "needs_specialist": false
            }),
        )
        .expect("format head-agent output");

        assert_eq!(
            formatted,
            "## Mail Assistant\n\nThis is the final head-agent answer."
        );
    }

    #[test]
    fn format_chat_agent_response_preserves_pending_readiness_message() {
        let formatted = format_chat_agent_response(
            &ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "email-analyzer".to_string(),
                routing_reason: "single email explanation".to_string(),
            },
            &json!({
                "status": DB_COMPLETENESS_PENDING_STATUS,
                "response_markdown": "## Mail Assistant\n\nMailbox is still processing.",
                "confidence": 1.0
            }),
        )
        .expect("format pending readiness output");

        assert_eq!(
            formatted,
            "## Mail Assistant\n\nMailbox is still processing."
        );
    }

    #[test]
    fn extract_chat_confidence_reads_nested_orchestrator_result() {
        let confidence = extract_chat_confidence(&json!({
            "task_type": "escalation_review",
            "result": {
                "confidence": 0.91
            }
        }));
        assert_eq!(confidence, Some(0.91));
    }

    #[tokio::test]
    async fn build_harness_stream_callback_streams_routed_mail_assistant_text() {
        let (sender, mut receiver) = mpsc::channel(256);
        let callback = build_harness_stream_callback(
            sender,
            ExecutionPlan {
                visible_agent_name: "mail-assistant".to_string(),
                execution_agent_name: "email-analyzer".to_string(),
                routing_reason: "single email explanation".to_string(),
            },
        );

        callback(HarnessEvent::FinalOutput {
            output: json!({
                "human_summary": "This is a password reset email.",
                "spam_status": "not-spam",
                "phishing_status": "not-phishing",
                "marketing_status": "not-marketing",
                "otp_status": "otp",
                "otp_code": "123456",
                "threat_level": "none",
                "category": "work",
                "email_type": "actionable"
            }),
        });

        let mut streamed = String::new();
        while let Ok(event) = receiver.try_recv() {
            if let ChatStreamEvent::AssistantDelta { delta } = event {
                streamed.push_str(&delta);
            }
        }

        assert!(streamed.contains("## Mail Assistant"));
        assert!(streamed.contains("password reset"));
        assert!(streamed.contains("123456"));
    }

    #[test]
    fn resolve_thread_visible_agent_allows_mail_assistant_threads_without_specialist_override() {
        let thread = ThreadSummary {
            thread_id: "thread-1".to_string(),
            agent_name: "mail-assistant".to_string(),
            title: None,
            context_email_id: None,
            message_count: 1,
            last_message_at: None,
            created_at: Utc::now(),
        };

        let visible_agent = resolve_thread_visible_agent(&thread, Some("mail-assistant"))
            .expect("mail assistant thread should continue");
        assert_eq!(visible_agent, "mail-assistant");

        let error = resolve_thread_visible_agent(&thread, Some("email-analyzer"))
            .expect_err("mail assistant thread should reject direct specialist override");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_thread_visible_agent_resumes_legacy_specialist_thread_through_mail_assistant() {
        std::env::remove_var("MAIL_ASSISTANT_DIRECT_SUBAGENTS");
        let thread = ThreadSummary {
            thread_id: "thread-legacy".to_string(),
            agent_name: "digest-agent".to_string(),
            title: None,
            context_email_id: None,
            message_count: 1,
            last_message_at: None,
            created_at: Utc::now(),
        };

        let visible_agent = resolve_thread_visible_agent(&thread, None)
            .expect("legacy specialist thread should be resumable");
        assert_eq!(visible_agent, "mail-assistant");

        let visible_agent = resolve_thread_visible_agent(&thread, Some("mail-assistant"))
            .expect("explicit Mail Assistant resume should be accepted");
        assert_eq!(visible_agent, "mail-assistant");
    }

    #[test]
    fn chat_request_validate_rejects_too_large_message() {
        let request = ChatRequest {
            thread_id: None,
            agent_name: Some("email-analyzer".to_string()),
            message: "x".repeat(MAX_CHAT_MESSAGE_CHARS + 1),
            context_email_id: None,
        };

        let error = request
            .validate()
            .expect_err("oversized message should fail validation");
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn parse_stream_request_message_rejects_oversized_text_frame() {
        let oversized = "x".repeat(MAX_CHAT_STREAM_REQUEST_BYTES + 1);
        let error = parse_stream_request_message(WsMessage::Text(oversized.into()))
            .expect_err("oversized websocket frame should be rejected");
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn parse_stream_request_message_validates_payload_shape() {
        let payload = json!({
            "message": ""
        });

        let error = parse_stream_request_message(WsMessage::Text(payload.to_string().into()))
            .expect_err("invalid chat request should fail validation");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn chat_request_validate_allows_missing_agent_name() {
        let request = ChatRequest {
            thread_id: None,
            agent_name: None,
            message: "hello".to_string(),
            context_email_id: None,
        };

        request
            .validate()
            .expect("missing agent name should default");
    }
}
