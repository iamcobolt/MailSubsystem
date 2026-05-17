use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder, Row};
use std::collections::HashMap;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::email_record_from_row;
use crate::db::{Database, EmailRecord};

#[derive(Debug, Clone)]
pub struct EmailListPage {
    pub emails: Vec<EmailRecord>,
    pub total_count: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EmailListFilters<'a> {
    pub category: Option<&'a str>,
    pub email_type: Option<&'a str>,
    pub spam_status: Option<&'a str>,
    pub search: Option<&'a str>,
    pub folder: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub since: Option<DateTime<Utc>>,
}

/// Aggregate digest statistics for a time window.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DigestWindowStats {
    pub total_received: i64,
    pub by_category: HashMap<String, i64>,
    pub by_email_type: HashMap<String, i64>,
    pub spam_count: i64,
    pub phishing_count: i64,
    pub marketing_count: i64,
    pub otp_count: i64,
    pub threats_detected: i64,
    pub filed_count: i64,
    pub trashed_count: i64,
    pub junked_count: i64,
}

impl Database {
    /// Return total row count in emails table.
    pub async fn count_emails(&self) -> Result<i64> {
        self.count_emails_for_account(DEFAULT_ACCOUNT_ID).await
    }

    pub async fn count_emails_for_account(&self, account_id: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM emails WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&self.pool)
            .await
            .context("Failed to count emails")?;
        Ok(count)
    }

    pub async fn get_email_stats_for_window(
        &self,
        since: DateTime<Utc>,
    ) -> Result<DigestWindowStats> {
        self.get_email_stats_for_window_for_account(DEFAULT_ACCOUNT_ID, since)
            .await
    }

    pub async fn get_email_stats_for_window_for_account(
        &self,
        account_id: &str,
        since: DateTime<Utc>,
    ) -> Result<DigestWindowStats> {
        let row = sqlx::query(
            r#"
            SELECT
              COUNT(*)::bigint AS total_received,
              COUNT(*) FILTER (WHERE spam_status = 'spam')::bigint AS spam_count,
              COUNT(*) FILTER (WHERE phishing_status = 'phishing')::bigint AS phishing_count,
              COUNT(*) FILTER (WHERE marketing_status = 'marketing')::bigint AS marketing_count,
              COUNT(*) FILTER (WHERE otp_status = 'otp')::bigint AS otp_count,
              COUNT(*) FILTER (WHERE threat_level IN ('high', 'critical'))::bigint AS threats_detected,
              COUNT(*) FILTER (WHERE action_status = 'trashed')::bigint AS trashed_count,
              COUNT(*) FILTER (WHERE action_status = 'junked')::bigint AS junked_count,
              COUNT(*) FILTER (
                WHERE action_status IS NOT NULL AND action_status NOT IN ('trashed', 'junked')
              )::bigint AS filed_count
            FROM emails
            WHERE account_id = $1
              AND received_date >= $2
              AND deleted_from_server_at IS NULL
            "#,
        )
        .bind(account_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .context("get_email_stats_for_window aggregate")?;

        let category_rows = sqlx::query(
            r#"
            SELECT category, COUNT(*)::bigint AS cnt
            FROM emails
            WHERE account_id = $1
              AND received_date >= $2
              AND deleted_from_server_at IS NULL
              AND category IS NOT NULL
            GROUP BY category
            "#,
        )
        .bind(account_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .context("get_email_stats_for_window by_category")?;
        let by_category = category_rows
            .into_iter()
            .map(|r| (r.get::<String, _>("category"), r.get::<i64, _>("cnt")))
            .collect();

        let email_type_rows = sqlx::query(
            r#"
            SELECT email_type, COUNT(*)::bigint AS cnt
            FROM emails
            WHERE account_id = $1
              AND received_date >= $2
              AND deleted_from_server_at IS NULL
              AND email_type IS NOT NULL
            GROUP BY email_type
            "#,
        )
        .bind(account_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .context("get_email_stats_for_window by_email_type")?;
        let by_email_type = email_type_rows
            .into_iter()
            .map(|r| (r.get::<String, _>("email_type"), r.get::<i64, _>("cnt")))
            .collect();

        Ok(DigestWindowStats {
            total_received: row.get("total_received"),
            by_category,
            by_email_type,
            spam_count: row.get("spam_count"),
            phishing_count: row.get("phishing_count"),
            marketing_count: row.get("marketing_count"),
            otp_count: row.get("otp_count"),
            threats_detected: row.get("threats_detected"),
            filed_count: row.get("filed_count"),
            trashed_count: row.get("trashed_count"),
            junked_count: row.get("junked_count"),
        })
    }

    pub async fn get_top_senders_for_window(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<(String, i64)>> {
        self.get_top_senders_for_window_for_account(DEFAULT_ACCOUNT_ID, since, limit)
            .await
    }

    pub async fn get_top_senders_for_window_for_account(
        &self,
        account_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<(String, i64)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT sender, COUNT(*)::bigint AS cnt
            FROM emails
            WHERE account_id = $1
              AND received_date >= $2
              AND deleted_from_server_at IS NULL
              AND sender IS NOT NULL
            GROUP BY sender
            ORDER BY cnt DESC
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(since)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_top_senders_for_window")?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("sender"), row.get("cnt")))
            .collect())
    }

    pub async fn get_escalation_count_for_window(&self, since: DateTime<Utc>) -> Result<i64> {
        self.get_escalation_count_for_window_for_account(DEFAULT_ACCOUNT_ID, since)
            .await
    }

    pub async fn get_escalation_count_for_window_for_account(
        &self,
        account_id: &str,
        since: DateTime<Utc>,
    ) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_runs WHERE account_id = $1 AND created_at >= $2 AND escalated = true",
        )
        .bind(account_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .context("get_escalation_count_for_window")?;
        Ok(count)
    }

    pub async fn get_email_by_message_id(&self, message_id: &str) -> Result<Option<EmailRecord>> {
        self.get_email_by_message_id_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn get_email_by_message_id_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<Option<EmailRecord>> {
        let row = sqlx::query(
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
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query email")?;

        Ok(row.as_ref().map(email_record_from_row))
    }

    pub async fn get_active_email_by_message_id_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<Option<EmailRecord>> {
        let row = sqlx::query(
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
              AND message_id = $2
              AND deleted_from_server_at IS NULL
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query active email")?;

        Ok(row.as_ref().map(email_record_from_row))
    }

    pub async fn list_emails(
        &self,
        filters: EmailListFilters<'_>,
        limit: usize,
        offset: usize,
    ) -> Result<EmailListPage> {
        self.list_emails_for_account(DEFAULT_ACCOUNT_ID, filters, limit, offset)
            .await
    }

    pub async fn list_emails_for_account(
        &self,
        account_id: &str,
        filters: EmailListFilters<'_>,
        limit: usize,
        offset: usize,
    ) -> Result<EmailListPage> {
        if limit == 0 {
            return Ok(EmailListPage {
                emails: Vec::new(),
                total_count: 0,
            });
        }

        let category = filters
            .category
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let email_type = filters
            .email_type
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let spam_status = filters
            .spam_status
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let folder = filters
            .folder
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let sender = filters
            .sender
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{}%", value));
        let organization = filters
            .organization
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{}%", value));
        let search = filters
            .search
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{}%", value));

        let mut count_query = QueryBuilder::<Postgres>::new(
            "SELECT COUNT(*)::bigint AS total_count FROM emails WHERE account_id = ",
        );
        count_query.push_bind(account_id);
        count_query.push(" AND deleted_from_server_at IS NULL");

        if let Some(category) = category {
            count_query.push(" AND category = ");
            count_query.push_bind(category);
        }
        if let Some(email_type) = email_type {
            count_query.push(" AND email_type = ");
            count_query.push_bind(email_type);
        }
        if let Some(spam_status) = spam_status {
            count_query.push(" AND spam_status = ");
            count_query.push_bind(spam_status);
        }
        if let Some(folder) = folder {
            count_query.push(" AND location = ");
            count_query.push_bind(folder);
        }
        if let Some(sender) = sender.as_deref() {
            count_query.push(" AND sender ILIKE ");
            count_query.push_bind(sender);
        }
        if let Some(organization) = organization.as_deref() {
            count_query.push(" AND organization ILIKE ");
            count_query.push_bind(organization);
        }
        if let Some(since) = filters.since {
            count_query.push(" AND received_date >= ");
            count_query.push_bind(since);
        }
        if let Some(search) = search.as_deref() {
            count_query.push(" AND (message_id ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR subject ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR sender ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR human_summary ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR organization ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR topic ILIKE ");
            count_query.push_bind(search);
            count_query.push(" OR body_text ILIKE ");
            count_query.push_bind(search);
            count_query.push(")");
        }

        let total_count = count_query
            .build()
            .fetch_one(&self.pool)
            .await
            .context("count_emails_for_account")?
            .get::<i64, _>("total_count");

        if total_count == 0 || offset >= total_count as usize {
            return Ok(EmailListPage {
                emails: Vec::new(),
                total_count,
            });
        }

        let mut query = QueryBuilder::<Postgres>::new(
            r#"
            SELECT
                message_id,
                subject,
                sender,
                received_date,
                spam_status,
                phishing_status,
                marketing_status,
                otp_status,
                otp_code,
                otp_expires,
                threat_level,
                threat_indicators,
                uid,
                uid_validity,
                ai_summary,
                human_summary,
                category,
                subcategory,
                organization,
                topic,
                location_recommendation,
                location,
                offer_expires,
                related_message_ids,
                email_type,
                COALESCE(is_read, false) AS is_read,
                NULL::text AS raw_email_content,
                NULL::text AS body_text,
                body_synced_at,
                message_size,
                message_tokens,
                analyzed_at,
                action_status,
                action_applied_at,
                analysis_attempts,
                analysis_failed_at,
                analysis_permanent_failure,
                last_analysis_error,
                created_at,
                updated_at
            FROM emails
            WHERE account_id =
            "#,
        );
        query.push_bind(account_id);
        query.push(" AND deleted_from_server_at IS NULL");

        if let Some(category) = category {
            query.push(" AND category = ");
            query.push_bind(category);
        }
        if let Some(email_type) = email_type {
            query.push(" AND email_type = ");
            query.push_bind(email_type);
        }
        if let Some(spam_status) = spam_status {
            query.push(" AND spam_status = ");
            query.push_bind(spam_status);
        }
        if let Some(folder) = folder {
            // The API uses 'folder' terminology; emails persist the current IMAP path in
            // the 'location' column.
            query.push(" AND location = ");
            query.push_bind(folder);
        }
        if let Some(sender) = sender.as_deref() {
            query.push(" AND sender ILIKE ");
            query.push_bind(sender);
        }
        if let Some(organization) = organization.as_deref() {
            query.push(" AND organization ILIKE ");
            query.push_bind(organization);
        }
        if let Some(since) = filters.since {
            query.push(" AND received_date >= ");
            query.push_bind(since);
        }
        if let Some(search) = search.as_deref() {
            query.push(" AND (message_id ILIKE ");
            query.push_bind(search);
            query.push(" OR subject ILIKE ");
            query.push_bind(search);
            query.push(" OR sender ILIKE ");
            query.push_bind(search);
            query.push(" OR human_summary ILIKE ");
            query.push_bind(search);
            query.push(" OR organization ILIKE ");
            query.push_bind(search);
            query.push(" OR topic ILIKE ");
            query.push_bind(search);
            query.push(" OR body_text ILIKE ");
            query.push_bind(search);
            query.push(")");
        }

        query.push(" ORDER BY received_date DESC NULLS LAST, created_at DESC, message_id DESC");
        query.push(" LIMIT ");
        query.push_bind(limit as i64);
        query.push(" OFFSET ");
        query.push_bind(offset as i64);

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .context("list_emails_for_account")?;

        Ok(EmailListPage {
            emails: rows.iter().map(email_record_from_row).collect(),
            total_count,
        })
    }
}
