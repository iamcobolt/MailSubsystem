#![deny(clippy::all)]

use std::time::Instant;

mod agent_catalog;
mod agent_router;
#[path = "agent_execution_runtime.rs"]
mod agent_runtime;
#[path = "ai_provider.rs"]
mod ai;
#[path = "email_analysis/mod.rs"]
mod ai_analysis;
#[path = "local_api/mod.rs"]
mod api;
#[path = "attachment_analysis.rs"]
mod attachments;
#[path = "cli_parser.rs"]
mod cli;
#[path = "cli_commands/mod.rs"]
mod commands;
#[path = "runtime_config.rs"]
mod config;
#[path = "core_daemon.rs"]
mod core_runtime;
#[path = "core_work_queue.rs"]
mod core_work;
mod database;
pub(crate) use database as db;
#[path = "embedding_service.rs"]
mod embeddings;
#[path = "agent_harness/mod.rs"]
pub mod harness;
#[path = "imap_client.rs"]
mod imap;
#[path = "folder_location_agent.rs"]
mod location_analysis;
#[path = "metrics_sink.rs"]
mod metrics;
mod model_ref;
#[path = "run_observability.rs"]
mod observability;
#[path = "mailbox_retrieval.rs"]
mod rag;
#[path = "provider_rate_limit.rs"]
mod rate_limit;
#[path = "runtime_task_services.rs"]
mod runtime_services;
pub mod spend_safety;
#[path = "worker_runtime.rs"]
mod subagent_runtime;
#[path = "mailbox_sync_runtime.rs"]
mod sync_runtime;
#[path = "body_sync_service.rs"]
mod sync_service;
#[path = "terminal_ui/mod.rs"]
mod tui;

pub async fn run() -> anyhow::Result<()> {
    let _ = dotenvy::from_path(commands::shared::DEFAULT_ENV_PATH);

    let args: Vec<String> = std::env::args().collect();
    let command = cli::parse_command(&args);

    let run_context = observability::init_observability(command.name());
    let started = Instant::now();
    let result = commands::dispatch(command).await;
    observability::finish_run(&run_context, &result, started.elapsed());
    result
}
