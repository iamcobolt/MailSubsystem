use anyhow::{Context, Result};
use sqlx::Row;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{BodySyncQueueDepth, Database};

#[derive(Debug, Clone, Default)]
pub struct DbCompletenessSnapshot {
    pub folder_count: i64,
    pub largest_folder_message_count: i64,
    pub email_count: i64,
    pub missing_message_id: i64,
    pub body_missing: i64,
    pub analysis_missing: i64,
    pub location_missing: i64,
    pub filing_pending: i64,
    pub body_sync: BodySyncQueueDepth,
}

impl DbCompletenessSnapshot {
    pub fn needs_full_sync_backfill(&self) -> bool {
        self.email_count == 0
            || (self.largest_folder_message_count >= 100
                && self.email_count.saturating_mul(4) < self.largest_folder_message_count)
    }

    pub fn has_active_backlog(&self) -> bool {
        self.needs_full_sync_backfill()
            || self.folder_count == 0
            || self.body_missing > 0
            || self.analysis_missing > 0
            || self.location_missing > 0
            || self.body_sync.pending > 0
            || self.body_sync.failed > 0
            || self.body_sync.processing > 0
    }
}

impl Database {
    /// Snapshot of account-scoped ingest completeness used to gate harness runs.
    pub async fn db_completeness_snapshot(&self) -> Result<DbCompletenessSnapshot> {
        self.db_completeness_snapshot_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn db_completeness_snapshot_for_account(
        &self,
        account_id: &str,
    ) -> Result<DbCompletenessSnapshot> {
        let row = sqlx::query(
            r#"
            SELECT
              (SELECT COUNT(*) FROM imap_folders WHERE account_id = $1) AS folder_count,
              (SELECT COALESCE(MAX(message_count), 0)::bigint FROM imap_folders WHERE account_id = $1 AND is_noselect = FALSE) AS largest_folder_message_count,
              (SELECT COUNT(*) FROM emails WHERE account_id = $1) AS email_count,
              (SELECT COUNT(*) FROM emails_missing_message_id WHERE account_id = $1) AS missing_message_id,
              (SELECT COUNT(*) FROM emails WHERE account_id = $1 AND deleted_from_server_at IS NULL AND body_text IS NULL AND raw_email_content IS NULL) AS body_missing,
              (SELECT COUNT(*) FROM emails WHERE account_id = $1 AND analyzed_at IS NULL AND analysis_permanent_failure = FALSE AND deleted_from_server_at IS NULL AND (body_text IS NOT NULL OR raw_email_content IS NOT NULL)) AS analysis_missing,
              (SELECT COUNT(*) FROM emails WHERE account_id = $1 AND analyzed_at IS NOT NULL AND category IS NOT NULL AND location_recommendation IS NULL AND location IS NOT NULL AND COALESCE(spam_status, '') <> 'spam' AND COALESCE(phishing_status, '') <> 'phishing' AND COALESCE(threat_level, '') NOT IN ('high', 'critical')) AS location_missing,
              (SELECT COUNT(*) FROM emails WHERE account_id = $1 AND location IS NOT NULL AND deleted_from_server_at IS NULL AND location_recommendation IS NOT NULL AND COALESCE(action_status, '') NOT IN ('trashed', 'junked') AND COALESCE(spam_status, '') <> 'spam' AND COALESCE(phishing_status, '') <> 'phishing' AND COALESCE(threat_level, '') NOT IN ('high', 'critical') AND user_pinned_folder IS NULL AND (filing_lock_until IS NULL OR filing_lock_until <= NOW()) AND LOWER(TRIM(location_recommendation)) <> LOWER(TRIM(location))) AS filing_pending,
              (SELECT COUNT(*) FROM body_sync_queue WHERE account_id = $1 AND status = 'pending') AS body_pending,
              (SELECT COUNT(*) FROM body_sync_queue WHERE account_id = $1 AND status = 'failed') AS body_failed,
              (SELECT COUNT(*) FROM body_sync_queue WHERE account_id = $1 AND status = 'processing') AS body_processing,
              (SELECT COUNT(*) FROM body_sync_queue WHERE account_id = $1 AND status = 'dead') AS body_dead
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("db_completeness_snapshot")?;

        Ok(DbCompletenessSnapshot {
            folder_count: row.get::<i64, _>("folder_count"),
            largest_folder_message_count: row.get::<i64, _>("largest_folder_message_count"),
            email_count: row.get::<i64, _>("email_count"),
            missing_message_id: row.get::<i64, _>("missing_message_id"),
            body_missing: row.get::<i64, _>("body_missing"),
            analysis_missing: row.get::<i64, _>("analysis_missing"),
            location_missing: row.get::<i64, _>("location_missing"),
            filing_pending: row.get::<i64, _>("filing_pending"),
            body_sync: BodySyncQueueDepth {
                pending: row.get::<i64, _>("body_pending"),
                failed: row.get::<i64, _>("body_failed"),
                processing: row.get::<i64, _>("body_processing"),
                dead: row.get::<i64, _>("body_dead"),
            },
        })
    }
}
