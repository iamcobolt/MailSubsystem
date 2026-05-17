use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::api_error::{ApiError, ApiResult};
use super::chat::{ChatRequest, ChatStreamEvent};

pub(super) const MAX_CHAT_STREAM_REQUEST_BYTES: usize = 32 * 1024;
pub(super) const MAX_CHAT_STREAM_EVENTS_BUFFER: usize = 1_024;
pub(super) const INITIAL_CHAT_STREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) type ChatEventSender = mpsc::Sender<ChatStreamEvent>;

pub(super) async fn receive_stream_request(socket: &mut WebSocket) -> ApiResult<ChatRequest> {
    let Some(message) = tokio::time::timeout(INITIAL_CHAT_STREAM_REQUEST_TIMEOUT, socket.next())
        .await
        .map_err(|_| ApiError::request_timeout("timed out waiting for the initial chat request"))?
    else {
        return Err(ApiError::bad_request(
            "expected an initial websocket message containing ChatRequest JSON",
        ));
    };

    let message = message.map_err(ApiError::internal)?;
    parse_stream_request_message(message)
}

pub(super) fn parse_stream_request_message(message: WsMessage) -> ApiResult<ChatRequest> {
    let request = match message {
        WsMessage::Text(text) => {
            if text.len() > MAX_CHAT_STREAM_REQUEST_BYTES {
                return Err(ApiError::payload_too_large(format!(
                    "chat request exceeds the {} byte websocket limit",
                    MAX_CHAT_STREAM_REQUEST_BYTES
                )));
            }
            serde_json::from_str::<ChatRequest>(&text).map_err(|error| {
                ApiError::bad_request(format!("invalid chat request: {}", error))
            })?
        }
        WsMessage::Binary(bytes) => {
            if bytes.len() > MAX_CHAT_STREAM_REQUEST_BYTES {
                return Err(ApiError::payload_too_large(format!(
                    "chat request exceeds the {} byte websocket limit",
                    MAX_CHAT_STREAM_REQUEST_BYTES
                )));
            }
            serde_json::from_slice::<ChatRequest>(&bytes).map_err(|error| {
                ApiError::bad_request(format!("invalid chat request: {}", error))
            })?
        }
        WsMessage::Close(_) => {
            return Err(ApiError::bad_request(
                "websocket closed before a chat request was sent",
            ));
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unsupported websocket message type: {:?}",
                other
            )));
        }
    };

    request.validate()?;
    Ok(request)
}

pub(super) async fn send_ws_event(socket: &mut WebSocket, event: &ChatStreamEvent) -> Result<()> {
    let payload = serde_json::to_string(event).context("serialize websocket stream event")?;
    socket
        .send(WsMessage::Text(payload.into()))
        .await
        .context("send websocket stream event")
}

pub(super) async fn emit_stream_event(stream: Option<&ChatEventSender>, event: ChatStreamEvent) {
    if let Some(stream) = stream {
        let _ = stream.send(event).await;
    }
}
