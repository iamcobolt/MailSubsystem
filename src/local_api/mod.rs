pub mod api_error;
#[path = "chat_routes.rs"]
pub mod chat;
mod chat_formatting;
mod chat_streaming;
#[path = "data_routes.rs"]
pub mod data;
#[path = "http_middleware.rs"]
pub mod middleware;
#[path = "route_registry.rs"]
pub mod routes;
#[path = "api_state.rs"]
pub mod state;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use tokio::net::TcpListener;

pub use state::ApiState;

pub fn build_router(state: Arc<ApiState>) -> anyhow::Result<Router> {
    let api_router = middleware::apply(routes::api_routes())?;
    Ok(Router::new().nest("/api", api_router).with_state(state))
}

pub async fn bind_listener(bind: &str) -> anyhow::Result<TcpListener> {
    crate::config::validate_api_bind_security(bind)?;
    TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind API listener at {}", bind))
}

pub async fn serve_with_listener(
    listener: TcpListener,
    state: Arc<ApiState>,
) -> anyhow::Result<()> {
    let app = build_router(state)?;
    axum::serve(listener, app).await.context("run API server")?;
    Ok(())
}
