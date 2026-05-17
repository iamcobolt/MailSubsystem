use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{types::Json, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct BodySyncQueueEntry {
    pub id: i64,
    pub folder_name: String,
    pub uid: i32,
    pub uid_validity: i32,
    pub message_id: String,
    pub attempt_count: i32,
}

#[derive(Debug, Clone)]
pub struct BodySyncQueueItem {
    pub folder_name: String,
    pub uid: i32,
    pub uid_validity: i32,
    pub message_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct BodySyncQueueDepth {
    pub pending: i64,
    pub failed: i64,
    pub processing: i64,
    pub dead: i64,
}

impl Database {
    /// Upsert envelope rows and enqueue durable body-sync work in one transaction.
    pub async fn upsert_envelopes_and_enqueue_body_sync(
        &self,
        payload: &Value,
        items: &[BodySyncQueueItem],
    ) -> Result<()> {
        self.upsert_envelopes_and_enqueue_body_sync_for_account(DEFAULT_ACCOUNT_ID, payload, items)
            .await
    }

    pub async fn upsert_envelopes_and_enqueue_body_sync_for_account(
        &self,
        account_id: &str,
        payload: &Value,
        items: &[BodySyncQueueItem],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin envelope+queue tx")?;
        sqlx::query("SELECT upsert_emails_from_envelope($1, $2)")
            .bind(account_id)
            .bind(Json(payload))
            .execute(&mut *tx)
            .await
            .context("upsert envelopes in envelope+queue tx")?;

        if !items.is_empty() {
            let queue_payload = Value::Array(
                items
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "folder_name": i.folder_name,
                            "uid": i.uid,
                            "uid_validity": i.uid_validity,
                            "message_id": i.message_id,
                        })
                    })
                    .collect(),
            );
            sqlx::query(
                r#"
                INSERT INTO body_sync_queue (account_id, folder_name, uid, uid_validity, message_id, status, available_at, locked_at, worker_id, last_error, updated_at)
                SELECT $1, j.folder_name, j.uid, j.uid_validity, j.message_id, 'pending', NOW(), NULL, NULL, NULL, NOW()
                FROM jsonb_to_recordset($2::jsonb) AS j(folder_name text, uid int, uid_validity int, message_id text)
                ON CONFLICT (account_id, folder_name, uid, uid_validity, message_id)
                DO UPDATE SET
                    status = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.status ELSE 'pending' END,
                    available_at = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.available_at ELSE NOW() END,
                    locked_at = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.locked_at ELSE NULL END,
                    worker_id = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.worker_id ELSE NULL END,
                    last_error = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.last_error ELSE NULL END,
                    updated_at = NOW()
                "#,
            )
            .bind(account_id)
            .bind(Json(queue_payload))
            .execute(&mut *tx)
            .await
            .context("enqueue body_sync_queue in envelope+queue tx")?;
        }

        tx.commit().await.context("commit envelope+queue tx")?;
        Ok(())
    }

    /// Enqueue body-sync jobs idempotently.
    pub async fn enqueue_body_sync_items(&self, items: &[BodySyncQueueItem]) -> Result<u64> {
        self.enqueue_body_sync_items_for_account(DEFAULT_ACCOUNT_ID, items)
            .await
    }

    pub async fn enqueue_body_sync_items_for_account(
        &self,
        account_id: &str,
        items: &[BodySyncQueueItem],
    ) -> Result<u64> {
        if items.is_empty() {
            return Ok(0);
        }
        let queue_payload = Value::Array(
            items
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "folder_name": i.folder_name,
                        "uid": i.uid,
                        "uid_validity": i.uid_validity,
                        "message_id": i.message_id,
                    })
                })
                .collect(),
        );
        let result = sqlx::query(
            r#"
            INSERT INTO body_sync_queue (account_id, folder_name, uid, uid_validity, message_id, status, available_at, locked_at, worker_id, last_error, updated_at)
            SELECT $1, j.folder_name, j.uid, j.uid_validity, j.message_id, 'pending', NOW(), NULL, NULL, NULL, NOW()
            FROM jsonb_to_recordset($2::jsonb) AS j(folder_name text, uid int, uid_validity int, message_id text)
            ON CONFLICT (account_id, folder_name, uid, uid_validity, message_id)
            DO UPDATE SET
                status = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.status ELSE 'pending' END,
                available_at = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.available_at ELSE NOW() END,
                locked_at = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.locked_at ELSE NULL END,
                worker_id = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.worker_id ELSE NULL END,
                last_error = CASE WHEN body_sync_queue.status = 'done' THEN body_sync_queue.last_error ELSE NULL END,
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(Json(queue_payload))
        .execute(&self.pool)
        .await
        .context("enqueue_body_sync_items")?;
        Ok(result.rows_affected())
    }

    /// Claim a batch of durable body-sync jobs using FOR UPDATE SKIP LOCKED.
    pub async fn claim_body_sync_batch(
        &self,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<BodySyncQueueEntry>> {
        self.claim_body_sync_batch_for_account(DEFAULT_ACCOUNT_ID, worker_id, limit)
            .await
    }

    pub async fn claim_body_sync_batch_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<BodySyncQueueEntry>> {
        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT id
                FROM body_sync_queue
                WHERE account_id = $1
                  AND status IN ('pending', 'failed')
                  AND available_at <= NOW()
                ORDER BY available_at ASC, id ASC
                LIMIT $3
                FOR UPDATE SKIP LOCKED
            )
            UPDATE body_sync_queue q
            SET status = 'processing',
                attempt_count = q.attempt_count + 1,
                locked_at = NOW(),
                worker_id = $2,
                updated_at = NOW()
            FROM claimable c
            WHERE q.account_id = $1
              AND q.id = c.id
            RETURNING q.id, q.folder_name, q.uid, q.uid_validity, q.message_id, q.attempt_count
            "#,
        )
        .bind(account_id)
        .bind(worker_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("claim_body_sync_batch")?;
        Ok(rows
            .into_iter()
            .map(|r| BodySyncQueueEntry {
                id: r.get::<i64, _>("id"),
                folder_name: r.get::<String, _>("folder_name"),
                uid: r.get::<i32, _>("uid"),
                uid_validity: r.get::<i32, _>("uid_validity"),
                message_id: r.get::<String, _>("message_id"),
                attempt_count: r.get::<i32, _>("attempt_count"),
            })
            .collect())
    }

    /// Mark queue jobs as done.
    pub async fn mark_body_sync_done(&self, ids: &[i64]) -> Result<u64> {
        self.mark_body_sync_done_for_account(DEFAULT_ACCOUNT_ID, ids)
            .await
    }

    pub async fn mark_body_sync_done_for_account(
        &self,
        account_id: &str,
        ids: &[i64],
    ) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE body_sync_queue
            SET status = 'done',
                locked_at = NULL,
                worker_id = NULL,
                last_error = NULL,
                updated_at = NOW()
            WHERE account_id = $1
              AND id = ANY($2)
            "#,
        )
        .bind(account_id)
        .bind(ids)
        .execute(&self.pool)
        .await
        .context("mark_body_sync_done")?;
        Ok(result.rows_affected())
    }

    /// Mark queue jobs as failed with retry scheduling, or dead-letter when max attempts exceeded.
    pub async fn mark_body_sync_retry_or_dead(
        &self,
        id: i64,
        attempt_count: i32,
        max_attempts: i32,
        retry_after_secs: i64,
        error: &str,
    ) -> Result<String> {
        self.mark_body_sync_retry_or_dead_for_account(
            DEFAULT_ACCOUNT_ID,
            id,
            attempt_count,
            max_attempts,
            retry_after_secs,
            error,
        )
        .await
    }

    pub async fn mark_body_sync_retry_or_dead_for_account(
        &self,
        account_id: &str,
        id: i64,
        attempt_count: i32,
        max_attempts: i32,
        retry_after_secs: i64,
        error: &str,
    ) -> Result<String> {
        let next_status = if attempt_count >= max_attempts {
            "dead"
        } else {
            "failed"
        };
        let result = sqlx::query(
            r#"
            UPDATE body_sync_queue
            SET status = $3,
                available_at = CASE WHEN $3 = 'failed' THEN NOW() + ($4 * INTERVAL '1 second') ELSE available_at END,
                locked_at = NULL,
                worker_id = NULL,
                last_error = LEFT($5, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND id = $2
            "#,
        )
        .bind(account_id)
        .bind(id)
        .bind(next_status)
        .bind(retry_after_secs.max(0))
        .bind(error)
        .execute(&self.pool)
        .await
        .context("mark_body_sync_retry_or_dead")?;

        if result.rows_affected() == 0 {
            anyhow::bail!("mark_body_sync_retry_or_dead: queue item {} not found", id);
        }
        Ok(next_status.to_string())
    }

    /// Opportunistically reset stale processing jobs if a worker died while holding them.
    pub async fn reset_stale_body_sync_processing(&self, stale_after_secs: i64) -> Result<u64> {
        self.reset_stale_body_sync_processing_for_account(DEFAULT_ACCOUNT_ID, stale_after_secs)
            .await
    }

    pub async fn reset_stale_body_sync_processing_for_account(
        &self,
        account_id: &str,
        stale_after_secs: i64,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE body_sync_queue
            SET status = 'failed',
                available_at = NOW(),
                locked_at = NULL,
                worker_id = NULL,
                last_error = COALESCE(last_error, 'stale lock reset'),
                updated_at = NOW()
            WHERE account_id = $1
              AND status = 'processing'
              AND locked_at IS NOT NULL
              AND locked_at < NOW() - ($2 * INTERVAL '1 second')
            "#,
        )
        .bind(account_id)
        .bind(stale_after_secs.max(1))
        .execute(&self.pool)
        .await
        .context("reset_stale_body_sync_processing")?;
        Ok(result.rows_affected())
    }

    /// Queue depth by status bucket.
    pub async fn body_sync_queue_depth(&self) -> Result<BodySyncQueueDepth> {
        self.body_sync_queue_depth_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn body_sync_queue_depth_for_account(
        &self,
        account_id: &str,
    ) -> Result<BodySyncQueueDepth> {
        let row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (WHERE status = 'pending') AS pending,
              COUNT(*) FILTER (WHERE status = 'failed') AS failed,
              COUNT(*) FILTER (WHERE status = 'processing') AS processing,
              COUNT(*) FILTER (WHERE status = 'dead') AS dead
            FROM body_sync_queue
            WHERE account_id = $1
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("body_sync_queue_depth")?;
        Ok(BodySyncQueueDepth {
            pending: row.get::<i64, _>("pending"),
            failed: row.get::<i64, _>("failed"),
            processing: row.get::<i64, _>("processing"),
            dead: row.get::<i64, _>("dead"),
        })
    }
}
