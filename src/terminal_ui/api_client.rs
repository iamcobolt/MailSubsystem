use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use url::Url;

use crate::agent_catalog::AgentCatalogEntry;

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:3100";
pub const DEFAULT_EMAIL_LIMIT: usize = 100;
pub const DEFAULT_RUN_LIMIT: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub thread_id: Option<String>,
    pub agent_name: String,
    pub message: String,
    pub context_email_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EmailListQuery {
    pub search: Option<String>,
    pub folder: Option<String>,
    pub category: Option<String>,
    pub spam_status: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub agent_name: String,
    pub title: Option<String>,
    pub context_email_id: Option<String>,
    pub message_count: i64,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub agent_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailRecord {
    pub message_id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_date: Option<DateTime<Utc>>,
    pub spam_status: String,
    pub human_summary: Option<String>,
    pub category: Option<String>,
    pub topic: Option<String>,
    pub location: Option<String>,
    pub raw_email_content: Option<String>,
    pub body_text: Option<String>,
    pub action_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailListResponse {
    pub emails: Vec<EmailRecord>,
    pub total_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FolderNode {
    pub name: String,
    pub message_count: i32,
    pub children: Vec<FolderNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestWindowStats {
    pub total_received: i64,
    pub marketing_count: i64,
    pub otp_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardStats {
    pub since: DateTime<Utc>,
    pub window_days: i64,
    pub total_emails: i64,
    pub analyzed_count: i64,
    pub filed_count: i64,
    pub inbox_remaining: i64,
    pub spam_count: i64,
    pub phishing_count: i64,
    pub folder_count: i64,
    pub window: DigestWindowStats,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRunSummary {
    pub agent_name: String,
    pub task_id: String,
    pub status: String,
    pub steps: i32,
    pub llm_calls: i32,
    pub tool_calls: i32,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListRunsResponse {
    pub runs: Vec<AgentRunSummary>,
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub stats: DashboardStats,
    pub runs: Vec<AgentRunSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatStreamEvent {
    Ready,
    ThreadReady {
        thread_id: String,
    },
    RunStarted {
        run_id: String,
        agent_name: String,
        visible_agent_name: Option<String>,
    },
    StepStarted {
        step: usize,
    },
    ToolCall {
        step: usize,
        tool_name: String,
    },
    ToolResult {
        tool_name: String,
        latency_ms: u64,
    },
    AssistantDelta {
        delta: String,
    },
    AssistantCompleted {
        thread_id: String,
    },
    Error {
        message: String,
    },
    Done,
}

#[derive(Debug, Clone)]
pub enum NetworkCommand {
    LoadAgents,
    LoadThreads,
    LoadMessages { thread_id: String },
    DeleteThread { thread_id: String },
    SendChat { request: ChatRequest },
    LoadEmails { query: EmailListQuery },
    LoadEmailDetail { message_id: String },
    LoadFolders,
    LoadStatus,
}

#[derive(Debug, Clone)]
pub enum NetworkResult {
    AgentsLoaded(Result<Vec<AgentCatalogEntry>, String>),
    ThreadsLoaded(Result<Vec<ThreadSummary>, String>),
    MessagesLoaded {
        thread_id: String,
        result: Result<Vec<ConversationMessage>, String>,
    },
    ThreadDeleted {
        thread_id: String,
        result: Result<(), String>,
    },
    ChatEvent(ChatStreamEvent),
    EmailsLoaded(Result<EmailListResponse, String>),
    EmailDetailLoaded {
        message_id: String,
        result: Box<Result<EmailRecord, String>>,
    },
    FoldersLoaded(Result<Vec<FolderNode>, String>),
    StatusLoaded(Result<StatusSnapshot, String>),
}

#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    base_url: String,
    ws_url: String,
    auth_token: Option<String>,
}

impl ApiClient {
    pub fn new(base_url: Option<String>) -> Result<Self> {
        let base_url = normalize_base_url(base_url.as_deref().unwrap_or(DEFAULT_API_URL))?;
        let ws_url = websocket_base(&base_url)?;
        Ok(Self {
            http: Client::new(),
            base_url,
            ws_url,
            auth_token: crate::config::api_auth_token(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        self.request_json("/api/threads").await
    }

    async fn list_agents(&self) -> Result<Vec<AgentCatalogEntry>> {
        self.request_json("/api/agents").await
    }

    async fn list_thread_messages(&self, thread_id: &str) -> Result<Vec<ConversationMessage>> {
        self.request_json(&format!(
            "/api/threads/{}/messages",
            encode_path_segment(thread_id)
        ))
        .await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        let response = self
            .with_auth(self.http.delete(
                self.resolve_http_url(&format!("/api/threads/{}", encode_path_segment(thread_id)))?,
            ))
            .send()
            .await
            .with_context(|| format!("delete thread '{}'", thread_id))?;

        if response.status().is_success() {
            return Ok(());
        }

        anyhow::bail!(read_error(response).await);
    }

    async fn list_emails(&self, query: &EmailListQuery) -> Result<EmailListResponse> {
        let mut url = self.resolve_http_url("/api/emails")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("limit", &query.limit.to_string());
            pairs.append_pair("offset", &query.offset.to_string());
            if let Some(search) = query
                .search
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                pairs.append_pair("search", search);
            }
            if let Some(folder) = query
                .folder
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                pairs.append_pair("folder", folder);
            }
            if let Some(category) = query
                .category
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                pairs.append_pair("category", category);
            }
            if let Some(spam_status) = query
                .spam_status
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                pairs.append_pair("spam_status", spam_status);
            }
        }

        self.request_json_from_url(url, "list emails").await
    }

    async fn get_email(&self, message_id: &str) -> Result<EmailRecord> {
        self.request_json(&format!("/api/emails/{}", encode_path_segment(message_id)))
            .await
    }

    async fn list_folders(&self) -> Result<Vec<FolderNode>> {
        self.request_json("/api/folders").await
    }

    async fn get_status_snapshot(&self) -> Result<StatusSnapshot> {
        let stats = self.request_json::<DashboardStats>("/api/stats").await?;
        let runs = self
            .request_json::<ListRunsResponse>(&format!("/api/runs?limit={}", DEFAULT_RUN_LIMIT))
            .await?;
        Ok(StatusSnapshot {
            stats,
            runs: runs.runs,
        })
    }

    async fn request_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.request_json_from_url(self.resolve_http_url(path)?, path)
            .await
    }

    async fn request_json_from_url<T>(&self, url: Url, context: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .with_auth(self.http.get(url))
            .send()
            .await
            .with_context(|| format!("GET {}", context))?;

        if !response.status().is_success() {
            anyhow::bail!(read_error(response).await);
        }

        response
            .json::<T>()
            .await
            .with_context(|| format!("decode JSON for {}", context))
    }

    fn resolve_http_url(&self, path: &str) -> Result<Url> {
        Url::parse(&format!("{}{}", self.base_url, path)).context("build HTTP URL")
    }

    fn resolve_ws_url(&self, path: &str) -> Result<Url> {
        Url::parse(&format!("{}{}", self.ws_url, path)).context("build websocket URL")
    }

    fn with_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = self.auth_token.as_deref() {
            request.bearer_auth(token)
        } else {
            request
        }
    }
}

pub fn dispatch_command(
    client: Arc<ApiClient>,
    sender: UnboundedSender<NetworkResult>,
    command: NetworkCommand,
) {
    match command {
        NetworkCommand::LoadAgents => {
            tokio::spawn(async move {
                let result = client.list_agents().await.map_err(error_text);
                let _ = sender.send(NetworkResult::AgentsLoaded(result));
            });
        }
        NetworkCommand::LoadThreads => {
            tokio::spawn(async move {
                let result = client.list_threads().await.map_err(error_text);
                let _ = sender.send(NetworkResult::ThreadsLoaded(result));
            });
        }
        NetworkCommand::LoadMessages { thread_id } => {
            tokio::spawn(async move {
                let result = client
                    .list_thread_messages(&thread_id)
                    .await
                    .map_err(error_text);
                let _ = sender.send(NetworkResult::MessagesLoaded { thread_id, result });
            });
        }
        NetworkCommand::DeleteThread { thread_id } => {
            tokio::spawn(async move {
                let result = client.delete_thread(&thread_id).await.map_err(error_text);
                let _ = sender.send(NetworkResult::ThreadDeleted { thread_id, result });
            });
        }
        NetworkCommand::SendChat { request } => {
            tokio::spawn(async move {
                if let Err(error) = stream_chat(client, request, sender.clone()).await {
                    let _ = sender.send(NetworkResult::ChatEvent(ChatStreamEvent::Error {
                        message: error_text(error),
                    }));
                }
            });
        }
        NetworkCommand::LoadEmails { query } => {
            tokio::spawn(async move {
                let result = client.list_emails(&query).await.map_err(error_text);
                let _ = sender.send(NetworkResult::EmailsLoaded(result));
            });
        }
        NetworkCommand::LoadEmailDetail { message_id } => {
            tokio::spawn(async move {
                let result = Box::new(client.get_email(&message_id).await.map_err(error_text));
                let _ = sender.send(NetworkResult::EmailDetailLoaded { message_id, result });
            });
        }
        NetworkCommand::LoadFolders => {
            tokio::spawn(async move {
                let result = client.list_folders().await.map_err(error_text);
                let _ = sender.send(NetworkResult::FoldersLoaded(result));
            });
        }
        NetworkCommand::LoadStatus => {
            tokio::spawn(async move {
                let result = client.get_status_snapshot().await.map_err(error_text);
                let _ = sender.send(NetworkResult::StatusLoaded(result));
            });
        }
    }
}

async fn stream_chat(
    client: Arc<ApiClient>,
    chat_request: ChatRequest,
    sender: UnboundedSender<NetworkResult>,
) -> Result<()> {
    let mut ws_request = client
        .resolve_ws_url("/api/chat/stream")?
        .to_string()
        .into_client_request()
        .context("build chat websocket request")?;
    if let Some(token) = client.auth_token.as_deref() {
        ws_request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", token)
                .parse()
                .context("build websocket authorization header")?,
        );
    }

    let (mut socket, _) = connect_async(ws_request)
        .await
        .context("connect chat websocket")?;

    let ready = read_stream_event(&mut socket).await?;
    match ready {
        ChatStreamEvent::Ready => {}
        other => anyhow::bail!("unexpected initial stream event: {:?}", other),
    }

    socket
        .send(Message::Text(
            serde_json::to_string(&chat_request)
                .context("serialize chat request")?
                .into(),
        ))
        .await
        .context("send chat request over websocket")?;

    loop {
        let event = read_stream_event(&mut socket).await?;
        let done = matches!(event, ChatStreamEvent::Done | ChatStreamEvent::Error { .. });
        let _ = sender.send(NetworkResult::ChatEvent(event));
        if done {
            break;
        }
    }

    Ok(())
}

async fn read_stream_event(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<ChatStreamEvent> {
    while let Some(message) = socket.next().await {
        match message.context("read websocket frame")? {
            Message::Text(text) => {
                return serde_json::from_str(&text).context("decode chat stream event");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(&bytes).context("decode binary chat stream event");
            }
            Message::Close(_) => anyhow::bail!("chat websocket closed before completion"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    anyhow::bail!("chat websocket ended before completion");
}

async fn read_error(response: reqwest::Response) -> String {
    let status = response.status();
    match response.text().await {
        Ok(body) if !body.trim().is_empty() => format!("{} {}", status, body.trim()),
        Ok(_) | Err(_) => status.to_string(),
    }
}

fn normalize_base_url(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let parsed = Url::parse(trimmed).with_context(|| format!("parse API URL '{}'", value))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!(
            "unsupported API URL scheme '{}'; expected http or https",
            parsed.scheme()
        );
    }
    Ok(trimmed.to_string())
}

fn websocket_base(http_base: &str) -> Result<String> {
    let mut url = Url::parse(http_base).context("parse API URL for websocket conversion")?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| anyhow::anyhow!("convert API URL scheme to websocket"))?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn encode_path_segment(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn error_text(error: impl std::fmt::Display) -> String {
    let message = format!("{error:#}");
    if looks_like_api_connection_refused(&message) {
        return format!(
            "{}\n\nThe TUI needs the MailSubsystem API. Start the server app in another terminal with `make app`, or use `make api` when you explicitly want API-only mode, then rerun `make tui`.",
            message
        );
    }
    message
}

fn looks_like_api_connection_refused(message: &str) -> bool {
    message.contains("Connection refused")
        || message.contains("connection refused")
        || message.contains("tcp connect error")
        || message.contains("os error 111")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_keeps_expected_origin() {
        let base = normalize_base_url("http://127.0.0.1:3100/").expect("normalize base");
        assert_eq!(base, "http://127.0.0.1:3100");
        assert_eq!(
            websocket_base(&base).expect("ws base"),
            "ws://127.0.0.1:3100"
        );
    }

    #[test]
    fn parse_chat_stream_event_round_trip() {
        let raw =
            r#"{"type":"tool_call","step":2,"tool_name":"search","arguments":{"q":"invoice"}}"#;
        let parsed: ChatStreamEvent = serde_json::from_str(raw).expect("parse tool call event");

        match parsed {
            ChatStreamEvent::ToolCall { step, tool_name } => {
                assert_eq!(step, 2);
                assert_eq!(tool_name, "search");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn error_text_suggests_starting_local_stack_for_connection_refused() {
        let message =
            error_text("GET /api/threads: tcp connect error: Connection refused (os error 111)");

        assert!(message.contains("make app"));
        assert!(message.contains("make api"));
        assert!(message.contains("API-only"));
    }
}
