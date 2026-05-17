use anyhow::{Context, Result};

use crate::config::{AccountConfig, DEFAULT_ACCOUNT_ID};
use crate::db;
use crate::imap;
use crate::metrics;

use super::shared::DEFAULT_ENV_PATH;

/// Default: OTPs expire after 1 hour (3600 seconds).
const DEFAULT_OTP_MAX_AGE_SECS: i64 = 3600;
/// Default: newsletters expire after 30 days.
const DEFAULT_NEWSLETTER_MAX_AGE_DAYS: i32 = 30;
/// Default: process up to 50 candidates per cycle.
const DEFAULT_LIFECYCLE_LIMIT: usize = 50;

pub struct LifecycleConfig {
    pub otp_max_age_secs: i64,
    pub newsletter_max_age_days: i32,
    pub limit: usize,
    pub trash_folder: String,
}

impl LifecycleConfig {
    pub fn from_env() -> Self {
        Self {
            otp_max_age_secs: std::env::var("LIFECYCLE_OTP_MAX_AGE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_OTP_MAX_AGE_SECS),
            newsletter_max_age_days: std::env::var("LIFECYCLE_NEWSLETTER_MAX_AGE_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_NEWSLETTER_MAX_AGE_DAYS),
            limit: std::env::var("LIFECYCLE_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_LIFECYCLE_LIMIT),
            trash_folder: std::env::var("TRASH_FOLDER").unwrap_or_else(|_| "Trash".to_string()),
        }
    }
}

/// CLI entry point: `mailsubsystem lifecycle-cleanup [--dry-run]`
pub async fn run_lifecycle_cleanup(dry_run: bool) -> Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let account = AccountConfig::load(DEFAULT_ACCOUNT_ID)?;
    let config = LifecycleConfig::from_env();
    let (trashed, total) = run_lifecycle_cleanup_for_account(dry_run, &account.id, &config).await?;
    if dry_run {
        println!(
            "Dry run: would trash {} of {} lifecycle candidates",
            trashed, total
        );
    } else {
        println!(
            "Lifecycle cleanup: trashed {} of {} candidates",
            trashed, total
        );
    }
    Ok(())
}

/// Daemon-callable entry point.
pub async fn run_lifecycle_cleanup_for_account(
    dry_run: bool,
    account_id: &str,
    config: &LifecycleConfig,
) -> Result<(usize, usize)> {
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let db = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let candidates = db
        .get_lifecycle_cleanup_candidates_for_account(
            account_id,
            config.otp_max_age_secs,
            config.newsletter_max_age_days,
            config.limit,
        )
        .await
        .context("fetch lifecycle candidates")?;

    let total = candidates.len();
    if total == 0 {
        return Ok((0, 0));
    }

    log::info!(
        "[lifecycle] found {} candidates for account {}",
        total,
        account_id
    );

    if dry_run {
        for email in &candidates {
            let reason = if email.otp_status.as_deref() == Some("otp") {
                "expired OTP"
            } else {
                "stale newsletter"
            };
            println!(
                "  [dry-run] would trash {}: {} ({})",
                email.message_id,
                email.subject.as_deref().unwrap_or("<no subject>"),
                reason
            );
        }
        return Ok((total, total));
    }

    let (server, username, password) = imap::load_imap_from_env().context("Load IMAP config")?;
    let imap_client = imap::ImapClient::new(server, username, password);
    let mut trashed = 0usize;

    for email in &candidates {
        let source = match email.location.as_deref().unwrap_or("") {
            "" => {
                log::warn!("[lifecycle] skip {}: no location set", email.message_id);
                continue;
            }
            loc => loc,
        };

        // Already in Trash — just mark it
        if source.eq_ignore_ascii_case(&config.trash_folder) {
            db.mark_email_action_applied_for_account(
                account_id,
                &email.message_id,
                "lifecycle_trashed",
            )
            .await?;
            trashed += 1;
            continue;
        }

        let uid = match email.uid {
            Some(u) => u as u32,
            None => {
                log::warn!("[lifecycle] skip {}: no UID", email.message_id);
                continue;
            }
        };

        // Move to Trash
        if let Err(e) = imap_client.ensure_mailbox(&config.trash_folder).await {
            log::warn!("[lifecycle] ensure Trash failed: {}", e);
            continue;
        }

        match imap_client
            .move_message(source, uid, &config.trash_folder)
            .await
        {
            Ok(()) => {
                db.mark_email_action_applied_for_account(
                    account_id,
                    &email.message_id,
                    "lifecycle_trashed",
                )
                .await?;
                let reason = if email.otp_status.as_deref() == Some("otp") {
                    "expired OTP"
                } else {
                    "stale newsletter"
                };
                log::info!(
                    "[lifecycle] trashed {}: {} ({})",
                    email.message_id,
                    email.subject.as_deref().unwrap_or("<no subject>"),
                    reason
                );
                metrics::counter("lifecycle_trashed_total", 1, &[("reason", reason)]);
                trashed += 1;
            }
            Err(e) => {
                log::warn!(
                    "[lifecycle] failed to move {} to Trash: {}",
                    email.message_id,
                    e
                );
            }
        }
    }

    Ok((trashed, total))
}
