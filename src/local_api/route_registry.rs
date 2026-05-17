use std::sync::Arc;

use axum::{
    routing::{delete, get, post},
    Json, Router,
};
use serde::Serialize;

use super::{chat, data, state::ApiState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

pub fn api_routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/agents", get(data::list_agents))
        .route("/emails", get(data::list_emails))
        .route("/emails/{message_id}", get(data::get_email))
        .route("/folders", get(data::list_folders))
        .route("/runs", get(data::list_runs))
        .route("/runs/{run_id}", get(data::get_run))
        .route("/stats", get(data::get_stats))
        .route("/chat", post(chat::post_chat))
        .route("/chat/stream", get(chat::ws_chat_stream))
        .route("/threads", get(chat::list_threads))
        .route(
            "/threads/{thread_id}/messages",
            get(chat::list_thread_messages),
        )
        .route("/threads/{thread_id}", delete(chat::delete_thread))
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn status(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
) -> Result<Json<crate::db::CoreWorkStatusSummary>, (axum::http::StatusCode, String)> {
    state
        .db
        .core_work_status_for_account(&state.account_id)
        .await
        .map(Json)
        .map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load core status: {}", error),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_returns_ok() {
        let Json(response) = health().await;
        assert_eq!(
            response,
            HealthResponse {
                status: "ok",
                version: env!("CARGO_PKG_VERSION"),
            }
        );
    }
}
