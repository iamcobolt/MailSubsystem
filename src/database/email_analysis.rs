use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{types::Json, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::email_record_from_row;
use crate::db::{Database, EmailRecord};

/// One row for the file command: emails that have a pending recommendation different from current location.
#[derive(Debug, Clone)]
pub struct PendingLocationApply {
    pub message_id: String,
    pub location: Option<String>,
    pub location_recommendation: String,
    pub location_create_if_missing: Option<bool>,
    pub uid: Option<i32>,
    pub uid_validity: Option<i32>,
}

/// Per-folder aggregate for location agent (organizations/categories in a folder).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FolderSummary {
    pub folder: String,
    pub count: i64,
    pub organizations: Vec<String>,
    pub categories: Vec<String>,
}

/// Sampled email rows from one folder for consolidation analysis.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FolderEmailSample {
    pub message_id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub category: Option<String>,
    pub email_type: Option<String>,
    pub organization: Option<String>,
    pub human_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateAiFieldsInput<'a> {
    pub message_id: &'a str,
    pub spam_status: Option<&'a str>,
    pub phishing_status: Option<&'a str>,
    pub marketing_status: Option<&'a str>,
    pub otp_status: Option<&'a str>,
    pub otp_code: Option<&'a str>,
    pub otp_expires: Option<DateTime<Utc>>,
    pub threat_level: Option<&'a str>,
    pub threat_indicators: Option<&'a Value>,
    pub ai_summary: Option<&'a Value>,
    pub human_summary: Option<&'a str>,
    pub category: Option<&'a str>,
    pub subcategory: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub email_type: Option<&'a str>,
    pub location_recommendation: Option<&'a str>,
    pub location_create_if_missing: Option<bool>,
    pub offer_expires: Option<DateTime<Utc>>,
}

/// AI-related columns only (for show command / verifying DB state).
#[derive(Debug)]
pub struct EmailAIFields {
    pub analyzed_by: Option<String>,
    pub spam_status: Option<String>,
    pub phishing_status: Option<String>,
    pub marketing_status: Option<String>,
    pub otp_status: Option<String>,
    pub otp_code: Option<String>,
    pub threat_level: Option<String>,
    pub threat_indicators: Option<Value>,
    pub ai_summary: Option<Value>,
    pub human_summary: Option<String>,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub organization: Option<String>,
    pub topic: Option<String>,
    pub email_type: Option<String>,
    pub location_recommendation: Option<String>,
}

impl Database {
    /// Fetch only AI-related columns for one email (for verifying what was persisted).
    pub async fn get_email_ai_fields(&self, message_id: &str) -> Result<Option<EmailAIFields>> {
        self.get_email_ai_fields_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn get_email_ai_fields_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<Option<EmailAIFields>> {
        let row = sqlx::query(
            r#"
            SELECT analyzed_by, spam_status, phishing_status, marketing_status, otp_status, otp_code,
                   threat_level, threat_indicators, ai_summary, human_summary, category,
                   subcategory, organization, topic, email_type, location_recommendation
            FROM emails
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_email_ai_fields")?;

        Ok(row.map(|row| {
            let ai_summary: Option<Json<Value>> = row.try_get("ai_summary").ok();
            EmailAIFields {
                analyzed_by: row.try_get("analyzed_by").ok(),
                spam_status: row.try_get("spam_status").ok(),
                phishing_status: row.try_get("phishing_status").ok(),
                marketing_status: row.try_get("marketing_status").ok(),
                otp_status: row.try_get("otp_status").ok(),
                otp_code: row.try_get("otp_code").ok(),
                threat_level: row.try_get("threat_level").ok(),
                threat_indicators: row
                    .try_get::<Json<Value>, _>("threat_indicators")
                    .ok()
                    .map(|json| json.0),
                ai_summary: ai_summary.map(|j| j.0),
                human_summary: row.try_get("human_summary").ok(),
                category: row.try_get("category").ok(),
                subcategory: row.try_get("subcategory").ok(),
                organization: row.try_get("organization").ok(),
                topic: row.try_get("topic").ok(),
                email_type: row.try_get("email_type").ok(),
                location_recommendation: row.try_get("location_recommendation").ok(),
            }
        }))
    }

    pub async fn get_all_message_ids(&self) -> Result<Vec<String>> {
        self.get_all_message_ids_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn get_all_message_ids_for_account(&self, account_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT message_id FROM emails WHERE account_id = $1")
            .bind(account_id)
            .fetch_all(&self.pool)
            .await
            .context("Failed to get message ids")?;
        Ok(rows
            .iter()
            .map(|r| r.get::<String, _>("message_id"))
            .collect())
    }

    pub async fn get_message_ids_by_location(&self, location: &str) -> Result<Vec<String>> {
        self.get_message_ids_by_location_for_account(DEFAULT_ACCOUNT_ID, location)
            .await
    }

    pub async fn get_message_ids_by_location_for_account(
        &self,
        account_id: &str,
        location: &str,
    ) -> Result<Vec<String>> {
        let rows =
            sqlx::query("SELECT message_id FROM emails WHERE account_id = $1 AND location = $2")
                .bind(account_id)
                .bind(location)
                .fetch_all(&self.pool)
                .await
                .context("Failed to get message ids by location")?;
        Ok(rows
            .iter()
            .map(|r| r.get::<String, _>("message_id"))
            .collect())
    }
}

impl Database {
    /// Folder tree for location agent: (folder_path, is_noselect). Thin wrapper around list_imap_folders.
    pub async fn get_folder_tree_for_location(&self) -> Result<Vec<(String, bool)>> {
        self.get_folder_tree_for_location_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn get_folder_tree_for_location_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<(String, bool)>> {
        let folders = self
            .list_imap_folders_for_account(account_id)
            .await
            .context("list_imap_folders")?;
        Ok(folders
            .into_iter()
            .map(|f| (f.folder_name, f.is_noselect))
            .collect())
    }

    /// Per-folder summary for location agent: count and top organizations/categories for emails in one folder.
    pub async fn get_per_folder_summaries(
        &self,
        folder_path: &str,
        limit: usize,
    ) -> Result<FolderSummary> {
        self.get_per_folder_summaries_for_account(DEFAULT_ACCOUNT_ID, folder_path, limit)
            .await
    }

    pub async fn get_per_folder_summaries_for_account(
        &self,
        account_id: &str,
        folder_path: &str,
        limit: usize,
    ) -> Result<FolderSummary> {
        let limit_i64 = limit as i64;
        let count_row = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM emails WHERE account_id = $1 AND location = $2",
        )
        .bind(account_id)
        .bind(folder_path)
        .fetch_one(&self.pool)
        .await
        .context("get_per_folder_summaries count")?;

        let org_rows = sqlx::query(
            "SELECT organization FROM emails WHERE account_id = $1 AND location = $2 AND organization IS NOT NULL GROUP BY organization ORDER BY COUNT(*) DESC LIMIT $3",
        )
        .bind(account_id)
        .bind(folder_path)
        .bind(limit_i64)
        .fetch_all(&self.pool)
        .await
        .context("get_per_folder_summaries organizations")?;
        let organizations: Vec<String> = org_rows
            .iter()
            .filter_map(|r| r.get::<Option<String>, _>("organization"))
            .collect();

        let cat_rows = sqlx::query(
            "SELECT category FROM emails WHERE account_id = $1 AND location = $2 AND category IS NOT NULL GROUP BY category ORDER BY COUNT(*) DESC LIMIT $3",
        )
        .bind(account_id)
        .bind(folder_path)
        .bind(limit_i64)
        .fetch_all(&self.pool)
        .await
        .context("get_per_folder_summaries categories")?;
        let categories: Vec<String> = cat_rows
            .iter()
            .filter_map(|r| r.get::<Option<String>, _>("category"))
            .collect();

        Ok(FolderSummary {
            folder: folder_path.to_string(),
            count: count_row,
            organizations,
            categories,
        })
    }

    pub async fn get_folder_email_samples(
        &self,
        folder_path: &str,
        limit: usize,
    ) -> Result<Vec<FolderEmailSample>> {
        self.get_folder_email_samples_for_account(DEFAULT_ACCOUNT_ID, folder_path, limit)
            .await
    }

    pub async fn get_folder_email_samples_for_account(
        &self,
        account_id: &str,
        folder_path: &str,
        limit: usize,
    ) -> Result<Vec<FolderEmailSample>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, category, email_type, organization, human_summary
            FROM emails
            WHERE account_id = $1
              AND location = $2
              AND deleted_from_server_at IS NULL
            ORDER BY received_date DESC NULLS LAST
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(folder_path)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_folder_email_samples")?;

        Ok(rows
            .into_iter()
            .map(|row| FolderEmailSample {
                message_id: row.get("message_id"),
                subject: row.get("subject"),
                sender: row.get("sender"),
                category: row.get("category"),
                email_type: row.get("email_type"),
                organization: row.get("organization"),
                human_summary: row.get("human_summary"),
            })
            .collect())
    }

    /// Emails eligible for location recommendation.
    /// `force=false`: only rows with no existing recommendation.
    /// `force=true`: include rows that already have a recommendation (recompute mode).
    pub async fn get_emails_needing_location(
        &self,
        limit: u32,
        force: bool,
    ) -> Result<Vec<EmailRecord>> {
        self.get_emails_needing_location_for_account(DEFAULT_ACCOUNT_ID, limit, force)
            .await
    }

    pub async fn get_emails_needing_location_for_account(
        &self,
        account_id: &str,
        limit: u32,
        force: bool,
    ) -> Result<Vec<EmailRecord>> {
        let query = if force {
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic, location_recommendation,
                   location, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND analyzed_at IS NOT NULL
              AND category IS NOT NULL
              AND location IS NOT NULL
              AND COALESCE(spam_status, '') <> 'spam'
              AND COALESCE(phishing_status, '') <> 'phishing'
              AND COALESCE(threat_level, '') NOT IN ('high', 'critical')
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
            "#
        } else {
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic, location_recommendation,
                   location, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND analyzed_at IS NOT NULL
              AND category IS NOT NULL
              AND location_recommendation IS NULL
              AND location IS NOT NULL
              AND COALESCE(spam_status, '') <> 'spam'
              AND COALESCE(phishing_status, '') <> 'phishing'
              AND COALESCE(threat_level, '') NOT IN ('high', 'critical')
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
            "#
        };

        let rows = sqlx::query(query)
            .bind(account_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .context("get_emails_needing_location")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(email_record_from_row(&row))
        }
        Ok(out)
    }

    /// Emails eligible for analysis.
    /// `force=false`: default queue semantics (unanalyzed + retry/backoff).
    /// `force=true`: include recently received rows regardless of analyzed_at/permanent failure.
    pub async fn get_unanalyzed_emails(&self, limit: u32, force: bool) -> Result<Vec<EmailRecord>> {
        self.get_unanalyzed_emails_for_account(DEFAULT_ACCOUNT_ID, limit, force)
            .await
    }

    pub async fn get_unanalyzed_emails_for_account(
        &self,
        account_id: &str,
        limit: u32,
        force: bool,
    ) -> Result<Vec<EmailRecord>> {
        let query = if force {
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
              AND deleted_from_server_at IS NULL
              AND (body_text IS NOT NULL OR raw_email_content IS NOT NULL)
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
            "#
        } else {
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
              AND analyzed_at IS NULL
              AND analysis_permanent_failure = FALSE
              AND deleted_from_server_at IS NULL
              AND (body_text IS NOT NULL OR raw_email_content IS NOT NULL)
              AND (
                  analysis_attempts = 0
                  OR analysis_failed_at IS NULL
                  OR analysis_failed_at < NOW() - (INTERVAL '1 minute' * POWER(2, LEAST(analysis_attempts, 6)))
              )
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
            "#
        };

        let rows = sqlx::query(query)
            .bind(account_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .context("get_unanalyzed_emails")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(email_record_from_row(&row));
        }
        Ok(out)
    }

    /// Recently analyzed emails used for harness benchmark replay.
    pub async fn get_emails_for_benchmark(&self, limit: usize) -> Result<Vec<EmailRecord>> {
        self.get_emails_for_benchmark_for_account(DEFAULT_ACCOUNT_ID, limit)
            .await
    }

    pub async fn get_emails_for_benchmark_for_account(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<EmailRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
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
              AND analyzed_at IS NOT NULL
              AND spam_status IS NOT NULL
              AND phishing_status IS NOT NULL
              AND category IS NOT NULL
            ORDER BY analyzed_at DESC NULLS LAST, updated_at DESC
            LIMIT $2
            "#,
        )
        .bind(account_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_emails_for_benchmark")?;

        Ok(rows.iter().map(email_record_from_row).collect())
    }

    /// Update AI analysis fields after analysis.
    pub async fn update_ai_fields(&self, input: &UpdateAiFieldsInput<'_>) -> Result<u64> {
        self.update_ai_fields_for_account(DEFAULT_ACCOUNT_ID, input)
            .await
    }

    pub async fn update_ai_fields_for_account(
        &self,
        account_id: &str,
        input: &UpdateAiFieldsInput<'_>,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE emails SET
                spam_status = COALESCE($3, spam_status),
                phishing_status = COALESCE($4, phishing_status),
                marketing_status = COALESCE($5, marketing_status),
                otp_status = COALESCE($6, otp_status),
                otp_code = COALESCE($7, otp_code),
                otp_expires = COALESCE($8, otp_expires),
                threat_level = COALESCE($9, threat_level),
                threat_indicators = COALESCE($10, threat_indicators),
                ai_summary = COALESCE($11, ai_summary),
                human_summary = COALESCE($12, human_summary),
                category = COALESCE($13, category),
                subcategory = COALESCE($14, subcategory),
                organization = COALESCE($15, organization),
                topic = COALESCE($16, topic),
                email_type = COALESCE($17, email_type),
                location_recommendation = COALESCE($18, location_recommendation),
                location_create_if_missing = COALESCE($19, location_create_if_missing),
                offer_expires = COALESCE($20, offer_expires),
                analyzed_at = NOW(),
                analysis_failed_at = NULL,
                analysis_permanent_failure = FALSE,
                last_analysis_error = NULL,
                action_status = NULL,
                action_applied_at = NULL,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(input.message_id)
        .bind(input.spam_status)
        .bind(input.phishing_status)
        .bind(input.marketing_status)
        .bind(input.otp_status)
        .bind(input.otp_code)
        .bind(input.otp_expires)
        .bind(input.threat_level)
        .bind(input.threat_indicators.cloned().map(Json::from))
        .bind(input.ai_summary.cloned().map(Json::from))
        .bind(input.human_summary)
        .bind(input.category)
        .bind(input.subcategory)
        .bind(input.organization)
        .bind(input.topic)
        .bind(input.email_type)
        .bind(input.location_recommendation)
        .bind(input.location_create_if_missing)
        .bind(input.offer_expires)
        .execute(&self.pool)
        .await
        .context("update_ai_fields")?;
        Ok(result.rows_affected())
    }

    pub async fn set_analyzed_by(
        &self,
        message_id: &str,
        analyzed_by: Option<&str>,
    ) -> Result<u64> {
        self.set_analyzed_by_for_account(DEFAULT_ACCOUNT_ID, message_id, analyzed_by)
            .await
    }

    pub async fn set_analyzed_by_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        analyzed_by: Option<&str>,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE emails
            SET analyzed_by = $3,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(analyzed_by)
        .execute(&self.pool)
        .await
        .context("set_analyzed_by")?;
        Ok(result.rows_affected())
    }

    /// Fetch analyzed emails that have not yet had security actions applied.
    pub async fn get_emails_pending_action(&self, limit: usize) -> Result<Vec<EmailRecord>> {
        self.get_emails_pending_action_for_account(DEFAULT_ACCOUNT_ID, limit)
            .await
    }

    pub async fn get_emails_pending_action_for_account(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<EmailRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic, location_recommendation,
                   location, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND analyzed_at IS NOT NULL
              AND (
                  action_status IS NULL
                  OR action_status IN ('pending_trash', 'pending_junk')
              )
              AND deleted_from_server_at IS NULL
              AND (
                  phishing_status IS NOT NULL
                  OR spam_status IS NOT NULL
                  OR otp_status IS NOT NULL
                  OR threat_level IS NOT NULL
              )
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
            "#,
        )
        .bind(account_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_emails_pending_action")?;

        Ok(rows.iter().map(email_record_from_row).collect())
    }

    pub async fn set_action_status(
        &self,
        message_id: &str,
        action_status: Option<&str>,
    ) -> Result<()> {
        self.set_action_status_for_account(DEFAULT_ACCOUNT_ID, message_id, action_status)
            .await
    }

    pub async fn set_action_status_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        action_status: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET action_status = $3,
                action_applied_at = NULL,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(action_status)
        .execute(&self.pool)
        .await
        .context("set_action_status_for_account")?;
        Ok(())
    }

    pub async fn mark_email_action_applied(&self, message_id: &str, action: &str) -> Result<()> {
        self.mark_email_action_applied_for_account(DEFAULT_ACCOUNT_ID, message_id, action)
            .await
    }

    pub async fn mark_email_action_applied_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        action: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET action_status = $3,
                action_applied_at = NOW(),
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(action)
        .execute(&self.pool)
        .await
        .context("mark_email_action_applied")?;
        Ok(())
    }

    pub async fn store_otp_code(
        &self,
        message_id: &str,
        code: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.store_otp_code_for_account(DEFAULT_ACCOUNT_ID, message_id, code, expires_at)
            .await
    }

    pub async fn store_otp_code_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        code: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO otp_codes (account_id, message_id, code, expires_at)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(code)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .context("store_otp_code")?;
        Ok(())
    }

    /// Find emails eligible for lifecycle cleanup: expired OTPs (>1hr) and stale newsletters.
    pub async fn get_lifecycle_cleanup_candidates_for_account(
        &self,
        account_id: &str,
        otp_max_age_secs: i64,
        newsletter_max_age_days: i32,
        limit: usize,
    ) -> Result<Vec<EmailRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, received_date, spam_status, phishing_status, marketing_status,
                   otp_status, otp_code, otp_expires, threat_level, threat_indicators, uid, uid_validity,
                   ai_summary, human_summary, category, subcategory, organization, topic, location_recommendation,
                   location, offer_expires, related_message_ids, email_type,
                   COALESCE(is_read, false) as is_read, raw_email_content, body_text, body_synced_at,
                   message_size, message_tokens, analyzed_at, action_status, action_applied_at,
                   analysis_attempts, analysis_failed_at, analysis_permanent_failure, last_analysis_error,
                   created_at, updated_at
            FROM emails
            WHERE account_id = $1
              AND deleted_from_server_at IS NULL
              AND analyzed_at IS NOT NULL
              AND (
                  -- Expired OTPs: otp_status = 'otp' and older than max age
                  (otp_status = 'otp' AND (
                      (otp_expires IS NOT NULL AND otp_expires < NOW())
                      OR (otp_expires IS NULL AND analyzed_at < NOW() - make_interval(secs => $2))
                  ))
                  -- Stale newsletters: email_type = 'newsletter' and older than retention period
                  OR (email_type = 'newsletter' AND received_date < NOW() - make_interval(days => $3))
              )
              AND action_status NOT IN ('lifecycle_trashed')
            ORDER BY received_date ASC
            LIMIT $4
            "#,
        )
        .bind(account_id)
        .bind(otp_max_age_secs as f64)
        .bind(newsletter_max_age_days)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_lifecycle_cleanup_candidates")?;

        Ok(rows.iter().map(email_record_from_row).collect())
    }

    pub async fn record_analysis_attempt_failed(
        &self,
        message_id: &str,
        error: &str,
    ) -> Result<()> {
        self.record_analysis_attempt_failed_for_account(DEFAULT_ACCOUNT_ID, message_id, error)
            .await
    }

    pub async fn record_analysis_attempt_failed_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET analysis_attempts = analysis_attempts + 1,
                analysis_failed_at = NOW(),
                last_analysis_error = LEFT($3, 2000),
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(error)
        .execute(&self.pool)
        .await
        .context("record_analysis_attempt_failed")?;
        Ok(())
    }

    pub async fn mark_analysis_permanent_failure(&self, message_id: &str) -> Result<()> {
        self.mark_analysis_permanent_failure_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn mark_analysis_permanent_failure_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE emails
            SET analysis_permanent_failure = TRUE,
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("mark_analysis_permanent_failure")?;
        Ok(())
    }

    /// Update only location_recommendation and location_create_if_missing (for location agent result).
    pub async fn update_location_recommendation(
        &self,
        message_id: &str,
        location_recommendation: &str,
        location_create_if_missing: bool,
    ) -> Result<u64> {
        self.update_location_recommendation_for_account(
            DEFAULT_ACCOUNT_ID,
            message_id,
            location_recommendation,
            location_create_if_missing,
        )
        .await
    }

    pub async fn update_location_recommendation_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        location_recommendation: &str,
        location_create_if_missing: bool,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET location_recommendation = $3, location_create_if_missing = $4, updated_at = NOW() WHERE account_id = $1 AND message_id = $2",
        )
        .bind(account_id)
        .bind(message_id)
        .bind(location_recommendation)
        .bind(location_create_if_missing)
        .execute(&self.pool)
        .await
        .context("update_location_recommendation")?;
        Ok(result.rows_affected())
    }

    /// True if the folder path exists and is selectable, and no parent segment is NOSELECT.
    pub async fn is_path_selectable(&self, path: &str) -> Result<bool> {
        let folders = self.list_imap_folders().await?;
        let delimiter = folders
            .first()
            .and_then(|f| f.delimiter.as_deref())
            .unwrap_or("/");
        let segments: Vec<&str> = path.split(delimiter).filter(|s| !s.is_empty()).collect();
        let mut prefix = String::new();
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                prefix.push_str(delimiter);
            }
            prefix.push_str(seg);
            if let Some(f) = folders.iter().find(|f| f.folder_name == prefix) {
                if f.is_noselect {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Emails with location_recommendation set and not yet in that folder (for file / apply-location).
    pub async fn get_emails_with_pending_location(&self) -> Result<Vec<PendingLocationApply>> {
        self.get_emails_with_pending_location_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn get_emails_with_pending_location_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<PendingLocationApply>> {
        let rows = sqlx::query(
            r#"
            SELECT message_id, location, location_recommendation, location_create_if_missing, uid, uid_validity
            FROM emails
            WHERE account_id = $1
              AND location IS NOT NULL
              AND deleted_from_server_at IS NULL
              AND location_recommendation IS NOT NULL
              AND COALESCE(action_status, '') NOT IN ('trashed', 'junked')
              AND COALESCE(spam_status, '') <> 'spam'
              AND COALESCE(phishing_status, '') <> 'phishing'
              AND COALESCE(threat_level, '') NOT IN ('high', 'critical')
              AND user_pinned_folder IS NULL
              AND (filing_lock_until IS NULL OR filing_lock_until <= NOW())
              AND LOWER(TRIM(location_recommendation)) <> LOWER(TRIM(location))
            ORDER BY received_date DESC NULLS LAST
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .context("get_emails_with_pending_location")?;
        Ok(rows
            .into_iter()
            .map(|r| PendingLocationApply {
                message_id: r.get("message_id"),
                location: r.get("location"),
                location_recommendation: r.get("location_recommendation"),
                location_create_if_missing: r.get("location_create_if_missing"),
                uid: r.get("uid"),
                uid_validity: r.get("uid_validity"),
            })
            .collect())
    }

    pub async fn delete_email(&self, message_id: &str) -> Result<u64> {
        self.delete_email_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn delete_email_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<u64> {
        let result = sqlx::query("DELETE FROM emails WHERE account_id = $1 AND message_id = $2")
            .bind(account_id)
            .bind(message_id)
            .execute(&self.pool)
            .await
            .context("Failed to delete email")?;
        Ok(result.rows_affected())
    }

    pub async fn prune_emails(
        &self,
        category: Option<&str>,
        subcategory: Option<&str>,
    ) -> Result<u64> {
        self.prune_emails_for_account(DEFAULT_ACCOUNT_ID, category, subcategory)
            .await
    }

    pub async fn prune_emails_for_account(
        &self,
        account_id: &str,
        category: Option<&str>,
        subcategory: Option<&str>,
    ) -> Result<u64> {
        let mut query = String::from("DELETE FROM emails WHERE account_id = $1");
        let mut count = 0u8;
        if category.is_some() {
            count += 1;
            query.push_str(&format!(" AND category = ${}", count + 1));
        }
        if subcategory.is_some() {
            count += 1;
            query.push_str(&format!(" AND subcategory = ${}", count + 1));
        }
        let mut q = sqlx::query(&query).bind(account_id);
        if let Some(c) = category {
            q = q.bind(c);
        }
        if let Some(s) = subcategory {
            q = q.bind(s);
        }
        let result = q
            .execute(&self.pool)
            .await
            .context("Failed to prune emails")?;
        Ok(result.rows_affected())
    }
}
