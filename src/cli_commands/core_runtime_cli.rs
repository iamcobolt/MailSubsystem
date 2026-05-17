use anyhow::Context;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::{core_runtime, db};

pub async fn run_core() -> anyhow::Result<()> {
    core_runtime::run_core().await
}

pub async fn run_app(bind: Option<String>) -> anyhow::Result<()> {
    core_runtime::run_app(bind).await
}

pub async fn run_core_status() -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;
    let status = database
        .core_work_status_for_account(DEFAULT_ACCOUNT_ID)
        .await
        .context("Failed to load core work status")?;

    println!("--- core status ---");
    println!("account: {}", status.account_id);
    println!("state: {}", status.state);
    println!(
        "queue: pending={} failed={} processing={} dead={}",
        status.queue_depth.pending,
        status.queue_depth.failed,
        status.queue_depth.processing,
        status.queue_depth.dead
    );
    println!(
        "pipeline: last_sync={} last_analysis={} last_locate={}",
        fmt_ts(status.pipeline.last_sync),
        fmt_ts(status.pipeline.last_analysis),
        fmt_ts(status.pipeline.last_locate)
    );
    if let Some(error) = &status.last_error {
        println!("last_error: {}", error);
    }

    print_work_table("active work", &status.active_work);
    print_work_table("recent failures", &status.recent_failures);
    print_work_table("recent completed", &status.recent_completed);

    Ok(())
}

fn fmt_ts(ts: Option<chrono::DateTime<chrono::Utc>>) -> String {
    ts.map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "-".to_string())
}

fn print_work_table(title: &str, items: &[db::CoreWorkStatusItem]) {
    println!("\n--- {} ---", title);
    if items.is_empty() {
        println!("(none)");
        return;
    }
    println!(
        "{:>6} {:<16} {:<10} {:<16} {:<24} {:<24} error",
        "id", "type", "status", "source", "reason", "updated_at"
    );
    println!("{}", "-".repeat(120));
    for item in items {
        println!(
            "{:>6} {:<16} {:<10} {:<16} {:<24} {:<24} {}",
            item.id,
            item.work_type,
            item.status,
            item.source.as_deref().unwrap_or("-"),
            item.reason.as_deref().unwrap_or("-"),
            item.updated_at.to_rfc3339(),
            item.last_error.as_deref().unwrap_or("-")
        );
    }
}
