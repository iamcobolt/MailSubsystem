use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{types::Json, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::{email_record_from_row, json_column};
use crate::db::{Database, EmailRecord};

#[derive(Debug, Clone)]
pub struct BatchSummary {
    pub batch_id: String,
    pub status: String,
    pub email_count: i32,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub quality_score: Option<f32>,
}

impl Database {
    pub async fn create_batch(&self, batch_id: &str, email_count: i32) -> Result<()> {
        self.create_batch_for_account(DEFAULT_ACCOUNT_ID, batch_id, email_count)
            .await
    }

    pub async fn create_batch_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
        email_count: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO analysis_batches (
                account_id, batch_id, email_count, status, created_at, updated_at
            )
            VALUES ($1, $2, $3, 'pending', NOW(), NOW())
            ON CONFLICT (account_id, batch_id) DO UPDATE
            SET email_count = EXCLUDED.email_count,
                status = 'pending',
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .bind(email_count)
        .execute(&self.pool)
        .await
        .context("create_batch_for_account")?;
        Ok(())
    }

    pub async fn assign_emails_to_batch(
        &self,
        batch_id: &str,
        message_ids: &[String],
    ) -> Result<u64> {
        self.assign_emails_to_batch_for_account(DEFAULT_ACCOUNT_ID, batch_id, message_ids)
            .await
    }

    pub async fn assign_emails_to_batch_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
        message_ids: &[String],
    ) -> Result<u64> {
        if message_ids.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query(
            r#"
            UPDATE emails
            SET batch_id = $3,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = ANY($2)
              AND deleted_from_server_at IS NULL
            "#,
        )
        .bind(account_id)
        .bind(message_ids)
        .bind(batch_id)
        .execute(&self.pool)
        .await
        .context("assign_emails_to_batch_for_account")?;
        Ok(result.rows_affected())
    }

    pub async fn update_batch_status(&self, batch_id: &str, status: &str) -> Result<()> {
        self.update_batch_status_for_account(DEFAULT_ACCOUNT_ID, batch_id, status)
            .await
    }

    pub async fn update_batch_status_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
        status: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE analysis_batches
            SET status = $3,
                completed_at = CASE WHEN $3 = 'completed' THEN COALESCE(completed_at, NOW()) ELSE completed_at END,
                updated_at = NOW()
            WHERE account_id = $1
              AND batch_id = $2
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .bind(status)
        .execute(&self.pool)
        .await
        .context("update_batch_status_for_account")?;
        Ok(())
    }

    pub async fn save_batch_plan(&self, batch_id: &str, plan: &Value) -> Result<()> {
        self.save_batch_plan_for_account(DEFAULT_ACCOUNT_ID, batch_id, plan)
            .await
    }

    pub async fn save_batch_plan_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
        plan: &Value,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE analysis_batches
            SET orchestrator_plan = $3,
                status = 'planning',
                updated_at = NOW()
            WHERE account_id = $1
              AND batch_id = $2
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .bind(Json(plan.clone()))
        .execute(&self.pool)
        .await
        .context("save_batch_plan_for_account")?;
        Ok(())
    }

    pub async fn save_batch_review(
        &self,
        batch_id: &str,
        review: &Value,
        quality_score: Option<f32>,
    ) -> Result<()> {
        self.save_batch_review_for_account(DEFAULT_ACCOUNT_ID, batch_id, review, quality_score)
            .await
    }

    pub async fn save_batch_review_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
        review: &Value,
        quality_score: Option<f32>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE analysis_batches
            SET orchestrator_review = $3,
                quality_score = COALESCE($4, quality_score),
                status = 'reviewing',
                updated_at = NOW()
            WHERE account_id = $1
              AND batch_id = $2
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .bind(Json(review.clone()))
        .bind(quality_score)
        .execute(&self.pool)
        .await
        .context("save_batch_review_for_account")?;
        Ok(())
    }

    pub async fn complete_batch(&self, batch_id: &str) -> Result<()> {
        self.complete_batch_for_account(DEFAULT_ACCOUNT_ID, batch_id)
            .await
    }

    pub async fn complete_batch_for_account(&self, account_id: &str, batch_id: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE analysis_batches
            SET status = 'completed',
                completed_at = NOW(),
                updated_at = NOW()
            WHERE account_id = $1
              AND batch_id = $2
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .execute(&self.pool)
        .await
        .context("complete_batch_for_account")?;
        Ok(())
    }

    pub async fn get_batch_emails(&self, batch_id: &str) -> Result<Vec<EmailRecord>> {
        self.get_batch_emails_for_account(DEFAULT_ACCOUNT_ID, batch_id)
            .await
    }

    pub async fn get_batch_emails_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
    ) -> Result<Vec<EmailRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic,
                   location, location_recommendation, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND batch_id = $2
              AND deleted_from_server_at IS NULL
            ORDER BY received_date DESC NULLS LAST
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await
        .context("get_batch_emails_for_account")?;

        Ok(rows.iter().map(email_record_from_row).collect())
    }

    pub async fn get_batch_results(&self, batch_id: &str) -> Result<Vec<(String, Option<Value>)>> {
        self.get_batch_results_for_account(DEFAULT_ACCOUNT_ID, batch_id)
            .await
    }

    pub async fn get_batch_results_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
    ) -> Result<Vec<(String, Option<Value>)>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, ai_summary
            FROM emails
            WHERE account_id = $1
              AND batch_id = $2
              AND deleted_from_server_at IS NULL
            ORDER BY received_date DESC NULLS LAST
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await
        .context("get_batch_results_for_account")?;

        Ok(rows
            .iter()
            .map(|row| (row.get("message_id"), json_column(row, "ai_summary")))
            .collect())
    }

    pub async fn get_recent_batches(&self, limit: usize) -> Result<Vec<BatchSummary>> {
        self.get_recent_batches_for_account(DEFAULT_ACCOUNT_ID, limit)
            .await
    }

    pub async fn get_recent_batches_for_account(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<BatchSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT batch_id, status, email_count, created_at, completed_at, quality_score
            FROM analysis_batches
            WHERE account_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
        )
        .bind(account_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_recent_batches_for_account")?;

        Ok(rows
            .iter()
            .map(|row| BatchSummary {
                batch_id: row.get("batch_id"),
                status: row.get("status"),
                email_count: row.get("email_count"),
                created_at: row.get("created_at"),
                completed_at: row.get("completed_at"),
                quality_score: row.get("quality_score"),
            })
            .collect())
    }

    pub async fn get_emails_needing_reanalysis(&self, batch_id: &str) -> Result<Vec<EmailRecord>> {
        self.get_emails_needing_reanalysis_for_account(DEFAULT_ACCOUNT_ID, batch_id)
            .await
    }

    pub async fn get_emails_needing_reanalysis_for_account(
        &self,
        account_id: &str,
        batch_id: &str,
    ) -> Result<Vec<EmailRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic,
                   location, location_recommendation, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND batch_id = $2
              AND reanalysis_requested = TRUE
              AND deleted_from_server_at IS NULL
              AND (
                analyzed_by IS NULL
                OR analyzed_by NOT LIKE 'orchestrator-reanalysis:%'
              )
            ORDER BY received_date DESC NULLS LAST
            "#,
        )
        .bind(account_id)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await
        .context("get_emails_needing_reanalysis_for_account")?;

        Ok(rows.iter().map(email_record_from_row).collect())
    }

    pub async fn flag_for_reanalysis(&self, message_id: &str, reason: &str) -> Result<()> {
        self.flag_for_reanalysis_for_account(DEFAULT_ACCOUNT_ID, message_id, reason)
            .await
    }

    pub async fn flag_for_reanalysis_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        reason: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET reanalysis_requested = TRUE,
                reanalysis_reason = $3,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(reason)
        .execute(&self.pool)
        .await
        .context("flag_for_reanalysis_for_account")?;
        Ok(())
    }

    pub async fn clear_reanalysis_flag(&self, message_id: &str) -> Result<()> {
        self.clear_reanalysis_flag_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn clear_reanalysis_flag_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET reanalysis_requested = FALSE,
                reanalysis_reason = NULL,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("clear_reanalysis_flag_for_account")?;
        Ok(())
    }
}
