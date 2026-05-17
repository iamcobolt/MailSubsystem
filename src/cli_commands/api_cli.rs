use crate::config::{api_bind_addr, DEFAULT_ACCOUNT_ID};
use crate::runtime_services;

pub async fn run_api(bind: Option<String>) -> anyhow::Result<()> {
    let bind_addr = bind.unwrap_or_else(api_bind_addr);
    let state = runtime_services::api_state_from_database(DEFAULT_ACCOUNT_ID, "api").await?;
    let api_server = runtime_services::bind_api_server(&bind_addr, state, "API server").await?;

    println!("Starting API server on http://{}", bind_addr);
    api_server.serve().await
}
