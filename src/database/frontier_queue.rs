use anyhow::{Context, Result};
use sqlx::Row;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct FrontierQueueEntry {
    pub message_id: String,
    pub attempt_count: i32,
}

#[derive(Debug, Clone, Default)]
pub struct FrontierQueueDepth {
    pub pending: i64,
    pub failed: i64,
    pub processing: i64,
    pub dead: i64,
}

impl Database {
    /// Enqueue a message for frontier (re)analysis.
    /// Idempotent: re-enqueue resets retry/dead state and makes the row claimable immediately.
    pub async fn enqueue_frontier_analysis(&self, message_id: &str) -> Result<u64> {
        self.enqueue_frontier_analysis_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn enqueue_frontier_analysis_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            INSERT INTO frontier_analysis_queue (
                account_id, message_id, enqueued_at, status, attempt_count, available_at, locked_at, worker_id, last_error, updated_at
            )
            VALUES ($1, $2, NOW(), 'pending', 0, NOW(), NULL, NULL, NULL, NOW())
            ON CONFLICT (account_id, message_id) DO UPDATE
            SET
                enqueued_at = NOW(),
                status = 'pending',
                attempt_count = 0,
                available_at = NOW(),
                locked_at = NULL,
                worker_id = NULL,
                last_error = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("enqueue_frontier_analysis")?;
        Ok(result.rows_affected())
    }

    /// Claim a batch of durable frontier-analysis jobs using FOR UPDATE SKIP LOCKED.
    pub async fn claim_frontier_queue_batch(
        &self,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<FrontierQueueEntry>> {
        self.claim_frontier_queue_batch_for_account(DEFAULT_ACCOUNT_ID, worker_id, limit)
            .await
    }

    pub async fn claim_frontier_queue_batch_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<FrontierQueueEntry>> {
        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT message_id
                FROM frontier_analysis_queue
                WHERE account_id = $1
                  AND status IN ('pending', 'failed')
                  AND available_at <= NOW()
                ORDER BY available_at ASC, enqueued_at ASC
                LIMIT $3
                FOR UPDATE SKIP LOCKED
            )
            UPDATE frontier_analysis_queue q
            SET status = 'processing',
                attempt_count = q.attempt_count + 1,
                locked_at = NOW(),
                worker_id = $2,
                updated_at = NOW()
            FROM claimable c
            WHERE q.account_id = $1
              AND q.message_id = c.message_id
            RETURNING q.message_id, q.attempt_count
            "#,
        )
        .bind(account_id)
        .bind(worker_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("claim_frontier_queue_batch")?;
        Ok(rows
            .into_iter()
            .map(|r| FrontierQueueEntry {
                message_id: r.get::<String, _>("message_id"),
                attempt_count: r.get::<i32, _>("attempt_count"),
            })
            .collect())
    }

    /// Mark a frontier queue job as done by deleting it from the durable queue.
    pub async fn mark_frontier_done(&self, message_id: &str) -> Result<u64> {
        self.mark_frontier_done_for_account(DEFAULT_ACCOUNT_ID, message_id)
            .await
    }

    pub async fn mark_frontier_done_for_account(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM frontier_analysis_queue WHERE account_id = $1 AND message_id = $2",
        )
        .bind(account_id)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("mark_frontier_done")?;
        Ok(result.rows_affected())
    }

    /// Mark queue job as failed with retry scheduling, or dead-letter when max attempts exceeded.
    pub async fn mark_frontier_retry_or_dead(
        &self,
        message_id: &str,
        attempt_count: i32,
        max_attempts: i32,
        retry_after_secs: i64,
        error: &str,
    ) -> Result<String> {
        self.mark_frontier_retry_or_dead_for_account(
            DEFAULT_ACCOUNT_ID,
            message_id,
            attempt_count,
            max_attempts,
            retry_after_secs,
            error,
        )
        .await
    }

    pub async fn mark_frontier_retry_or_dead_for_account(
        &self,
        account_id: &str,
        message_id: &str,
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
            UPDATE frontier_analysis_queue
            SET status = $3,
                available_at = CASE WHEN $3 = 'failed' THEN NOW() + ($4 * INTERVAL '1 second') ELSE available_at END,
                locked_at = NULL,
                worker_id = NULL,
                last_error = LEFT($5, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND message_id = $2
            "#,
        )
        .bind(account_id)
        .bind(message_id)
        .bind(next_status)
        .bind(retry_after_secs.max(0))
        .bind(error)
        .execute(&self.pool)
        .await
        .context("mark_frontier_retry_or_dead")?;

        if result.rows_affected() == 0 {
            anyhow::bail!(
                "mark_frontier_retry_or_dead: queue item {} not found",
                message_id
            );
        }
        Ok(next_status.to_string())
    }

    /// Opportunistically reset stale frontier jobs if a worker died while holding them.
    pub async fn reset_stale_frontier_processing(&self, stale_after_secs: i64) -> Result<u64> {
        self.reset_stale_frontier_processing_for_account(DEFAULT_ACCOUNT_ID, stale_after_secs)
            .await
    }

    pub async fn reset_stale_frontier_processing_for_account(
        &self,
        account_id: &str,
        stale_after_secs: i64,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE frontier_analysis_queue
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
        .context("reset_stale_frontier_processing")?;
        Ok(result.rows_affected())
    }

    /// Frontier queue depth by status bucket.
    pub async fn frontier_queue_depth(&self) -> Result<FrontierQueueDepth> {
        self.frontier_queue_depth_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn frontier_queue_depth_for_account(
        &self,
        account_id: &str,
    ) -> Result<FrontierQueueDepth> {
        let row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (WHERE status = 'pending') AS pending,
              COUNT(*) FILTER (WHERE status = 'failed') AS failed,
              COUNT(*) FILTER (WHERE status = 'processing') AS processing,
              COUNT(*) FILTER (WHERE status = 'dead') AS dead
            FROM frontier_analysis_queue
            WHERE account_id = $1
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("frontier_queue_depth")?;
        Ok(FrontierQueueDepth {
            pending: row.get::<i64, _>("pending"),
            failed: row.get::<i64, _>("failed"),
            processing: row.get::<i64, _>("processing"),
            dead: row.get::<i64, _>("dead"),
        })
    }
}
