//! Agentic core runtime shell.

use crate::config::{api_bind_addr, DEFAULT_ACCOUNT_ID};
use crate::core_work;
use crate::runtime_services;
use crate::runtime_services::{RuntimeTask, RuntimeTaskSet, ShutdownSignals};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;

fn shutdown_grace_secs_from_env() -> u64 {
    std::env::var("CORE_SHUTDOWN_GRACE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30)
        .max(1)
}

async fn stop_tasks_gracefully(
    tasks: &mut RuntimeTaskSet,
    shutdown_tx: &watch::Sender<bool>,
    account_id: &str,
    worker_id: &str,
) -> anyhow::Result<()> {
    let _ = shutdown_tx.send(true);
    let grace_secs = shutdown_grace_secs_from_env();
    match timeout(Duration::from_secs(grace_secs), tasks.wait_for_any()).await {
        Ok(Some(task_result)) => task_result.into_result()?,
        Ok(None) => {}
        Err(_) => {
            log::warn!(
                "[core] shutdown grace period expired after {}s; aborting runtime tasks",
                grace_secs
            );
            tasks.abort_all_and_wait().await;
            let released = core_work::cleanup_worker_claims_from_runtime(
                account_id,
                worker_id,
                "shutdown grace expired; worker task was aborted",
            )
            .await?;
            if released > 0 {
                log::warn!(
                    "[core] shutdown cleanup released {} work item(s) owned by {}",
                    released,
                    worker_id
                );
            }
        }
    }
    Ok(())
}

pub async fn run_core() -> anyhow::Result<()> {
    println!("Starting MailSubsystem core runtime");
    println!("  Coordinator: durable work queue active");
    println!("  API: disabled (run `mailsubsystem api` when needed)");
    println!("Press Ctrl+C or send SIGTERM to stop gracefully");
    log::info!("[core] coordinator-only runtime started");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let coordinator_config = core_work::CoreCoordinatorConfig::from_env();
    let account_id = coordinator_config.account_id.clone();
    let worker_id = coordinator_config.worker_id.clone();
    let mut tasks = RuntimeTaskSet::new();
    tasks.push(RuntimeTask::spawn(
        "core coordinator",
        core_work::run_core_coordinator_with_config(coordinator_config, shutdown_rx),
    ));
    let mut shutdown = ShutdownSignals::new()?;

    tokio::select! {
        task_result = tasks.wait_for_any() => {
            let _ = shutdown_tx.send(true);
            tasks.abort_all_and_wait().await;
            let released = core_work::cleanup_worker_claims_from_runtime(
                &account_id,
                &worker_id,
                "core coordinator stopped before runtime shutdown completed",
            )
            .await?;
            if released > 0 {
                log::warn!(
                    "[core] shutdown cleanup released {} work item(s) owned by {}",
                    released,
                    worker_id
                );
            }
            if let Some(task_result) = task_result {
                task_result.into_result()?;
            }
        }
        signal = shutdown.recv() => {
            let signal = signal?;
            log::info!("[core] shutdown requested signal={}", signal);
            stop_tasks_gracefully(&mut tasks, &shutdown_tx, &account_id, &worker_id).await?;
        }
    }

    println!("Core stopped");
    Ok(())
}

pub async fn run_app(bind: Option<String>) -> anyhow::Result<()> {
    let bind_addr = bind.unwrap_or_else(api_bind_addr);
    let database = runtime_services::load_database("app").await?;
    let api_state = runtime_services::api_state(database, DEFAULT_ACCOUNT_ID);
    let api_server =
        runtime_services::bind_api_server(&bind_addr, api_state, "app API server").await?;

    println!("Starting MailSubsystem app");
    println!("  API:  http://{}", api_server.bind_addr());
    println!("  Core: durable work queue active");
    println!("  TUI:  run `mailsubsystem tui` in another terminal");
    println!("Press Ctrl+C or send SIGTERM to stop gracefully");
    log::info!("[app] core + API runtime started bind={}", bind_addr);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let coordinator_config = core_work::CoreCoordinatorConfig::from_env();
    let account_id = coordinator_config.account_id.clone();
    let worker_id = coordinator_config.worker_id.clone();
    let mut tasks = RuntimeTaskSet::new();
    tasks.push(api_server.spawn("app API server"));
    tasks.push(RuntimeTask::spawn(
        "app core coordinator",
        core_work::run_core_coordinator_with_config(coordinator_config, shutdown_rx),
    ));
    let mut shutdown = ShutdownSignals::new()?;

    tokio::select! {
        task_result = tasks.wait_for_any() => {
            let _ = shutdown_tx.send(true);
            tasks.abort_all_and_wait().await;
            let released = core_work::cleanup_worker_claims_from_runtime(
                &account_id,
                &worker_id,
                "app runtime stopped before shutdown completed",
            )
            .await?;
            if released > 0 {
                log::warn!(
                    "[app] shutdown cleanup released {} work item(s) owned by {}",
                    released,
                    worker_id
                );
            }
            if let Some(task_result) = task_result {
                task_result.into_result()?;
            }
        }
        signal = shutdown.recv() => {
            let signal = signal?;
            log::info!("[app] shutdown requested signal={}", signal);
            stop_tasks_gracefully(&mut tasks, &shutdown_tx, &account_id, &worker_id).await?;
        }
    }

    println!("App stopped");
    Ok(())
}
