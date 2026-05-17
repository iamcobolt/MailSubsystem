use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::types::Json;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{Database, StoreEmailInput};

impl Database {
    /// Upsert a single email. Uses upsert_emails_from_import with a single-element payload.
    pub async fn store_email(&self, input: &StoreEmailInput<'_>) -> Result<()> {
        self.store_email_for_account(DEFAULT_ACCOUNT_ID, input)
            .await
    }

    pub async fn store_email_for_account(
        &self,
        account_id: &str,
        input: &StoreEmailInput<'_>,
    ) -> Result<()> {
        let _ = input.ai_summary;
        let elem = serde_json::json!({
            "message_id": input.message_id,
            "subject": input.subject,
            "sender": input.sender,
            "received_date": input.received_date.map(|d| d.to_rfc3339()),
            "location": input.location,
            "modseq": input.modseq,
            "uid": input.uid,
            "uid_validity": input.uid_validity,
            "raw_email_content": input.raw_email_content,
            "body_text": input.body_text,
        });
        let payload = serde_json::json!([elem]);
        sqlx::query("SELECT upsert_emails_from_import($1, $2)")
            .bind(account_id)
            .bind(Json(payload))
            .execute(&self.pool)
            .await
            .context("Failed to store email")?;
        Ok(())
    }

    /// Batch upsert emails via upsert_emails_from_import.
    pub async fn upsert_emails_from_import(&self, payload: &Value) -> Result<()> {
        self.upsert_emails_from_import_for_account(DEFAULT_ACCOUNT_ID, payload)
            .await
    }

    pub async fn upsert_emails_from_import_for_account(
        &self,
        account_id: &str,
        payload: &Value,
    ) -> Result<()> {
        sqlx::query("SELECT upsert_emails_from_import($1, $2)")
            .bind(account_id)
            .bind(Json(payload))
            .execute(&self.pool)
            .await
            .context("Failed to upsert emails from import")?;
        Ok(())
    }

    /// Upsert full messages and mark body_synced_at in one transaction.
    /// Mark by message_ids when provided; otherwise by (folder_name, uids).
    pub async fn upsert_and_mark_body_synced(
        &self,
        payload: &Value,
        folder_name: Option<&str>,
        uids: Option<&[i32]>,
        message_ids: Option<&[String]>,
    ) -> Result<u64> {
        self.upsert_and_mark_body_synced_for_account(
            DEFAULT_ACCOUNT_ID,
            payload,
            folder_name,
            uids,
            message_ids,
        )
        .await
    }

    pub async fn upsert_and_mark_body_synced_for_account(
        &self,
        account_id: &str,
        payload: &Value,
        folder_name: Option<&str>,
        uids: Option<&[i32]>,
        message_ids: Option<&[String]>,
    ) -> Result<u64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("Failed to begin transaction")?;
        sqlx::query("SELECT upsert_emails_from_import($1, $2)")
            .bind(account_id)
            .bind(Json(payload))
            .execute(&mut *tx)
            .await
            .context("Failed to upsert emails from import")?;

        let n = if let Some(ids) = message_ids {
            if ids.is_empty() {
                0
            } else {
                let result = sqlx::query(
                    "UPDATE emails SET body_synced_at = NOW(), updated_at = NOW() WHERE account_id = $1 AND message_id = ANY($2)",
                )
                .bind(account_id)
                .bind(ids)
                .execute(&mut *tx)
                .await
                .context("Failed to mark emails body synced")?;
                result.rows_affected()
            }
        } else if let (Some(folder), Some(uid_list)) = (folder_name, uids) {
            if uid_list.is_empty() {
                0
            } else {
                let result = sqlx::query(
                    "UPDATE emails SET body_synced_at = NOW(), updated_at = NOW() WHERE account_id = $1 AND location = $2 AND uid = ANY($3)",
                )
                .bind(account_id)
                .bind(folder)
                .bind(uid_list)
                .execute(&mut *tx)
                .await
                .context("Failed to mark emails body synced")?;
                result.rows_affected()
            }
        } else {
            0
        };

        tx.commit().await.context("Failed to commit transaction")?;
        Ok(n)
    }

    /// Record (folder, uid, uid_validity) to emails_missing_message_id for later review.
    pub async fn record_missing_message_id(
        &self,
        folder_name: &str,
        uid: i32,
        uid_validity: i32,
    ) -> Result<()> {
        self.record_missing_message_id_for_account(
            DEFAULT_ACCOUNT_ID,
            folder_name,
            uid,
            uid_validity,
        )
        .await
    }

    pub async fn record_missing_message_id_for_account(
        &self,
        account_id: &str,
        folder_name: &str,
        uid: i32,
        uid_validity: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO emails_missing_message_id (account_id, folder_name, uid, uid_validity, attempted_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (account_id, folder_name, uid, uid_validity) DO UPDATE SET attempted_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(folder_name)
        .bind(uid)
        .bind(uid_validity)
        .execute(&self.pool)
        .await
        .context("Failed to record missing message_id")?;
        Ok(())
    }

    /// Upsert emails from IMAP envelope data via upsert_emails_from_envelope.
    pub async fn upsert_emails_from_envelope(&self, payload: &Value) -> Result<()> {
        self.upsert_emails_from_envelope_for_account(DEFAULT_ACCOUNT_ID, payload)
            .await
    }

    pub async fn upsert_emails_from_envelope_for_account(
        &self,
        account_id: &str,
        payload: &Value,
    ) -> Result<()> {
        sqlx::query("SELECT upsert_emails_from_envelope($1, $2)")
            .bind(account_id)
            .bind(Json(payload))
            .execute(&self.pool)
            .await
            .context("Failed to upsert emails from envelope (ensure schema.sql is applied)")?;
        Ok(())
    }
}
