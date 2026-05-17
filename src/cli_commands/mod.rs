#[path = "agent_cli.rs"]
pub mod agent_commands;
#[path = "analysis_cli.rs"]
pub mod analysis_commands;
#[path = "api_cli.rs"]
pub mod api_commands;
#[path = "folder_consolidation_cli.rs"]
pub mod consolidation_commands;
#[path = "core_runtime_cli.rs"]
pub mod core_commands;
#[path = "digest_cli.rs"]
pub mod digest_commands;
#[path = "scratchpad_hygiene_cli.rs"]
pub mod hygiene_commands;
#[path = "message_lifecycle_cli.rs"]
pub mod lifecycle_commands;
#[path = "folder_location_cli.rs"]
pub mod location_commands;
#[path = "maintenance_cli.rs"]
pub mod maintenance_commands;
#[path = "command_support.rs"]
pub mod shared;
#[path = "sync_cli.rs"]
pub mod sync_commands;
#[path = "terminal_ui_cli.rs"]
pub mod tui_commands;

use crate::cli::Command;

pub async fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Check => sync_commands::run_check().await,
        Command::Sync => sync_commands::run_sync().await,
        Command::SyncSlow => sync_commands::run_sync_slow().await,
        Command::Status => sync_commands::run_status().await,
        Command::SyncIncremental => sync_commands::run_sync_incremental().await,
        Command::SyncWindow { days, full } => sync_commands::run_sync_window(days, full).await,
        Command::Core => core_commands::run_core().await,
        Command::CoreStatus => core_commands::run_core_status().await,
        Command::App { bind } => core_commands::run_app(bind).await,
        Command::Api { bind } => api_commands::run_api(bind).await,
        Command::Tui { api_url } => tui_commands::run_tui(api_url).await,
        Command::Analyze { message_id, force } => {
            analysis_commands::run_analyze(message_id, force).await
        }
        Command::TestLlm { local, frontier } => {
            analysis_commands::run_test_llm(local, frontier).await
        }
        Command::Show { message_id } => analysis_commands::run_show(message_id).await,
        Command::ResolveOrphans => location_commands::run_resolve_orphans().await,
        Command::ProcessFrontierQueue { limit } => {
            analysis_commands::run_process_frontier_queue(limit).await
        }
        Command::Locate {
            message_id,
            force,
            limit,
        } => location_commands::run_locate_with_limit(message_id, force, limit).await,
        Command::AgentRun {
            agent_spec,
            task_id,
            input,
        } => agent_commands::run_agent(agent_spec, task_id, input).await,
        Command::AgentRuns {
            limit,
            status,
            agent,
        } => agent_commands::run_agent_runs(limit, status, agent).await,
        Command::AgentShow { run_id } => agent_commands::run_agent_show(run_id).await,
        Command::AgentBenchmark { limit } => agent_commands::run_agent_benchmark(limit).await,
        Command::AgentScratchpad { agent, key } => {
            agent_commands::run_agent_scratchpad(agent, key).await
        }
        Command::AgentScratchpadDelete { agent, key } => {
            agent_commands::run_agent_scratchpad_delete(agent, key).await
        }
        Command::AgentStats => agent_commands::run_agent_stats().await,
        Command::AgentCalibrate { agent } => agent_commands::run_agent_calibrate(agent).await,
        Command::ScratchpadHygiene { account } => {
            hygiene_commands::run_scratchpad_hygiene(account.as_deref()).await
        }
        Command::File { dry_run } => location_commands::run_file(dry_run).await,
        Command::LifecycleCleanup { dry_run } => {
            lifecycle_commands::run_lifecycle_cleanup(dry_run).await
        }
        Command::Consolidate { dry_run, account } => {
            consolidation_commands::run_consolidate(dry_run, account.as_deref()).await
        }
        Command::Digest {
            window,
            account,
            json,
        } => digest_commands::run_digest(&window, account.as_deref(), json).await,
        Command::BackfillMessageTokens => maintenance_commands::run_backfill_message_tokens().await,
        Command::BackfillMessage { message_id } => {
            maintenance_commands::run_backfill_message(message_id).await
        }
        Command::BackfillFromImap { limit } => {
            maintenance_commands::run_backfill_from_imap(limit).await
        }
        Command::MigrateSchema { apply } => maintenance_commands::run_migrate_schema(apply).await,
        Command::EmbedBackfill { limit } => analysis_commands::run_embed_backfill(limit).await,
        Command::EmbedRebuild { limit } => analysis_commands::run_embed_rebuild(limit).await,
        Command::Help => {
            crate::cli::print_usage();
            Ok(())
        }
    }
}
