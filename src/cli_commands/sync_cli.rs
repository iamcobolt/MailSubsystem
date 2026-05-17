use anyhow::Context;

use crate::{db, imap, sync_runtime};

/// Verify database and IMAP server are reachable and authenticatable.
pub async fn run_check() -> anyhow::Result<()> {
    let mut db_ok = false;
    let mut imap_ok = false;

    match db::DatabaseConfig::load() {
        Ok(db_config) => match db::Database::new(&db_config.connection_string()).await {
            Ok(database) => match database.list_tables().await {
                Ok(tables) => {
                    let has_emails = tables.iter().any(|t| t.eq_ignore_ascii_case("emails"));
                    let has_folders = tables
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case("imap_folders"));
                    println!(
                        "✓ Database: connected (tables: {}, emails: {}, imap_folders: {})",
                        tables.len(),
                        has_emails,
                        has_folders
                    );
                    db_ok = true;
                }
                Err(e) => println!("✗ Database: connected but list_tables failed: {}", e),
            },
            Err(e) => println!("✗ Database: connection failed: {}", e),
        },
        Err(e) => println!("✗ Database: config load failed: {}", e),
    }

    match imap::load_imap_from_env() {
        Ok((server, username, password)) => {
            let client = imap::ImapClient::new(server.clone(), username, password);
            match client.connect().await {
                Ok(_) => {
                    println!("✓ IMAP: connected and authenticated ({})", server);
                    imap_ok = true;
                }
                Err(e) => println!("✗ IMAP: connection/auth failed: {}", e),
            }
        }
        Err(e) => println!("✗ IMAP: config load failed: {}", e),
    }

    if db_ok && imap_ok {
        println!("\nAll checks passed.");
        Ok(())
    } else {
        anyhow::bail!("One or more connectivity checks failed")
    }
}

pub async fn run_sync() -> anyhow::Result<()> {
    sync_runtime::run_sync().await
}

pub async fn run_sync_slow() -> anyhow::Result<()> {
    sync_runtime::run_sync_slow().await
}

pub async fn run_sync_incremental() -> anyhow::Result<()> {
    sync_runtime::run_sync_incremental_once().await
}

pub async fn run_sync_window(days: Option<i64>, full: bool) -> anyhow::Result<()> {
    sync_runtime::run_sync_window(days, full).await
}

/// Show imap_folders and emails table state.
pub async fn run_status() -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;

    let folders = database
        .list_imap_folders()
        .await
        .context("Failed to list imap folders")?;

    println!("--- imap_folders ---");
    println!(
        "{:<25} {:>12} {:>14} {:>10} {:>8}",
        "folder_name", "last_synced", "last_full", "msg_count", "priority"
    );
    println!("{}", "-".repeat(75));
    for f in &folders {
        let last = f
            .last_synced_uid
            .map(|u| u.to_string())
            .unwrap_or_else(|| "-".into());
        let last_full = f
            .last_full_sync_uid
            .map(|u| u.to_string())
            .unwrap_or_else(|| "-".into());
        let count = f
            .message_count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into());
        let pri = f
            .priority
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<25} {:>12} {:>14} {:>10} {:>8}",
            f.folder_name, last, last_full, count, pri
        );
    }

    let email_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM emails")
        .fetch_one(&database.pool)
        .await
        .context("Failed to count emails")?;
    println!("\n--- emails ---");
    println!("total rows: {}", email_count.0);
    Ok(())
}
