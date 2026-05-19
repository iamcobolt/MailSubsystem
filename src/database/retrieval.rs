use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mailparse::{parse_mail, ParsedMail};
use pgvector::Vector;
use serde_json::Value;
use sqlx::{types::Json, Postgres, Row, Transaction};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::Database;
use crate::embeddings::clean_email_body_text;

const EMBEDDING_MODEL_LOCK_KEY: i64 = 0x4d53_5345_4d42_4544;

pub struct EmbeddingModelLock<'a> {
    _tx: Transaction<'a, Postgres>,
}

/// Raw content of a thread/related message for RAG (subject, sender, body).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadMessageRaw {
    pub message_id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub body_text: Option<String>,
    pub received_date: Option<DateTime<Utc>>,
}

/// Result of semantic search (RAG search_similar_emails).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SimilarEmailResult {
    pub message_id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub human_summary: Option<String>,
    pub organization: Option<String>,
    pub list_id: Option<String>,
    pub spam_status: Option<String>,
    pub marketing_status: Option<String>,
    pub email_type: Option<String>,
    pub category: Option<String>,
    pub topic: Option<String>,
    /// Cosine distance (lower = more similar).
    pub distance: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SimilarEmailSearchHints<'a> {
    pub sender: Option<&'a str>,
    pub category: Option<&'a str>,
    pub email_type: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub list_id: Option<&'a str>,
    pub exclude_message_id: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct SimilarEmailVectorQuery<'a> {
    pub embedding: &'a [f32],
    pub limit: usize,
    pub max_distance: f64,
    pub hints: SimilarEmailSearchHints<'a>,
}

impl<'a> SimilarEmailVectorQuery<'a> {
    pub fn with_threshold(embedding: &'a [f32], limit: usize, max_distance: f64) -> Self {
        Self {
            embedding,
            limit,
            max_distance,
            hints: SimilarEmailSearchHints::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SimilarEmailKeywordQuery<'a> {
    pub query_text: &'a str,
    pub limit: usize,
    pub hints: SimilarEmailSearchHints<'a>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingCoverageStats {
    pub total_with_text: i64,
    pub with_embedding: i64,
    pub without_embedding: i64,
}

/// Aggregated sender behavior prior for intent classification.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SenderIntentProfile {
    pub sender: String,
    pub sample_size: i64,
    pub spam_count: i64,
    pub not_spam_count: i64,
    pub marketing_count: i64,
    pub not_marketing_count: i64,
    pub newsletter_count: i64,
    pub notification_count: i64,
    pub actionable_count: i64,
    pub informative_count: i64,
    pub spam_rate: f64,
    pub not_spam_rate: f64,
    pub marketing_rate: f64,
    pub informative_rate: f64,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

impl Database {
    /// RAG: get ai_summary for related messages (thread context).
    pub async fn get_thread_summaries(
        &self,
        message_ids: &[String],
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.get_thread_summaries_for_account(DEFAULT_ACCOUNT_ID, message_ids, limit)
            .await
    }

    /// RAG: get ai_summary for related messages (thread context).
    pub async fn get_thread_summaries_for_account(
        &self,
        account_id: &str,
        message_ids: &[String],
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT ai_summary FROM emails WHERE account_id = $1 AND message_id = ANY($2) AND ai_summary IS NOT NULL ORDER BY received_date ASC LIMIT $3",
        )
        .bind(account_id)
        .bind(message_ids)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_thread_summaries")?;
        let out: Vec<serde_json::Value> = rows
            .iter()
            .filter_map(|r| r.get::<Option<Json<Value>>, _>("ai_summary"))
            .map(|j| j.0)
            .collect();
        Ok(out)
    }

    /// RAG: get raw content (subject, sender, body_text) for related/thread messages so the model can use full content.
    pub async fn get_thread_raw_content(
        &self,
        message_ids: &[String],
        limit: usize,
    ) -> Result<Vec<ThreadMessageRaw>> {
        self.get_thread_raw_content_for_account(DEFAULT_ACCOUNT_ID, message_ids, limit)
            .await
    }

    /// RAG: get raw content (subject, sender, body_text) for related/thread messages so the model can use full content.
    pub async fn get_thread_raw_content_for_account(
        &self,
        account_id: &str,
        message_ids: &[String],
        limit: usize,
    ) -> Result<Vec<ThreadMessageRaw>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT message_id, subject, sender, body_text, received_date FROM emails WHERE account_id = $1 AND message_id = ANY($2) ORDER BY received_date ASC LIMIT $3",
        )
        .bind(account_id)
        .bind(message_ids)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_thread_raw_content")?;
        let out: Vec<ThreadMessageRaw> = rows
            .into_iter()
            .map(|r| ThreadMessageRaw {
                message_id: r.get("message_id"),
                subject: r.get("subject"),
                sender: r.get("sender"),
                body_text: r.get("body_text"),
                received_date: r.get("received_date"),
            })
            .collect();
        Ok(out)
    }

    /// RAG: get sender history (recent ai_summary and metadata).
    pub async fn get_sender_history(
        &self,
        sender: Option<&str>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.get_sender_history_for_account_excluding(DEFAULT_ACCOUNT_ID, sender, limit, None)
            .await
    }

    /// RAG: get sender history (recent synced messages, prior analysis, and excerpts).
    pub async fn get_sender_history_for_account(
        &self,
        account_id: &str,
        sender: Option<&str>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.get_sender_history_for_account_excluding(account_id, sender, limit, None)
            .await
    }

    /// RAG: get sender history, optionally excluding the email currently being analyzed.
    pub async fn get_sender_history_for_account_excluding(
        &self,
        account_id: &str,
        sender: Option<&str>,
        limit: usize,
        exclude_message_id: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let sender = match sender {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(Vec::new()),
        };
        let rows = sqlx::query(
            r#"
            SELECT jsonb_build_object(
                'message_id', message_id,
                'subject', subject,
                'received_date', received_date,
                'analysis_status', CASE WHEN ai_summary IS NULL THEN 'pending' ELSE 'analyzed' END,
                'spam_status', spam_status,
                'phishing_status', phishing_status,
                'marketing_status', marketing_status,
                'category', category,
                'subcategory', subcategory,
                'organization', organization,
                'topic', topic,
                'email_type', email_type,
                'location', location,
                'human_summary', human_summary,
                'ai_summary', ai_summary,
                'body_excerpt', NULLIF(
                    left(regexp_replace(coalesce(body_text, raw_email_content, ''), '[[:space:]]+', ' ', 'g'), 700),
                    ''
                )
            ) AS history
            FROM emails
            WHERE account_id = $1
              AND sender = $2
              AND ($4::text IS NULL OR message_id <> $4)
            ORDER BY received_date DESC
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(sender)
        .bind(limit.max(1) as i64)
        .bind(exclude_message_id)
        .fetch_all(&self.pool)
        .await
        .context("get_sender_history")?;
        let out: Vec<serde_json::Value> = rows
            .iter()
            .filter_map(|r| r.get::<Option<Json<Value>>, _>("history"))
            .map(|j| j.0)
            .collect();
        Ok(out)
    }

    /// RAG: sender-level intent profile over recent analyzed emails.
    pub async fn get_sender_intent_profile(
        &self,
        sender: Option<&str>,
        limit: usize,
    ) -> Result<Option<SenderIntentProfile>> {
        self.get_sender_intent_profile_for_account(DEFAULT_ACCOUNT_ID, sender, limit)
            .await
    }

    /// RAG: sender-level intent profile over recent analyzed emails.
    pub async fn get_sender_intent_profile_for_account(
        &self,
        account_id: &str,
        sender: Option<&str>,
        limit: usize,
    ) -> Result<Option<SenderIntentProfile>> {
        let sender = match sender {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return Ok(None),
        };
        let row = sqlx::query(
            r#"
            WITH recent AS (
                SELECT spam_status, marketing_status, email_type, category, received_date
                FROM emails
                WHERE account_id = $1
                  AND sender = $2
                  AND ai_summary IS NOT NULL
                ORDER BY received_date DESC
                LIMIT $3
            )
            SELECT
                COUNT(*)::bigint AS sample_size,
                COUNT(*) FILTER (WHERE spam_status = 'spam')::bigint AS spam_count,
                COUNT(*) FILTER (WHERE spam_status = 'not-spam')::bigint AS not_spam_count,
                COUNT(*) FILTER (WHERE marketing_status = 'marketing')::bigint AS marketing_count,
                COUNT(*) FILTER (WHERE marketing_status = 'not-marketing')::bigint AS not_marketing_count,
                COUNT(*) FILTER (WHERE email_type = 'newsletter')::bigint AS newsletter_count,
                COUNT(*) FILTER (WHERE email_type = 'notification')::bigint AS notification_count,
                COUNT(*) FILTER (WHERE email_type = 'actionable')::bigint AS actionable_count,
                COUNT(*) FILTER (WHERE category IN ('financial', 'education', 'work'))::bigint AS informative_count,
                MIN(received_date) AS first_seen,
                MAX(received_date) AS last_seen
            FROM recent
            "#,
        )
        .bind(account_id)
        .bind(&sender)
        .bind(limit.max(1) as i64)
        .fetch_one(&self.pool)
        .await
        .context("get_sender_intent_profile")?;

        let sample_size: i64 = row.get("sample_size");
        if sample_size == 0 {
            return Ok(None);
        }
        let spam_count: i64 = row.get("spam_count");
        let not_spam_count: i64 = row.get("not_spam_count");
        let marketing_count: i64 = row.get("marketing_count");
        let not_marketing_count: i64 = row.get("not_marketing_count");
        let newsletter_count: i64 = row.get("newsletter_count");
        let notification_count: i64 = row.get("notification_count");
        let actionable_count: i64 = row.get("actionable_count");
        let informative_count: i64 = row.get("informative_count");
        let denom = sample_size as f64;

        Ok(Some(SenderIntentProfile {
            sender,
            sample_size,
            spam_count,
            not_spam_count,
            marketing_count,
            not_marketing_count,
            newsletter_count,
            notification_count,
            actionable_count,
            informative_count,
            spam_rate: spam_count as f64 / denom,
            not_spam_rate: not_spam_count as f64 / denom,
            marketing_rate: marketing_count as f64 / denom,
            informative_rate: informative_count as f64 / denom,
            first_seen: row.get("first_seen"),
            last_seen: row.get("last_seen"),
        }))
    }

    /// RAG: list distinct folder names (location) for location_recommendation matching.
    pub async fn get_existing_folders(&self) -> Result<Vec<String>> {
        self.get_existing_folders_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    /// RAG: list distinct folder names (location) for location_recommendation matching.
    pub async fn get_existing_folders_for_account(&self, account_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT location FROM emails WHERE account_id = $1 AND location IS NOT NULL ORDER BY location",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .context("get_existing_folders")?;
        Ok(rows
            .iter()
            .map(|r| r.get::<String, _>("location"))
            .collect())
    }

    /// Ensure pgvector extension exists (idempotent).
    pub async fn ensure_vector_extension(&self) -> Result<()> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector')",
        )
        .fetch_one(&self.pool)
        .await
        .context("check vector extension")?;
        if !exists {
            sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
                .execute(&self.pool)
                .await
                .context("ensure vector extension")?;
        }
        Ok(())
    }

    /// Ensure indexes used by hybrid RAG retrieval/backfill exist (idempotent).
    pub async fn ensure_hybrid_retrieval_indexes(&self) -> Result<()> {
        if !self
            .relation_exists("public.idx_emails_rag_hybrid_fts_gin")
            .await?
        {
            sqlx::query(
                r#"
            CREATE INDEX IF NOT EXISTS idx_emails_rag_hybrid_fts_gin
            ON emails
            USING GIN ((
              setweight(
                to_tsvector(
                  'english',
                  regexp_replace(COALESCE(subject, ''), '[^[:space:]]{128,}', ' ', 'g')
                ),
                'A'
              ) ||
              setweight(
                to_tsvector(
                  'english',
                  regexp_replace(COALESCE(sender, ''), '[^[:space:]]{128,}', ' ', 'g')
                ),
                'A'
              ) ||
              setweight(
                to_tsvector(
                  'english',
                  regexp_replace(COALESCE(human_summary, ''), '[^[:space:]]{128,}', ' ', 'g')
                ),
                'B'
              ) ||
              setweight(
                to_tsvector(
                  'english',
                  regexp_replace(
                    COALESCE(category, '') || ' ' ||
                    COALESCE(subcategory, '') || ' ' ||
                    COALESCE(organization, '') || ' ' ||
                    COALESCE(topic, '') || ' ' ||
                    COALESCE(email_type, '') || ' ' ||
                    COALESCE(list_id, ''),
                    '[^[:space:]]{128,}',
                    ' ',
                    'g'
                  )
                ),
                'B'
              ) ||
              setweight(
                to_tsvector(
                  'english',
                  LEFT(
                    regexp_replace(
                      COALESCE(
                        NULLIF(TRIM(COALESCE(body_text, '')), ''),
                        regexp_replace(
                          LEFT(COALESCE(raw_email_content, ''), 12000),
                          '[^[:alnum:]@._-]+',
                          ' ',
                          'g'
                        ),
                        ''
                      ),
                      '[^[:space:]]{128,}',
                      ' ',
                      'g'
                    ),
                    2000
                  )
                ),
                'C'
              )
            ))
            WHERE ai_summary IS NOT NULL
            "#,
            )
            .execute(&self.pool)
            .await
            .context("ensure idx_emails_rag_hybrid_fts_gin")?;
        }

        if !self
            .relation_exists("public.idx_emails_embedding_backfill_received_date")
            .await?
        {
            sqlx::query(
                r#"
            CREATE INDEX IF NOT EXISTS idx_emails_embedding_backfill_received_date
            ON emails (received_date DESC)
            WHERE embedding IS NULL
              AND (
                (body_text IS NOT NULL AND TRIM(body_text) <> '')
                OR (raw_email_content IS NOT NULL AND LENGTH(raw_email_content) > 100)
              )
            "#,
            )
            .execute(&self.pool)
            .await
            .context("ensure idx_emails_embedding_backfill_received_date")?;
        }
        Ok(())
    }

    /// Update embedding for an email by message_id.
    pub async fn update_embedding(&self, message_id: &str, embedding: &[f32]) -> Result<()> {
        self.update_embedding_for_account(DEFAULT_ACCOUNT_ID, message_id, embedding)
            .await
    }

    /// Update embedding for an email by message_id.
    pub async fn update_embedding_for_account(
        &self,
        account_id: &str,
        message_id: &str,
        embedding: &[f32],
    ) -> Result<()> {
        let v = Vector::from(embedding.to_vec());
        sqlx::query(
            "UPDATE emails SET embedding = $1, updated_at = NOW() WHERE account_id = $2 AND message_id = $3",
        )
            .bind(v)
            .bind(account_id)
            .bind(message_id)
            .execute(&self.pool)
            .await
            .context("update_embedding")?;
        Ok(())
    }
}

impl Database {
    /// Acquire a process-cooperative database lock for embedding model metadata
    /// and vector index rebuilds.
    pub async fn acquire_embedding_model_lock(&self) -> Result<EmbeddingModelLock<'_>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin embedding model lock transaction")?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(EMBEDDING_MODEL_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .context("acquire embedding model advisory lock")?;
        Ok(EmbeddingModelLock { _tx: tx })
    }

    /// Acquire a shared lock while writing embeddings for the currently
    /// configured model. Exclusive model resets wait for this lock.
    pub async fn acquire_embedding_model_shared_lock(&self) -> Result<EmbeddingModelLock<'_>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin embedding model shared lock transaction")?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
            .bind(EMBEDDING_MODEL_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .context("acquire embedding model shared advisory lock")?;
        Ok(EmbeddingModelLock { _tx: tx })
    }

    /// Null all embeddings across accounts. Returns the number of rows affected.
    pub async fn null_all_embeddings(&self) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET embedding = NULL, updated_at = NOW() \
             WHERE embedding IS NOT NULL",
        )
        .execute(&self.pool)
        .await
        .context("null_all_embeddings")?;
        Ok(result.rows_affected())
    }

    /// Null all embeddings for an account. Returns the number of rows affected.
    pub async fn null_all_embeddings_for_account(&self, account_id: &str) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE emails SET embedding = NULL, updated_at = NOW() \
             WHERE account_id = $1 AND embedding IS NOT NULL",
        )
        .bind(account_id)
        .execute(&self.pool)
        .await
        .context("null_all_embeddings")?;
        Ok(result.rows_affected())
    }

    /// Drop and recreate the HNSW embedding index and alter the column type
    /// to match new dimensions. Call after nulling embeddings when switching models.
    pub async fn rebuild_embedding_index(&self, dimensions: usize) -> Result<()> {
        // Drop indexes that reference the embedding column
        sqlx::query("DROP INDEX IF EXISTS idx_emails_embedding_cosine")
            .execute(&self.pool)
            .await
            .context("drop HNSW index")?;
        sqlx::query("DROP INDEX IF EXISTS idx_emails_embedding_backfill_received_date")
            .execute(&self.pool)
            .await
            .context("drop backfill index")?;

        // Alter column to new dimension (safe because all embeddings were nulled)
        let alter = format!(
            "ALTER TABLE emails ALTER COLUMN embedding TYPE vector({})",
            dimensions
        );
        sqlx::query(&alter)
            .execute(&self.pool)
            .await
            .context("alter embedding column dimensions")?;

        // Recreate indexes
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_emails_embedding_cosine \
             ON emails USING hnsw (embedding vector_cosine_ops) \
             WHERE embedding IS NOT NULL",
        )
        .execute(&self.pool)
        .await
        .context("recreate HNSW index")?;

        if let Err(err) = self.ensure_hybrid_retrieval_indexes().await {
            log::warn!("Failed to recreate hybrid retrieval indexes: {}", err);
        }

        Ok(())
    }

    /// Embedding coverage over rows that have usable text content.
    pub async fn get_embedding_coverage_stats(&self) -> Result<EmbeddingCoverageStats> {
        self.get_embedding_coverage_stats_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    /// Embedding coverage over rows that have usable text content.
    pub async fn get_embedding_coverage_stats_for_account(
        &self,
        account_id: &str,
    ) -> Result<EmbeddingCoverageStats> {
        let row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (
                WHERE (body_text IS NOT NULL AND TRIM(body_text) <> '')
                   OR (raw_email_content IS NOT NULL AND LENGTH(raw_email_content) > 100)
              )::bigint AS total_with_text,
              COUNT(*) FILTER (
                WHERE embedding IS NOT NULL
                  AND (
                    (body_text IS NOT NULL AND TRIM(body_text) <> '')
                    OR (raw_email_content IS NOT NULL AND LENGTH(raw_email_content) > 100)
                  )
              )::bigint AS with_embedding
            FROM emails
            WHERE account_id = $1
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("get_embedding_coverage_stats")?;
        let total_with_text: i64 = row.get("total_with_text");
        let with_embedding: i64 = row.get("with_embedding");
        Ok(EmbeddingCoverageStats {
            total_with_text,
            with_embedding,
            without_embedding: total_with_text.saturating_sub(with_embedding),
        })
    }

    /// Emails that have body_text or raw_email_content but no embedding yet (for backfill).
    /// Returns (message_id, canonical_text_to_embed).
    pub async fn get_emails_needing_embedding(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        self.get_emails_needing_embedding_for_account(DEFAULT_ACCOUNT_ID, limit)
            .await
    }

    /// Emails that have body_text or raw_email_content but no embedding yet (for backfill).
    /// Returns (message_id, canonical_text_to_embed).
    pub async fn get_emails_needing_embedding_for_account(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let select_sql = r#"
            SELECT message_id, subject, sender, human_summary,
                   category, subcategory, organization, topic, email_type, otp_status,
                   list_id, list_unsubscribe, x_priority, return_path, reply_to,
                   body_text, raw_email_content
            FROM emails
            WHERE account_id = $1
              AND embedding IS NULL
              AND (
                (body_text IS NOT NULL AND TRIM(body_text) != '')
                OR (raw_email_content IS NOT NULL AND LENGTH(raw_email_content) > 100)
              )
            ORDER BY received_date DESC NULLS LAST
            LIMIT $2
        "#;

        // Prefer index-backed planning for backfill candidate selection. This is scoped to
        // this transaction only; if anything fails, we fall back to the normal query path.
        let rows = match self.pool.begin().await {
            Ok(mut tx) => {
                if let Err(err) = sqlx::query("SET LOCAL enable_seqscan = off")
                    .execute(&mut *tx)
                    .await
                {
                    log::warn!(
                        "get_emails_needing_embedding: could not set LOCAL planner hint: {}",
                        err
                    );
                }
                match sqlx::query(select_sql)
                    .bind(account_id)
                    .bind(limit as i64)
                    .fetch_all(&mut *tx)
                    .await
                {
                    Ok(rows) => {
                        if let Err(err) = tx.commit().await {
                            log::warn!(
                                "get_emails_needing_embedding: transaction commit warning: {}",
                                err
                            );
                        }
                        rows
                    }
                    Err(err) => {
                        let _ = tx.rollback().await;
                        log::warn!(
                            "get_emails_needing_embedding: tuned query failed, using fallback: {}",
                            err
                        );
                        sqlx::query(select_sql)
                            .bind(account_id)
                            .bind(limit as i64)
                            .fetch_all(&self.pool)
                            .await
                            .context("get_emails_needing_embedding")?
                    }
                }
            }
            Err(err) => {
                log::warn!(
                    "get_emails_needing_embedding: transaction start failed, using fallback: {}",
                    err
                );
                sqlx::query(select_sql)
                    .bind(account_id)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await
                    .context("get_emails_needing_embedding")?
            }
        };

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let message_id: String = r.get("message_id");
            let subject: Option<String> = r.get("subject");
            let sender: Option<String> = r.get("sender");
            let human_summary: Option<String> = r.get("human_summary");
            let category: Option<String> = r.get("category");
            let subcategory: Option<String> = r.get("subcategory");
            let organization: Option<String> = r.get("organization");
            let topic: Option<String> = r.get("topic");
            let email_type: Option<String> = r.get("email_type");
            let otp_status: Option<String> = r.get("otp_status");
            let list_id: Option<String> = r.get("list_id");
            let list_unsubscribe: Option<String> = r.get("list_unsubscribe");
            let x_priority: Option<String> = r.get("x_priority");
            let return_path: Option<String> = r.get("return_path");
            let reply_to: Option<String> = r.get("reply_to");
            let body_text: Option<String> = r.get("body_text");
            let raw_email_content: Option<String> = r.get("raw_email_content");

            let cleaned_body =
                clean_body_for_embedding(body_text.as_deref(), raw_email_content.as_deref());
            let text = build_canonical_embedding_text(&CanonicalEmbeddingTextInput {
                subject: subject.as_deref(),
                sender: sender.as_deref(),
                human_summary: human_summary.as_deref(),
                category: category.as_deref(),
                subcategory: subcategory.as_deref(),
                organization: organization.as_deref(),
                topic: topic.as_deref(),
                email_type: email_type.as_deref(),
                otp_status: otp_status.as_deref(),
                list_id: list_id.as_deref(),
                list_unsubscribe: list_unsubscribe.as_deref(),
                x_priority: x_priority.as_deref(),
                return_path: return_path.as_deref(),
                reply_to: reply_to.as_deref(),
                body_clean: &cleaned_body,
            });

            if !text.is_empty() {
                out.push((message_id, text));
            }
        }
        Ok(out)
    }
}

/// Extract body text from raw RFC822 for embedding. Prefers text/plain, falls back to text/html.
fn extract_body_from_raw(raw: Option<&str>) -> String {
    let raw = match raw {
        Some(s) if s.len() > 100 => s,
        _ => return String::new(),
    };
    let bytes = raw.as_bytes();
    let parsed = match parse_mail(bytes) {
        Ok(p) => p,
        Err(_) => return String::new(),
    };
    extract_body_from_parsed(&parsed).unwrap_or_default()
}

fn clean_body_for_embedding(body_text: Option<&str>, raw_email_content: Option<&str>) -> String {
    let body_source = if let Some(body) = body_text {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            trimmed.to_string()
        } else {
            extract_body_from_raw(raw_email_content)
        }
    } else {
        extract_body_from_raw(raw_email_content)
    };

    let cleaned = clean_email_body_text(&body_source, 24_000);
    if cleaned.is_empty() {
        clean_email_body_text(raw_email_content.or(body_text).unwrap_or_default(), 24_000)
    } else {
        cleaned
    }
}

fn extract_body_from_parsed(parsed: &ParsedMail) -> Option<String> {
    let mimetype = parsed.ctype.mimetype.to_lowercase();
    if mimetype.starts_with("text/plain") || mimetype.starts_with("text/html") {
        return parsed.get_body().ok();
    }
    if mimetype.starts_with("multipart/") {
        let mut plain: Option<String> = None;
        let mut html: Option<String> = None;
        for sub in &parsed.subparts {
            if let Some(t) = extract_body_from_parsed(sub) {
                let sub_mt = sub.ctype.mimetype.to_lowercase();
                if sub_mt.starts_with("text/html") {
                    html = Some(t);
                } else {
                    plain = Some(t);
                }
            }
        }
        return plain.or(html);
    }
    None
}

struct CanonicalEmbeddingTextInput<'a> {
    subject: Option<&'a str>,
    sender: Option<&'a str>,
    human_summary: Option<&'a str>,
    category: Option<&'a str>,
    subcategory: Option<&'a str>,
    organization: Option<&'a str>,
    topic: Option<&'a str>,
    email_type: Option<&'a str>,
    otp_status: Option<&'a str>,
    list_id: Option<&'a str>,
    list_unsubscribe: Option<&'a str>,
    x_priority: Option<&'a str>,
    return_path: Option<&'a str>,
    reply_to: Option<&'a str>,
    body_clean: &'a str,
}

fn build_canonical_embedding_text(input: &CanonicalEmbeddingTextInput<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(v) = normalized_nonempty(input.subject) {
        sections.push(format!("subject: {}", v));
    }
    if let Some(v) = normalized_nonempty(input.sender) {
        sections.push(format!("sender: {}", v));
        if let Some(domain) = extract_sender_domain(v) {
            sections.push(format!("sender_domain: {}", domain));
        }
    }

    let mut labels: Vec<String> = Vec::new();
    if let Some(v) = normalized_nonempty(input.category) {
        labels.push(format!("category={}", v));
    }
    if let Some(v) = normalized_nonempty(input.subcategory) {
        labels.push(format!("subcategory={}", v));
    }
    if let Some(v) = normalized_nonempty(input.organization) {
        labels.push(format!("organization={}", v));
    }
    if let Some(v) = normalized_nonempty(input.topic) {
        labels.push(format!("topic={}", v));
    }
    if let Some(v) = normalized_nonempty(input.email_type) {
        labels.push(format!("email_type={}", v));
    }
    if let Some(v) = normalized_nonempty(input.otp_status) {
        labels.push(format!("otp_status={}", v));
    }
    if !labels.is_empty() {
        sections.push(format!("labels: {}", labels.join(" | ")));
    }

    let mut delivery: Vec<String> = Vec::new();
    if let Some(v) = normalized_nonempty(input.list_id) {
        delivery.push(format!("list_id={}", v));
    }
    if let Some(v) = normalized_nonempty(input.list_unsubscribe) {
        delivery.push(format!("list_unsubscribe={}", v));
    }
    if let Some(v) = normalized_nonempty(input.x_priority) {
        delivery.push(format!("x_priority={}", v));
    }
    if let Some(v) = normalized_nonempty(input.return_path) {
        delivery.push(format!("return_path={}", v));
    }
    if let Some(v) = normalized_nonempty(input.reply_to) {
        delivery.push(format!("reply_to={}", v));
    }
    if !delivery.is_empty() {
        sections.push(format!("delivery_signals: {}", delivery.join(" | ")));
    }

    if let Some(v) = normalized_nonempty(input.human_summary) {
        sections.push(format!("summary: {}", clean_email_body_text(v, 800)));
    }
    if let Some(v) = normalized_nonempty(Some(input.body_clean)) {
        sections.push(format!("body: {}", v));
    }
    sections.join("\n")
}

fn normalized_nonempty(input: Option<&str>) -> Option<&str> {
    let trimmed = input?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn extract_sender_domain(sender: &str) -> Option<String> {
    let trimmed = sender.trim();
    let candidate = if let (Some(start), Some(end)) = (trimmed.find('<'), trimmed.find('>')) {
        trimmed[start + 1..end].trim()
    } else {
        trimmed
    };
    let at_idx = candidate.rfind('@')?;
    let domain = candidate[at_idx + 1..].trim().to_lowercase();
    if domain.is_empty() {
        None
    } else {
        Some(domain)
    }
}

impl Database {
    /// Semantic search: nearest emails by cosine similarity. Returns message_id, subject, sender, human_summary, distance.
    pub async fn search_similar_emails_by_vector(
        &self,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_vector_for_account(DEFAULT_ACCOUNT_ID, query)
            .await
    }

    /// Semantic search: nearest emails by cosine similarity. Returns message_id, subject, sender, human_summary, distance.
    pub async fn search_similar_emails_by_vector_for_account(
        &self,
        account_id: &str,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_vector_with_threshold_for_account(
            account_id,
            SimilarEmailVectorQuery {
                max_distance: 1.0,
                ..query
            },
        )
        .await
    }

    /// Semantic search with max distance threshold (lower distance = more similar).
    pub async fn search_similar_emails_by_vector_with_threshold(
        &self,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_vector_with_threshold_for_account(DEFAULT_ACCOUNT_ID, query)
            .await
    }

    /// Semantic search with max distance threshold (lower distance = more similar).
    pub async fn search_similar_emails_by_vector_with_threshold_for_account(
        &self,
        account_id: &str,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_vector_with_threshold_and_hints_for_account(account_id, query)
            .await
    }

    /// Semantic search with max distance threshold and structured hints to improve ranking stability.
    pub async fn search_similar_emails_by_vector_with_threshold_and_hints(
        &self,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_vector_with_threshold_and_hints_for_account(
            DEFAULT_ACCOUNT_ID,
            query,
        )
        .await
    }

    /// Semantic search with max distance threshold and structured hints to improve ranking stability.
    pub async fn search_similar_emails_by_vector_with_threshold_and_hints_for_account(
        &self,
        account_id: &str,
        query: SimilarEmailVectorQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        let v = Vector::from(query.embedding.to_vec());
        let rows = sqlx::query(
            r#"
            SELECT message_id, subject, sender, human_summary, organization, list_id,
                   spam_status, marketing_status, email_type, category, topic,
                   (embedding <=> $2)::float as distance
            FROM emails
            WHERE account_id = $1
              AND embedding IS NOT NULL
              AND ai_summary IS NOT NULL
              AND (embedding <=> $2) <= $4
              AND ($9::text IS NULL OR message_id <> $9)
            ORDER BY
              (
                (embedding <=> $2)
                - CASE
                    WHEN $5::text IS NOT NULL AND sender = $5 THEN 0.08
                    ELSE 0.0
                  END
                - CASE
                    WHEN $6::text IS NOT NULL AND category = $6 THEN 0.05
                    ELSE 0.0
                  END
                - CASE
                    WHEN $7::text IS NOT NULL AND email_type = $7 THEN 0.04
                    ELSE 0.0
                  END
                - CASE
                    WHEN $8::text IS NOT NULL AND organization = $8 THEN 0.04
                    ELSE 0.0
                  END
                - CASE
                    WHEN $10::text IS NOT NULL AND list_id = $10 THEN 0.03
                    ELSE 0.0
                  END
              ) ASC,
              (embedding <=> $2) ASC
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(v)
        .bind(query.limit as i64)
        .bind(query.max_distance)
        .bind(query.hints.sender)
        .bind(query.hints.category)
        .bind(query.hints.email_type)
        .bind(query.hints.organization)
        .bind(query.hints.exclude_message_id)
        .bind(query.hints.list_id)
        .fetch_all(&self.pool)
        .await
        .context("search_similar_emails_by_vector_with_threshold_and_hints")?;
        Ok(rows
            .into_iter()
            .map(|r| SimilarEmailResult {
                message_id: r.get("message_id"),
                subject: r.get("subject"),
                sender: r.get("sender"),
                human_summary: r.get("human_summary"),
                organization: r.get("organization"),
                list_id: r.get("list_id"),
                spam_status: r.get("spam_status"),
                marketing_status: r.get("marketing_status"),
                email_type: r.get("email_type"),
                category: r.get("category"),
                topic: r.get("topic"),
                distance: r.get::<f64, _>("distance"),
            })
            .collect())
    }

    /// Keyword/full-text retrieval for similar emails using subject/sender/summary/labels/body.
    /// Returns `SimilarEmailResult` with a synthetic distance derived from lexical rank
    /// (`distance = 1 / (1 + rank)`; lower remains "more similar").
    pub async fn search_similar_emails_by_keywords_with_hints(
        &self,
        query: SimilarEmailKeywordQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        self.search_similar_emails_by_keywords_with_hints_for_account(DEFAULT_ACCOUNT_ID, query)
            .await
    }

    /// Keyword/full-text retrieval for similar emails using subject/sender/summary/labels/body.
    /// Returns `SimilarEmailResult` with a synthetic distance derived from lexical rank
    /// (`distance = 1 / (1 + rank)`; lower remains "more similar").
    pub async fn search_similar_emails_by_keywords_with_hints_for_account(
        &self,
        account_id: &str,
        query: SimilarEmailKeywordQuery<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        let query_text = query.query_text.trim();
        if query_text.is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            WITH q AS (
              SELECT plainto_tsquery('english', $2) AS tsq
            ),
            candidates AS (
              SELECT
                e.message_id,
                e.subject,
                e.sender,
                e.human_summary,
                e.organization,
                e.list_id,
                e.spam_status,
                e.marketing_status,
                e.email_type,
                e.category,
                e.topic,
                e.received_date,
                (
                  setweight(
                    to_tsvector(
                      'english',
                      regexp_replace(COALESCE(e.subject, ''), '[^[:space:]]{128,}', ' ', 'g')
                    ),
                    'A'
                  ) ||
                  setweight(
                    to_tsvector(
                      'english',
                      regexp_replace(COALESCE(e.sender, ''), '[^[:space:]]{128,}', ' ', 'g')
                    ),
                    'A'
                  ) ||
                  setweight(
                    to_tsvector(
                      'english',
                      regexp_replace(COALESCE(e.human_summary, ''), '[^[:space:]]{128,}', ' ', 'g')
                    ),
                    'B'
                  ) ||
                  setweight(
                    to_tsvector(
                      'english',
                      regexp_replace(
                        COALESCE(e.category, '') || ' ' ||
                        COALESCE(e.subcategory, '') || ' ' ||
                        COALESCE(e.organization, '') || ' ' ||
                        COALESCE(e.topic, '') || ' ' ||
                        COALESCE(e.email_type, '') || ' ' ||
                        COALESCE(e.list_id, ''),
                        '[^[:space:]]{128,}',
                        ' ',
                        'g'
                      )
                    ),
                    'B'
                  ) ||
                  setweight(
                    to_tsvector(
                      'english',
                      LEFT(
                        regexp_replace(
                          COALESCE(
                            NULLIF(TRIM(COALESCE(e.body_text, '')), ''),
                            regexp_replace(
                              LEFT(COALESCE(e.raw_email_content, ''), 12000),
                              '[^[:alnum:]@._-]+',
                              ' ',
                              'g'
                            ),
                            ''
                          ),
                          '[^[:space:]]{128,}',
                          ' ',
                          'g'
                        ),
                        2000
                      )
                    ),
                    'C'
                  )
                ) AS doc_tsv
              FROM emails e
              CROSS JOIN q
              WHERE e.account_id = $1
                AND e.ai_summary IS NOT NULL
                AND numnode(q.tsq) > 0
                AND ($8::text IS NULL OR e.message_id <> $8)
            ),
            scored AS (
              SELECT
                c.message_id,
                c.subject,
                c.sender,
                c.human_summary,
                c.organization,
                c.list_id,
                c.spam_status,
                c.marketing_status,
                c.email_type,
                c.category,
                c.topic,
                c.received_date,
                ts_rank_cd(c.doc_tsv, q.tsq)::float AS lexical_rank
              FROM candidates c
              CROSS JOIN q
              WHERE c.doc_tsv @@ q.tsq
            )
            SELECT
              message_id,
              subject,
              sender,
              human_summary,
              organization,
              list_id,
              spam_status,
              marketing_status,
              email_type,
              category,
              topic,
              (1.0 / (1.0 + lexical_rank))::float AS distance
            FROM scored
            ORDER BY
              (
                lexical_rank
                + CASE
                    WHEN $4::text IS NOT NULL AND sender = $4 THEN 0.35
                    ELSE 0.0
                  END
                + CASE
                    WHEN $5::text IS NOT NULL AND category = $5 THEN 0.20
                    ELSE 0.0
                  END
                + CASE
                    WHEN $6::text IS NOT NULL AND email_type = $6 THEN 0.15
                    ELSE 0.0
                  END
                + CASE
                    WHEN $7::text IS NOT NULL AND organization = $7 THEN 0.15
                    ELSE 0.0
                  END
                + CASE
                    WHEN $9::text IS NOT NULL AND list_id = $9 THEN 0.10
                    ELSE 0.0
                  END
              ) DESC,
              lexical_rank DESC,
              received_date DESC NULLS LAST
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(query_text)
        .bind(query.limit as i64)
        .bind(query.hints.sender)
        .bind(query.hints.category)
        .bind(query.hints.email_type)
        .bind(query.hints.organization)
        .bind(query.hints.exclude_message_id)
        .bind(query.hints.list_id)
        .fetch_all(&self.pool)
        .await
        .context("search_similar_emails_by_keywords_with_hints")?;

        Ok(rows
            .into_iter()
            .map(|r| SimilarEmailResult {
                message_id: r.get("message_id"),
                subject: r.get("subject"),
                sender: r.get("sender"),
                human_summary: r.get("human_summary"),
                organization: r.get("organization"),
                list_id: r.get("list_id"),
                spam_status: r.get("spam_status"),
                marketing_status: r.get("marketing_status"),
                email_type: r.get("email_type"),
                category: r.get("category"),
                topic: r.get("topic"),
                distance: r.get::<f64, _>("distance"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_canonical_embedding_text, clean_body_for_embedding, CanonicalEmbeddingTextInput,
    };

    #[test]
    fn clean_body_for_embedding_falls_back_to_raw_when_parsed_body_is_empty() {
        let raw = "X-Broken: true\r\n\r\n".repeat(12);

        let cleaned = clean_body_for_embedding(Some("   "), Some(&raw));

        assert!(!cleaned.is_empty());
        assert!(cleaned.contains("X-Broken"));
    }

    #[test]
    fn canonical_embedding_text_keeps_metadata_without_body() {
        let text = build_canonical_embedding_text(&CanonicalEmbeddingTextInput {
            subject: Some("Delivery update"),
            sender: Some("sender@example.com"),
            human_summary: None,
            category: None,
            subcategory: None,
            organization: None,
            topic: None,
            email_type: None,
            otp_status: None,
            list_id: None,
            list_unsubscribe: None,
            x_priority: None,
            return_path: None,
            reply_to: None,
            body_clean: "",
        });

        assert!(text.contains("subject: Delivery update"));
        assert!(text.contains("sender_domain: example.com"));
        assert!(!text.contains("body:"));
    }
}
