use anyhow::{Context, Result};
use sqlx::{types::Json, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct ImapFolder {
    pub folder_name: String,
    pub delimiter: Option<String>,
    pub is_noselect: bool,
    pub attributes: Vec<String>,
    /// Last UID synced from the IMAP server for this folder (envelope sync).
    pub last_synced_uid: Option<i32>,
    /// Last UID fully fetched (body + raw content) for this folder.
    pub last_full_sync_uid: Option<i32>,
    /// Message count from IMAP EXISTS (SELECT response).
    pub message_count: Option<i32>,
    /// Computed priority: 10 = INBOX, 1 = archive/junk/trash, 2–9 = by message count.
    pub priority: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct FolderSyncResult {
    pub new_count: i32,
    pub deleted_count: i32,
    pub restored_count: i32,
    pub updated_count: i32,
}

pub struct SystemFilingMoveRecord<'a> {
    pub account_id: &'a str,
    pub message_id: &'a str,
    pub location: &'a str,
    pub uid: i32,
    pub uid_validity: i32,
    pub actor: &'a str,
    pub cooldown_hours: i64,
}

impl Database {
    /// Get last_synced_uid for a folder. None if never synced.
    pub async fn get_folder_last_synced_uid(&self, folder_name: &str) -> Result<Option<i32>> {
        self.get_folder_last_synced_uid_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn get_folder_last_synced_uid_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<Option<i32>> {
        let row = sqlx::query(
            "SELECT last_synced_uid FROM imap_folders WHERE account_id = $1 AND folder_name = $2",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get last_synced_uid")?;
        Ok(row.and_then(|r| r.get("last_synced_uid")))
    }

    /// Update last_synced_uid for a folder after syncing a message.
    pub async fn update_folder_last_synced_uid(&self, folder_name: &str, uid: i32) -> Result<()> {
        self.update_folder_last_synced_uid_for_account(DEFAULT_ACCOUNT_ID, folder_name, uid)
            .await
    }

    pub async fn update_folder_last_synced_uid_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uid: i32,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE imap_folders SET last_synced_uid = $1, updated_at = NOW() WHERE account_id = $2 AND folder_name = $3",
        )
        .bind(uid)
        .bind(account_id)
        .bind(folder_name)
        .execute(&self.pool)
        .await
        .context("Failed to update last_synced_uid")?;
        Ok(())
    }

    /// Update last_full_sync_uid for a folder after full (body) sync.
    pub async fn update_folder_last_full_sync_uid(
        &self,
        folder_name: &str,
        uid: i32,
    ) -> Result<()> {
        self.update_folder_last_full_sync_uid_for_account(DEFAULT_ACCOUNT_ID, folder_name, uid)
            .await
    }

    pub async fn update_folder_last_full_sync_uid_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uid: i32,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE imap_folders SET last_full_sync_uid = $1, updated_at = NOW() WHERE account_id = $2 AND folder_name = $3",
        )
        .bind(uid)
        .bind(account_id)
        .bind(folder_name)
        .execute(&self.pool)
        .await
        .context("Failed to update last_full_sync_uid")?;
        Ok(())
    }

    /// Update highest_modseq for a folder. Only updates if new_modseq is greater than current highest_modseq.
    pub async fn update_folder_highest_modseq(
        &self,
        folder_name: &str,
        new_modseq: i64,
    ) -> Result<()> {
        self.update_folder_highest_modseq_for_account(DEFAULT_ACCOUNT_ID, folder_name, new_modseq)
            .await
    }

    pub async fn update_folder_highest_modseq_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        new_modseq: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE imap_folders SET highest_modseq = GREATEST(COALESCE(highest_modseq, 0), $1), updated_at = NOW() WHERE account_id = $2 AND folder_name = $3",
        )
        .bind(new_modseq)
        .bind(account_id)
        .bind(folder_name)
        .execute(&self.pool)
        .await
        .context("Failed to update highest_modseq")?;
        Ok(())
    }

    /// Mark by message_id (fallback when location+uid mark fails). Updates specific rows by primary key.
    pub async fn mark_emails_body_synced_by_message_id(
        &self,
        message_ids: &[String],
    ) -> Result<u64> {
        self.mark_emails_body_synced_by_message_id_for_account(DEFAULT_ACCOUNT_ID, message_ids)
            .await
    }

    pub async fn mark_emails_body_synced_by_message_id_for_account(
        &self,
        account_id: &str,
        message_ids: &[String],
    ) -> Result<u64> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE emails SET body_synced_at = NOW(), updated_at = NOW() WHERE account_id = $1 AND message_id = ANY($2)",
        )
        .bind(account_id)
        .bind(message_ids)
        .execute(&self.pool)
        .await
        .context("Failed to mark emails body synced by message_id")?;
        Ok(result.rows_affected())
    }

    /// Mark all emails for (location, uids) as body-synced. Updates by folder+uid so every row
    /// for those UIDs is marked (handles duplicates). Prefer over message_id-based mark.
    pub async fn mark_emails_body_synced_by_uid(
        &self,
        folder_name: &str,
        uids: &[i32],
    ) -> Result<u64> {
        self.mark_emails_body_synced_by_uid_for_account(DEFAULT_ACCOUNT_ID, folder_name, uids)
            .await
    }

    pub async fn mark_emails_body_synced_by_uid_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uids: &[i32],
    ) -> Result<u64> {
        if uids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE emails SET body_synced_at = NOW(), updated_at = NOW() WHERE account_id = $1 AND location = $2 AND uid = ANY($3)",
        )
        .bind(account_id)
        .bind(folder_name)
        .bind(uids)
        .execute(&self.pool)
        .await
        .context("Failed to mark emails body synced")?;
        Ok(result.rows_affected())
    }

    /// Emails needing body sync: (uid, message_id, uid_validity) for a folder where body not yet fetched.
    /// Uses last_full_sync_uid to fetch next batch low-to-high.
    pub async fn get_emails_needing_body_sync(
        &self,
        folder_name: &str,
        limit: i32,
    ) -> Result<Vec<(i32, String, i32)>> {
        self.get_emails_needing_body_sync_for_account(DEFAULT_ACCOUNT_ID, folder_name, limit)
            .await
    }

    pub async fn get_emails_needing_body_sync_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        limit: i32,
    ) -> Result<Vec<(i32, String, i32)>> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT ON (uid) uid, message_id, COALESCE(uid_validity, 0) AS uid_validity
            FROM emails
            WHERE account_id = $1
              AND location = $2
              AND uid IS NOT NULL
              AND body_synced_at IS NULL
            ORDER BY uid, message_id
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(folder_name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get emails needing body sync")?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<i32, _>("uid"),
                    r.get::<String, _>("message_id"),
                    r.get::<i32, _>("uid_validity"),
                )
            })
            .collect())
    }
}

impl Database {
    /// Accepts (name, is_noselect, delimiter) per folder (e.g. from imap::list_mailboxes_with_attributes).
    pub async fn sync_folders_from_imap(
        &self,
        imap_folders: &[(String, bool, Option<String>)],
    ) -> Result<FolderSyncResult> {
        self.sync_folders_from_imap_for_account(DEFAULT_ACCOUNT_ID, imap_folders)
            .await
    }

    pub async fn sync_folders_from_imap_for_account(
        &self,
        account_id: &str,
        imap_folders: &[(String, bool, Option<String>)],
    ) -> Result<FolderSyncResult> {
        let payload: Vec<serde_json::Value> = imap_folders
            .iter()
            .map(|(name, is_noselect, delimiter)| {
                serde_json::json!({
                    "name": name,
                    "is_noselect": is_noselect,
                    "delimiter": delimiter,
                    "attributes": []
                })
            })
            .collect();
        sqlx::query("SELECT upsert_imap_folders_from_list($1, $2)")
            .bind(account_id)
            .bind(Json(serde_json::Value::Array(payload)))
            .execute(&self.pool)
            .await
            .context("Failed to sync folders from IMAP")?;

        // Recompute priority after syncing folders
        self.recompute_imap_folder_priority_for_account(account_id)
            .await
            .context("Failed to recompute folder priority after sync")?;

        Ok(FolderSyncResult {
            new_count: imap_folders.len() as i32,
            deleted_count: 0,
            restored_count: 0,
            updated_count: 0,
        })
    }

    /// Update message_count for folders (from IMAP EXISTS). Also recomputes priority since it depends on message_count.
    pub async fn update_imap_folder_message_counts(&self, updates: &[(String, i32)]) -> Result<()> {
        self.update_imap_folder_message_counts_for_account(DEFAULT_ACCOUNT_ID, updates)
            .await
    }

    pub async fn update_imap_folder_message_counts_for_account(
        &self,
        account_id: &str,
        updates: &[(String, i32)],
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let payload: Vec<serde_json::Value> = updates
            .iter()
            .map(|(name, count)| serde_json::json!({ "folder_name": name, "message_count": count }))
            .collect();
        sqlx::query("SELECT update_imap_folders_message_counts($1, $2)")
            .bind(account_id)
            .bind(Json(serde_json::Value::Array(payload)))
            .execute(&self.pool)
            .await
            .context("Failed to update imap folder message counts")?;

        // Recompute priority after updating message counts (priority depends on message_count for non-INBOX/non-trash folders)
        self.recompute_imap_folder_priority_for_account(account_id)
            .await
            .context("Failed to recompute folder priority after message count update")?;

        Ok(())
    }

    /// Recompute imap_folders.priority from folder names and message_count.
    pub async fn recompute_imap_folder_priority(&self) -> Result<()> {
        self.recompute_imap_folder_priority_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn recompute_imap_folder_priority_for_account(&self, account_id: &str) -> Result<()> {
        sqlx::query("SELECT recompute_imap_folder_priority($1)")
            .bind(account_id)
            .execute(&self.pool)
            .await
            .context("Failed to recompute imap folder priority")?;
        Ok(())
    }

    /// Emails needing IMAP backfill: null subject, sender, received_date, raw_email_content, body_text, message_size, or message_tokens.
    /// Returns (message_id, location, uid, uid_validity, fill_subject, fill_sender, fill_received_date, fill_raw, fill_body, fill_size).
    /// fill_* = true means that column is NULL/empty in DB and should be filled from IMAP fetch.
    pub async fn get_emails_needing_imap_backfill(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, i32, i32, bool, bool, bool, bool, bool, bool)>> {
        self.get_emails_needing_imap_backfill_for_account(DEFAULT_ACCOUNT_ID, limit)
            .await
    }

    pub async fn get_emails_needing_imap_backfill_for_account(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, i32, i32, bool, bool, bool, bool, bool, bool)>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, location, uid, uid_validity,
                   (subject IS NULL OR TRIM(subject) = '') AS fill_subject,
                   (sender IS NULL OR TRIM(sender) = '') AS fill_sender,
                   received_date IS NULL AS fill_received_date,
                   raw_email_content IS NULL AS fill_raw,
                   body_text IS NULL AS fill_body,
                   message_size IS NULL AS fill_size
            FROM emails
            WHERE account_id = $1
              AND location IS NOT NULL AND uid IS NOT NULL AND uid_validity IS NOT NULL
              AND (subject IS NULL OR TRIM(subject) = '' OR sender IS NULL OR TRIM(sender) = ''
                   OR received_date IS NULL OR raw_email_content IS NULL OR body_text IS NULL
                   OR message_size IS NULL OR message_tokens IS NULL)
            ORDER BY received_date DESC NULLS LAST, location, uid
            LIMIT $2
            "#,
        )
        .bind(account_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_emails_needing_imap_backfill")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("message_id"),
                    r.get::<String, _>("location"),
                    r.get::<i32, _>("uid"),
                    r.get::<i32, _>("uid_validity"),
                    r.get::<bool, _>("fill_subject"),
                    r.get::<bool, _>("fill_sender"),
                    r.get::<bool, _>("fill_received_date"),
                    r.get::<bool, _>("fill_raw"),
                    r.get::<bool, _>("fill_body"),
                    r.get::<bool, _>("fill_size"),
                )
            })
            .collect())
    }

    /// Backfill message_tokens for rows that have body_text or raw_email_content but no message_tokens.
    /// Uses chars/4 heuristic. Returns count of rows updated.
    pub async fn backfill_message_tokens(&self) -> Result<i64> {
        self.backfill_message_tokens_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    /// Backfill message_tokens for a single account.
    pub async fn backfill_message_tokens_for_account(&self, account_id: &str) -> Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT backfill_message_tokens($1)")
            .bind(account_id)
            .fetch_one(&self.pool)
            .await
            .context("backfill_message_tokens_for_account")?;
        Ok(n)
    }

    /// Record a sync window run (window_days=0 => full sync).
    pub async fn record_sync_window_run(&self, window_days: i32) -> Result<()> {
        self.record_sync_window_run_for_account(DEFAULT_ACCOUNT_ID, window_days)
            .await
    }

    pub async fn record_sync_window_run_for_account(
        &self,
        account_id: &str,
        window_days: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO sync_window_runs (account_id, window_days, last_run_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (account_id, window_days) DO UPDATE SET last_run_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(window_days)
        .execute(&self.pool)
        .await
        .context("Failed to record sync window run")?;
        Ok(())
    }

    /// Get highest_modseq for a folder.
    pub async fn get_folder_highest_modseq(&self, folder_name: &str) -> Result<Option<i64>> {
        self.get_folder_highest_modseq_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn get_folder_highest_modseq_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<Option<i64>> {
        let row = sqlx::query(
            "SELECT highest_modseq FROM imap_folders WHERE account_id = $1 AND folder_name = $2",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get highest_modseq")?;
        Ok(row.and_then(|r| r.get("highest_modseq")))
    }

    /// Get uid_validity for a folder from imap_folders (fallback to emails table).
    pub async fn get_folder_uid_validity(&self, folder_name: &str) -> Result<Option<i32>> {
        self.get_folder_uid_validity_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn get_folder_uid_validity_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<Option<i32>> {
        let row = sqlx::query(
            "SELECT uid_validity FROM imap_folders WHERE account_id = $1 AND folder_name = $2",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get uid_validity from imap_folders")?;
        let from_folders: Option<i32> = row.and_then(|r| r.get("uid_validity"));
        if from_folders.is_some() {
            return Ok(from_folders);
        }

        let row = sqlx::query(
            "SELECT DISTINCT uid_validity FROM emails WHERE account_id = $1 AND location = $2 AND uid_validity IS NOT NULL LIMIT 1",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get uid_validity from emails")?;
        Ok(row.and_then(|r| r.get("uid_validity")))
    }

    /// Update uid_validity for a folder in imap_folders. Returns previous value (if any).
    pub async fn update_folder_uid_validity(
        &self,
        folder_name: &str,
        uid_validity: i32,
    ) -> Result<Option<i32>> {
        self.update_folder_uid_validity_for_account(DEFAULT_ACCOUNT_ID, folder_name, uid_validity)
            .await
    }

    pub async fn update_folder_uid_validity_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uid_validity: i32,
    ) -> Result<Option<i32>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin uid_validity transaction")?;

        let previous = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT uid_validity FROM imap_folders WHERE account_id = $1 AND folder_name = $2 FOR UPDATE",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_optional(&mut *tx)
        .await
        .context("select uid_validity")?
        .flatten();

        if previous != Some(uid_validity) {
            sqlx::query(
                "UPDATE imap_folders SET uid_validity = $3, updated_at = NOW() WHERE account_id = $1 AND folder_name = $2",
            )
            .bind(account_id)
            .bind(folder_name)
            .bind(uid_validity)
            .execute(&mut *tx)
            .await
            .context("update uid_validity")?;
        }

        tx.commit()
            .await
            .context("commit uid_validity transaction")?;

        Ok(previous)
    }

    /// Reset folder sync state when UIDVALIDITY changes.
    pub async fn reset_folder_sync_state(
        &self,
        folder_name: &str,
        uid_validity: i32,
    ) -> Result<()> {
        self.reset_folder_sync_state_for_account(DEFAULT_ACCOUNT_ID, folder_name, uid_validity)
            .await
    }

    pub async fn reset_folder_sync_state_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uid_validity: i32,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE imap_folders SET last_synced_uid = NULL, last_full_sync_uid = NULL, highest_modseq = NULL, uid_validity = $3, updated_at = NOW() WHERE account_id = $1 AND folder_name = $2",
        )
        .bind(account_id)
        .bind(folder_name)
        .bind(uid_validity)
        .execute(&self.pool)
        .await
        .context("Failed to reset folder sync state")?;
        Ok(())
    }

    /// Reset selectable folder cursors so a full sync can recover from stale metadata.
    pub async fn reset_folder_sync_cursors_for_account(&self, account_id: &str) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE imap_folders SET last_synced_uid = NULL, last_full_sync_uid = NULL, highest_modseq = NULL, updated_at = NOW() WHERE account_id = $1 AND is_noselect = FALSE",
        )
        .bind(account_id)
        .execute(&self.pool)
        .await
        .context("Failed to reset folder sync cursors")?;
        Ok(result.rows_affected())
    }

    /// Clear location/uid for all emails in a folder (used on UIDVALIDITY change).
    pub async fn clear_folder_uids(&self, folder_name: &str) -> Result<u64> {
        self.clear_folder_uids_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn clear_folder_uids_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET location = NULL, uid = NULL, uid_validity = NULL, updated_at = NOW() WHERE account_id = $1 AND location = $2",
        )
        .bind(account_id)
        .bind(folder_name)
        .execute(&self.pool)
        .await
        .context("Failed to clear folder UIDs")?;
        Ok(result.rows_affected())
    }

    /// Update is_read flag for emails by UID and location.
    pub async fn update_email_flags(
        &self,
        folder_name: &str,
        updates: &[(i32, bool)],
    ) -> Result<u64> {
        self.update_email_flags_for_account(DEFAULT_ACCOUNT_ID, folder_name, updates)
            .await
    }

    pub async fn update_email_flags_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        updates: &[(i32, bool)],
    ) -> Result<u64> {
        if updates.is_empty() {
            return Ok(0);
        }
        let (uids, flags): (Vec<i32>, Vec<bool>) = updates.iter().cloned().unzip();
        let result = sqlx::query(
            r#"
            WITH updates AS (
                SELECT * FROM UNNEST($1::int[], $2::bool[]) AS t(uid, is_read)
            )
            UPDATE emails e
            SET is_read = u.is_read, updated_at = NOW()
            FROM updates u
            WHERE e.account_id = $3
              AND e.location = $4
              AND e.uid = u.uid
            "#,
        )
        .bind(&uids)
        .bind(&flags)
        .bind(account_id)
        .bind(folder_name)
        .execute(&self.pool)
        .await
        .context("Failed to update email flags")?;
        Ok(result.rows_affected())
    }

    /// Message_ids with location IS NULL and not yet confirmed deleted; can be reattached or marked deleted.
    pub async fn get_orphan_message_ids(&self) -> Result<Vec<String>> {
        self.get_orphan_message_ids_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn get_orphan_message_ids_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT message_id FROM emails WHERE account_id = $1 AND location IS NULL AND message_id IS NOT NULL AND deleted_from_server_at IS NULL",
        )
            .bind(account_id)
            .fetch_all(&self.pool)
            .await
            .context("get_orphan_message_ids")?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("message_id"))
            .collect())
    }

    /// Mark a message as confirmed deleted from server (not found in any folder during resolve-orphans).
    pub async fn mark_email_deleted_from_server(&self, message_id: &str) -> Result<u64> {
        self.mark_email_deleted_from_server_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn mark_email_deleted_from_server_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET deleted_from_server_at = NOW(), updated_at = NOW() WHERE account_id = $1 AND message_id = $2",
        )
        .bind(account_id)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("mark_email_deleted_from_server")?;
        Ok(result.rows_affected())
    }

    /// Update location/uid/uid_validity for a message (e.g. after finding it in another folder when resolving moved messages).
    pub async fn update_email_location(
        &self,
        message_id: &str,
        location: &str,
        uid: i32,
        uid_validity: i32,
    ) -> Result<u64> {
        self.update_email_location_for_account(
            DEFAULT_ACCOUNT_ID,
            message_id,
            location,
            uid,
            uid_validity,
        )
        .await
    }

    pub async fn update_email_location_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        location: &str,
        uid: i32,
        uid_validity: i32,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET location = $3, uid = $4, uid_validity = $5, updated_at = NOW() WHERE account_id = $1 AND message_id = $2",
        )
        .bind(account_id)
        .bind(message_id)
        .bind(location)
        .bind(uid)
        .bind(uid_validity)
        .execute(&self.pool)
        .await
        .context("update_email_location")?;
        Ok(result.rows_affected())
    }

    pub async fn record_system_filing_move_for_account(
        &self,
        move_record: SystemFilingMoveRecord<'_>,
    ) -> Result<u64> {
        let previous_location = sqlx::query(
            r#"
            SELECT location
            FROM emails
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(move_record.account_id)
        .bind(move_record.message_id)
        .fetch_optional(&self.pool)
        .await
        .context("load previous email location")?
        .and_then(|row| row.get::<Option<String>, _>("location"));

        let result = sqlx::query(
            r#"
            UPDATE emails
            SET location = $3,
                uid = $4,
                uid_validity = $5,
                last_filed_at = NOW(),
                last_filed_by = $6,
                filing_lock_until = NOW() + ($7 * INTERVAL '1 hour'),
                move_count = COALESCE(move_count, 0) + 1,
                user_pinned_folder = CASE
                    WHEN user_pinned_folder IS NOT NULL
                     AND LOWER(user_pinned_folder) = LOWER($3)
                    THEN NULL
                    ELSE user_pinned_folder
                END,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(move_record.account_id)
        .bind(move_record.message_id)
        .bind(move_record.location)
        .bind(move_record.uid)
        .bind(move_record.uid_validity)
        .bind(move_record.actor)
        .bind(move_record.cooldown_hours.max(1))
        .execute(&self.pool)
        .await
        .context("record_system_filing_move")?;

        if result.rows_affected() > 0
            && previous_location
                .as_deref()
                .map(|previous| !previous.eq_ignore_ascii_case(move_record.location))
                .unwrap_or(true)
        {
            let _ = sqlx::query(
                r#"
                INSERT INTO email_location_events (
                    account_id, message_id, event_type, actor, from_folder,
                    to_folder, reason, confidence, metadata
                )
                VALUES ($1, $2, 'system_moved', 'core', $3, $4, $5, 1.0, $6)
                "#,
            )
            .bind(move_record.account_id)
            .bind(move_record.message_id)
            .bind(previous_location.as_deref())
            .bind(move_record.location)
            .bind("file_apply")
            .bind(Json(serde_json::json!({
                "last_filed_by": move_record.actor,
                "cooldown_hours": move_record.cooldown_hours.max(1)
            })))
            .execute(&self.pool)
            .await
            .context("record system location event")?;
        }

        Ok(result.rows_affected())
    }
}

impl Database {
    /// Mark emails as deleted (expunged) by UID and location.
    /// Sets location to NULL to mark as deleted/moved.
    pub async fn mark_emails_expunged(&self, folder_name: &str, uids: &[i32]) -> Result<u64> {
        self.mark_emails_expunged_for_account(DEFAULT_ACCOUNT_ID, folder_name, uids)
            .await
    }

    pub async fn mark_emails_expunged_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uids: &[i32],
    ) -> Result<u64> {
        if uids.is_empty() {
            return Ok(0);
        }
        // Set location to NULL to mark as deleted/moved
        let result = sqlx::query(
            "UPDATE emails SET location = NULL, uid = NULL, uid_validity = NULL, updated_at = NOW() WHERE account_id = $1 AND location = $2 AND uid = ANY($3)",
        )
        .bind(account_id)
        .bind(folder_name)
        .bind(uids)
        .execute(&self.pool)
        .await
        .context("Failed to mark emails as expunged")?;
        Ok(result.rows_affected())
    }

    /// Get all UIDs currently in database for a folder.
    pub async fn get_folder_uids(&self, folder_name: &str) -> Result<Vec<i32>> {
        self.get_folder_uids_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn get_folder_uids_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<Vec<i32>> {
        let rows = sqlx::query(
            "SELECT DISTINCT uid FROM emails WHERE account_id = $1 AND location = $2 AND uid IS NOT NULL ORDER BY uid",
        )
        .bind(account_id)
        .bind(folder_name)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get folder UIDs")?;
        Ok(rows.into_iter().filter_map(|r| r.get("uid")).collect())
    }

    /// Check if message_id exists in another folder (for detecting moves).
    pub async fn find_message_location(&self, message_id: &str) -> Result<Option<String>> {
        self.find_message_location_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn find_message_location_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT location FROM emails WHERE account_id = $1 AND message_id = $2 AND location IS NOT NULL LIMIT 1",
        )
        .bind(account_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to find message location")?;
        Ok(row.and_then(|r| r.get("location")))
    }

    /// Remove one folder from imap_folders (e.g. after deleting it from server when it was NOSELECT).
    pub async fn delete_imap_folder(&self, folder_name: &str) -> Result<u64> {
        self.delete_imap_folder_for_account(DEFAULT_ACCOUNT_ID, folder_name)
            .await
    }

    pub async fn delete_imap_folder_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
    ) -> Result<u64> {
        let result =
            sqlx::query("DELETE FROM imap_folders WHERE account_id = $1 AND folder_name = $2")
                .bind(account_id)
                .bind(folder_name)
                .execute(&self.pool)
                .await
                .context("delete_imap_folder")?;
        Ok(result.rows_affected())
    }

    pub async fn list_imap_folders(&self) -> Result<Vec<ImapFolder>> {
        self.list_imap_folders_for_account(DEFAULT_ACCOUNT_ID).await
    }

    pub async fn list_imap_folders_for_account(&self, account_id: &str) -> Result<Vec<ImapFolder>> {
        let rows = sqlx::query(
            "SELECT folder_name, delimiter, is_noselect, attributes, last_synced_uid, last_full_sync_uid, message_count, priority FROM imap_folders WHERE account_id = $1 ORDER BY folder_name",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list imap folders")?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let attrs: Option<Vec<String>> = r.try_get("attributes").ok();
                ImapFolder {
                    folder_name: r.get("folder_name"),
                    delimiter: r.get("delimiter"),
                    is_noselect: r.get::<Option<bool>, _>("is_noselect").unwrap_or(false),
                    attributes: attrs.unwrap_or_default(),
                    last_synced_uid: r.get("last_synced_uid"),
                    last_full_sync_uid: r.try_get("last_full_sync_uid").ok().flatten(),
                    message_count: r.get("message_count"),
                    priority: r.get("priority"),
                }
            })
            .collect())
    }
}
