use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{postgres::PgRow, types::Json, Row};

use crate::db::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreWorkType {
    SyncFull,
    SyncIncremental,
    SyncBody,
    Analyze,
    Embed,
    Locate,
    FilePreview,
    FileApply,
    AssistantHeartbeat,
    SubagentTask,
}

impl CoreWorkType {
    pub fn as_str(self) -> &'static str {
        match self {
            CoreWorkType::SyncFull => "sync_full",
            CoreWorkType::SyncIncremental => "sync_incremental",
            CoreWorkType::SyncBody => "sync_body",
            CoreWorkType::Analyze => "analyze",
            CoreWorkType::Embed => "embed",
            CoreWorkType::Locate => "locate",
            CoreWorkType::FilePreview => "file_preview",
            CoreWorkType::FileApply => "file_apply",
            CoreWorkType::AssistantHeartbeat => "assistant_heartbeat",
            CoreWorkType::SubagentTask => "subagent_task",
        }
    }
}

impl FromStr for CoreWorkType {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sync_full" => Ok(CoreWorkType::SyncFull),
            "sync_incremental" => Ok(CoreWorkType::SyncIncremental),
            "sync_body" => Ok(CoreWorkType::SyncBody),
            "analyze" => Ok(CoreWorkType::Analyze),
            "embed" => Ok(CoreWorkType::Embed),
            "locate" => Ok(CoreWorkType::Locate),
            "file_preview" => Ok(CoreWorkType::FilePreview),
            "file_apply" => Ok(CoreWorkType::FileApply),
            "assistant_heartbeat" => Ok(CoreWorkType::AssistantHeartbeat),
            "subagent_task" => Ok(CoreWorkType::SubagentTask),
            _ => anyhow::bail!("unknown core work type '{}'", value),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoreWorkQueueEntry {
    pub id: i64,
    pub work_type: CoreWorkType,
    pub payload: Value,
    pub attempt_count: i32,
    pub max_attempts: i32,
    pub worker_id: String,
    pub locked_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CoreWorkQueueDepth {
    pub pending: i64,
    pub failed: i64,
    pub processing: i64,
    pub dead: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreWorkBackpressureConfig {
    pub max_active: i64,
}

impl CoreWorkBackpressureConfig {
    pub fn from_env() -> Self {
        let max_active = std::env::var("CORE_WORK_QUEUE_MAX_ACTIVE")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_CORE_WORK_QUEUE_MAX_ACTIVE)
            .max(1);
        Self { max_active }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoreWorkQueuePressure {
    pub active: i64,
    pub max_active: i64,
    pub backpressured: bool,
}

impl CoreWorkQueuePressure {
    fn new(active: i64, max_active: i64) -> Self {
        let max_active = max_active.max(1);
        Self {
            active,
            max_active,
            backpressured: active >= max_active,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreWorkStatusItem {
    pub id: i64,
    pub work_type: String,
    pub idempotency_key: String,
    pub status: String,
    pub source: Option<String>,
    pub reason: Option<String>,
    pub attempt_count: i32,
    pub max_attempts: i32,
    pub worker_id: Option<String>,
    pub last_error: Option<String>,
    pub available_at: DateTime<Utc>,
    pub locked_at: Option<DateTime<Utc>>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorePipelineTimestamps {
    pub last_sync: Option<DateTime<Utc>>,
    pub last_analysis: Option<DateTime<Utc>>,
    pub last_locate: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreWorkStatusSummary {
    pub account_id: String,
    pub state: String,
    pub queue_depth: CoreWorkQueueDepth,
    pub queue_pressure: CoreWorkQueuePressure,
    pub active_work: Vec<CoreWorkStatusItem>,
    pub recent_failures: Vec<CoreWorkStatusItem>,
    pub recent_completed: Vec<CoreWorkStatusItem>,
    pub pipeline: CorePipelineTimestamps,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreWorkEnqueueOutcome {
    Enqueued(u64),
    Backpressured(CoreWorkQueuePressure),
}

const DEFAULT_CORE_WORK_QUEUE_MAX_ACTIVE: i64 = 10_000;
const DEFAULT_CORE_WORK_LEASE_SECS: i64 = 600;

fn core_work_claim_lease_secs_from_env() -> i64 {
    std::env::var("CORE_WORK_LEASE_SECS")
        .or_else(|_| std::env::var("CORE_WORK_STALE_AFTER_SECS"))
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_CORE_WORK_LEASE_SECS)
        .max(1)
}

fn payload_text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn core_work_status_is_active(status: &str) -> bool {
    matches!(status, "pending" | "failed" | "processing")
}

fn core_work_status_item_from_row(row: &PgRow) -> CoreWorkStatusItem {
    let payload = row.get::<Json<Value>, _>("payload").0;
    CoreWorkStatusItem {
        id: row.get::<i64, _>("id"),
        work_type: row.get::<String, _>("work_type"),
        idempotency_key: row.get::<String, _>("idempotency_key"),
        status: row.get::<String, _>("status"),
        source: payload_text(&payload, "requested_by").or_else(|| payload_text(&payload, "source")),
        reason: payload_text(&payload, "reason"),
        attempt_count: row.get::<i32, _>("attempt_count"),
        max_attempts: row.get::<i32, _>("max_attempts"),
        worker_id: row.get::<Option<String>, _>("worker_id"),
        last_error: row.get::<Option<String>, _>("last_error"),
        available_at: row.get::<DateTime<Utc>, _>("available_at"),
        locked_at: row.get::<Option<DateTime<Utc>>, _>("locked_at"),
        lease_expires_at: row.get::<Option<DateTime<Utc>>, _>("lease_expires_at"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
        completed_at: row.get::<Option<DateTime<Utc>>, _>("completed_at"),
        payload,
    }
}

fn core_work_queue_entry_from_row(row: &PgRow) -> Result<CoreWorkQueueEntry> {
    Ok(CoreWorkQueueEntry {
        id: row.get::<i64, _>("id"),
        work_type: CoreWorkType::from_str(&row.get::<String, _>("work_type"))?,
        payload: row.get::<Json<Value>, _>("payload").0,
        attempt_count: row.get::<i32, _>("attempt_count"),
        max_attempts: row.get::<i32, _>("max_attempts"),
        worker_id: row.get::<String, _>("worker_id"),
        locked_at: row.get::<DateTime<Utc>, _>("locked_at"),
        lease_expires_at: row.get::<DateTime<Utc>, _>("lease_expires_at"),
    })
}

fn core_state_from_work(depth: &CoreWorkQueueDepth, active: &[CoreWorkStatusItem]) -> String {
    if depth.dead > 0 {
        return "error".to_string();
    }

    if let Some(active_type) = active.first().map(|item| item.work_type.as_str()) {
        return match active_type {
            "sync_full" | "sync_incremental" | "sync_body" => "syncing",
            "analyze" | "embed" => "analyzing",
            "locate" | "file_preview" | "file_apply" => "locating",
            _ => "working",
        }
        .to_string();
    }

    if depth.failed > 0 {
        "error".to_string()
    } else if depth.pending > 0 {
        "queued".to_string()
    } else {
        "idle".to_string()
    }
}

fn record_core_work_lease_rejected(account_id: &str, claim: &CoreWorkQueueEntry, action: &str) {
    let source = payload_text(&claim.payload, "requested_by")
        .or_else(|| payload_text(&claim.payload, "source"))
        .unwrap_or_else(|| "system".to_string());
    let reason =
        payload_text(&claim.payload, "reason").unwrap_or_else(|| "unspecified".to_string());
    crate::metrics::counter(
        "core_work_lease_rejected_total",
        1,
        &[("work_type", claim.work_type.as_str()), ("action", action)],
    );
    log::warn!(
        target: "core_work",
        "{}",
        serde_json::json!({
            "event": "core_work_lease_rejected",
            "account_id": account_id,
            "id": claim.id,
            "work_type": claim.work_type.as_str(),
            "source": source,
            "reason": reason,
            "action": action,
            "worker_id": claim.worker_id.as_str(),
            "locked_at": claim.locked_at,
            "claimed_lease_expires_at": claim.lease_expires_at,
        })
    );
}

impl Database {
    pub async fn enqueue_core_work_for_account(
        &self,
        account_id: &str,
        work_type: CoreWorkType,
        idempotency_key: &str,
        payload: Value,
    ) -> Result<u64> {
        match self
            .try_enqueue_core_work_for_account(
                account_id,
                work_type,
                idempotency_key,
                payload,
                CoreWorkBackpressureConfig::from_env(),
            )
            .await?
        {
            CoreWorkEnqueueOutcome::Enqueued(rows) => Ok(rows),
            CoreWorkEnqueueOutcome::Backpressured(pressure) => {
                anyhow::bail!(
                    "core work queue backpressure: active {} >= max {}",
                    pressure.active,
                    pressure.max_active
                )
            }
        }
    }

    pub async fn try_enqueue_core_work_for_account(
        &self,
        account_id: &str,
        work_type: CoreWorkType,
        idempotency_key: &str,
        payload: Value,
        backpressure: CoreWorkBackpressureConfig,
    ) -> Result<CoreWorkEnqueueOutcome> {
        let source = payload_text(&payload, "requested_by")
            .or_else(|| payload_text(&payload, "source"))
            .unwrap_or_else(|| "system".to_string());
        let reason = payload_text(&payload, "reason").unwrap_or_else(|| "unspecified".to_string());
        let existing_status = sqlx::query_scalar::<_, String>(
            r#"
            SELECT status
            FROM core_work_queue
            WHERE account_id = $1
              AND work_type = $2
              AND idempotency_key = $3
            "#,
        )
        .bind(account_id)
        .bind(work_type.as_str())
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .context("load existing core work status for backpressure")?;
        let pressure = self
            .core_work_queue_pressure_for_account_with_limit(account_id, backpressure.max_active)
            .await?;
        let would_increase_active = existing_status
            .as_deref()
            .map(|status| !core_work_status_is_active(status))
            .unwrap_or(true);
        if would_increase_active && pressure.backpressured {
            crate::metrics::counter(
                "core_work_enqueue_backpressure_total",
                1,
                &[
                    ("work_type", work_type.as_str()),
                    ("source", source.as_str()),
                ],
            );
            log::warn!(
                target: "core_work",
                "{}",
                serde_json::json!({
                    "event": "core_work_enqueue_backpressure",
                    "account_id": account_id,
                    "work_type": work_type.as_str(),
                    "idempotency_key": idempotency_key,
                    "source": source,
                    "reason": reason,
                    "active": pressure.active,
                    "max_active": pressure.max_active,
                })
            );
            return Ok(CoreWorkEnqueueOutcome::Backpressured(pressure));
        }

        let result = sqlx::query(
            r#"
            INSERT INTO core_work_queue (
                account_id, work_type, idempotency_key, payload, status, attempt_count,
                max_attempts, available_at, locked_at, lease_expires_at, worker_id,
                last_error, created_at, updated_at, completed_at
            )
            VALUES ($1, $2, $3, $4, 'pending', 0, 3, NOW(), NULL, NULL, NULL, NULL, NOW(), NOW(), NULL)
            ON CONFLICT (account_id, work_type, idempotency_key) DO UPDATE
            SET
                payload = EXCLUDED.payload,
                status = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.status
                    ELSE 'pending'
                END,
                attempt_count = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.attempt_count
                    ELSE 0
                END,
                available_at = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.available_at
                    ELSE NOW()
                END,
                locked_at = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.locked_at
                    ELSE NULL
                END,
                lease_expires_at = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.lease_expires_at
                    ELSE NULL
                END,
                worker_id = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.worker_id
                    ELSE NULL
                END,
                last_error = CASE
                    WHEN core_work_queue.status = 'processing' THEN core_work_queue.last_error
                    ELSE NULL
                END,
                completed_at = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(work_type.as_str())
        .bind(idempotency_key)
        .bind(Json(payload.clone()))
        .execute(&self.pool)
        .await
        .context("enqueue_core_work")?;
        crate::metrics::counter(
            "core_work_enqueue_total",
            1,
            &[
                ("work_type", work_type.as_str()),
                ("source", source.as_str()),
            ],
        );
        log::info!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_enqueue",
                "account_id": account_id,
                "work_type": work_type.as_str(),
                "idempotency_key": idempotency_key,
                "source": source,
                "reason": reason,
                "rows_affected": result.rows_affected(),
            })
        );
        Ok(CoreWorkEnqueueOutcome::Enqueued(result.rows_affected()))
    }

    pub async fn claim_core_work_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
    ) -> Result<Option<CoreWorkQueueEntry>> {
        self.claim_core_work_with_lease_for_account(
            account_id,
            worker_id,
            core_work_claim_lease_secs_from_env(),
        )
        .await
    }

    pub async fn claim_core_work_with_lease_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        lease_secs: i64,
    ) -> Result<Option<CoreWorkQueueEntry>> {
        let row = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT id
                FROM core_work_queue
                WHERE account_id = $1
                  AND status IN ('pending', 'failed')
                  AND available_at <= NOW()
                ORDER BY
                    CASE work_type
                        WHEN 'assistant_heartbeat' THEN 1
                        WHEN 'subagent_task' THEN 1
                        ELSE 0
                    END ASC,
                    CASE work_type
                        WHEN 'sync_full' THEN 0
                        WHEN 'sync_body' THEN 1
                        WHEN 'sync_incremental' THEN 2
                        WHEN 'analyze' THEN 3
                        WHEN 'embed' THEN 4
                        WHEN 'locate' THEN 5
                        WHEN 'file_preview' THEN 6
                        WHEN 'file_apply' THEN 7
                        WHEN 'assistant_heartbeat' THEN 8
                        WHEN 'subagent_task' THEN 9
                        ELSE 10
                    END ASC,
                    available_at ASC,
                    id ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE core_work_queue q
            SET status = 'processing',
                attempt_count = q.attempt_count + 1,
                locked_at = NOW(),
                lease_expires_at = NOW() + ($3 * INTERVAL '1 second'),
                worker_id = $2,
                updated_at = NOW()
            FROM claimable c
            WHERE q.account_id = $1
              AND q.id = c.id
            RETURNING q.id, q.work_type, q.payload, q.attempt_count, q.max_attempts,
                      q.worker_id, q.locked_at, q.lease_expires_at
            "#,
        )
        .bind(account_id)
        .bind(worker_id)
        .bind(lease_secs.max(1))
        .fetch_optional(&self.pool)
        .await
        .context("claim_core_work")?;

        row.map(|r| {
            let entry = core_work_queue_entry_from_row(&r)?;
            let source = payload_text(&entry.payload, "requested_by")
                .or_else(|| payload_text(&entry.payload, "source"))
                .unwrap_or_else(|| "system".to_string());
            let reason =
                payload_text(&entry.payload, "reason").unwrap_or_else(|| "unspecified".to_string());
            crate::metrics::counter(
                "core_work_claim_total",
                1,
                &[
                    ("work_type", entry.work_type.as_str()),
                    ("source", source.as_str()),
                ],
            );
            log::info!(
                target: "core_work",
                "{}",
                serde_json::json!({
                    "event": "core_work_claim",
                    "account_id": account_id,
                    "id": entry.id,
                    "work_type": entry.work_type.as_str(),
                    "source": source,
                    "reason": reason,
                    "attempt_count": entry.attempt_count,
                    "max_attempts": entry.max_attempts,
                    "worker_id": worker_id,
                    "locked_at": entry.locked_at,
                    "lease_expires_at": entry.lease_expires_at,
                })
            );
            Ok(entry)
        })
        .transpose()
    }

    pub async fn claim_core_work_batch_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        work_type: CoreWorkType,
        limit: usize,
    ) -> Result<Vec<CoreWorkQueueEntry>> {
        self.claim_core_work_batch_with_lease_for_account(
            account_id,
            worker_id,
            work_type,
            limit,
            core_work_claim_lease_secs_from_env(),
        )
        .await
    }

    pub async fn claim_core_work_batch_with_lease_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        work_type: CoreWorkType,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<CoreWorkQueueEntry>> {
        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT id
                FROM core_work_queue
                WHERE account_id = $1
                  AND work_type = $3
                  AND status IN ('pending', 'failed')
                  AND available_at <= NOW()
                ORDER BY available_at ASC, id ASC
                LIMIT $4
                FOR UPDATE SKIP LOCKED
            )
            UPDATE core_work_queue q
            SET status = 'processing',
                attempt_count = q.attempt_count + 1,
                locked_at = NOW(),
                lease_expires_at = NOW() + ($5 * INTERVAL '1 second'),
                worker_id = $2,
                updated_at = NOW()
            FROM claimable c
            WHERE q.account_id = $1
              AND q.id = c.id
            RETURNING q.id, q.work_type, q.payload, q.attempt_count, q.max_attempts,
                      q.worker_id, q.locked_at, q.lease_expires_at
            "#,
        )
        .bind(account_id)
        .bind(worker_id)
        .bind(work_type.as_str())
        .bind(limit.min(i64::MAX as usize) as i64)
        .bind(lease_secs.max(1))
        .fetch_all(&self.pool)
        .await
        .context("claim_core_work_batch")?;

        rows.into_iter()
            .map(|r| core_work_queue_entry_from_row(&r))
            .collect()
    }

    pub async fn core_work_due_for_account(
        &self,
        account_id: &str,
        work_type: CoreWorkType,
        idempotency_key: &str,
        interval_secs: i64,
    ) -> Result<bool> {
        let row = sqlx::query(
            r#"
            SELECT status, completed_at
            FROM core_work_queue
            WHERE account_id = $1
              AND work_type = $2
              AND idempotency_key = $3
            "#,
        )
        .bind(account_id)
        .bind(work_type.as_str())
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .context("core_work_due")?;

        let Some(row) = row else {
            return Ok(true);
        };
        let status: String = row.get("status");
        if matches!(status.as_str(), "pending" | "processing" | "failed") {
            return Ok(false);
        }
        let completed_at: Option<DateTime<Utc>> = row.get("completed_at");
        Ok(completed_at
            .map(|completed| {
                completed <= Utc::now() - chrono::Duration::seconds(interval_secs.max(1))
            })
            .unwrap_or(true))
    }

    pub async fn mark_claimed_core_work_done_for_account(
        &self,
        account_id: &str,
        claim: &CoreWorkQueueEntry,
    ) -> Result<bool> {
        let row = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET status = 'done',
                locked_at = NULL,
                lease_expires_at = NULL,
                worker_id = NULL,
                last_error = NULL,
                completed_at = NOW(),
                updated_at = NOW()
            WHERE account_id = $1
              AND id = $2
              AND status = 'processing'
              AND worker_id = $3
              AND locked_at = $4
              AND lease_expires_at > NOW()
            RETURNING work_type, payload
            "#,
        )
        .bind(account_id)
        .bind(claim.id)
        .bind(&claim.worker_id)
        .bind(claim.locked_at)
        .fetch_optional(&self.pool)
        .await
        .context("mark_core_work_done")?;
        let Some(row) = row else {
            record_core_work_lease_rejected(account_id, claim, "complete");
            return Ok(false);
        };
        let payload = row.get::<Json<Value>, _>("payload").0;
        let work_type = row.get::<String, _>("work_type");
        let source = payload_text(&payload, "requested_by")
            .or_else(|| payload_text(&payload, "source"))
            .unwrap_or_else(|| "system".to_string());
        let reason = payload_text(&payload, "reason").unwrap_or_else(|| "unspecified".to_string());
        crate::metrics::counter(
            "core_work_complete_total",
            1,
            &[
                ("work_type", work_type.as_str()),
                ("source", source.as_str()),
            ],
        );
        log::info!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_complete",
                "account_id": account_id,
                "id": claim.id,
                "work_type": work_type,
                "source": source,
                "reason": reason,
                "rows_affected": 1,
            })
        );
        Ok(true)
    }

    pub async fn mark_claimed_core_work_retry_or_dead_for_account(
        &self,
        account_id: &str,
        claim: &CoreWorkQueueEntry,
        retry_after_secs: i64,
        error: &str,
    ) -> Result<Option<String>> {
        let next_status = if claim.attempt_count >= claim.max_attempts {
            "dead"
        } else {
            "failed"
        };
        let row = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET status = $3,
                available_at = CASE WHEN $3 = 'failed' THEN NOW() + ($4 * INTERVAL '1 second') ELSE available_at END,
                locked_at = NULL,
                lease_expires_at = NULL,
                worker_id = NULL,
                last_error = LEFT($5, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND id = $2
              AND status = 'processing'
              AND worker_id = $6
              AND locked_at = $7
              AND lease_expires_at > NOW()
            RETURNING work_type, payload
            "#,
        )
        .bind(account_id)
        .bind(claim.id)
        .bind(next_status)
        .bind(retry_after_secs.max(0))
        .bind(error)
        .bind(&claim.worker_id)
        .bind(claim.locked_at)
        .fetch_optional(&self.pool)
        .await
        .context("mark_core_work_retry_or_dead")?;

        let Some(row) = row else {
            record_core_work_lease_rejected(account_id, claim, "fail");
            return Ok(None);
        };
        let payload = row.get::<Json<Value>, _>("payload").0;
        let work_type = row.get::<String, _>("work_type");
        let source = payload_text(&payload, "requested_by")
            .or_else(|| payload_text(&payload, "source"))
            .unwrap_or_else(|| "system".to_string());
        let reason = payload_text(&payload, "reason").unwrap_or_else(|| "unspecified".to_string());
        let metric_name = if next_status == "dead" {
            "core_work_dead_letter_total"
        } else {
            "core_work_retry_total"
        };
        crate::metrics::counter(
            metric_name,
            1,
            &[
                ("work_type", work_type.as_str()),
                ("source", source.as_str()),
            ],
        );
        log::warn!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": if next_status == "dead" { "core_work_dead_letter" } else { "core_work_retry" },
                "account_id": account_id,
                "id": claim.id,
                "work_type": work_type,
                "source": source,
                "reason": reason,
                "status": next_status,
                "attempt_count": claim.attempt_count,
                "max_attempts": claim.max_attempts,
                "retry_after_secs": retry_after_secs.max(0),
                "error": error,
            })
        );
        Ok(Some(next_status.to_string()))
    }

    pub async fn renew_core_work_lease_for_account(
        &self,
        account_id: &str,
        claim: &CoreWorkQueueEntry,
        lease_secs: i64,
    ) -> Result<bool> {
        let row = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET lease_expires_at = NOW() + ($5 * INTERVAL '1 second'),
                updated_at = NOW()
            WHERE account_id = $1
              AND id = $2
              AND status = 'processing'
              AND worker_id = $3
              AND locked_at = $4
              AND lease_expires_at > NOW()
            RETURNING lease_expires_at
            "#,
        )
        .bind(account_id)
        .bind(claim.id)
        .bind(&claim.worker_id)
        .bind(claim.locked_at)
        .bind(lease_secs.max(1))
        .fetch_optional(&self.pool)
        .await
        .context("renew_core_work_lease")?;

        let Some(row) = row else {
            record_core_work_lease_rejected(account_id, claim, "renew");
            return Ok(false);
        };
        let lease_expires_at = row.get::<DateTime<Utc>, _>("lease_expires_at");
        crate::metrics::counter(
            "core_work_lease_renew_total",
            1,
            &[("work_type", claim.work_type.as_str())],
        );
        log::debug!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_lease_renew",
                "account_id": account_id,
                "id": claim.id,
                "work_type": claim.work_type.as_str(),
                "worker_id": claim.worker_id.as_str(),
                "locked_at": claim.locked_at,
                "lease_expires_at": lease_expires_at,
            })
        );
        Ok(true)
    }

    pub async fn reset_stale_core_work_for_account(
        &self,
        account_id: &str,
        stale_after_secs: i64,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET status = 'failed',
                available_at = NOW(),
                locked_at = NULL,
                lease_expires_at = NULL,
                worker_id = NULL,
                last_error = COALESCE(last_error, 'expired lease reset'),
                updated_at = NOW()
            WHERE account_id = $1
              AND status = 'processing'
              AND locked_at IS NOT NULL
              AND (
                    lease_expires_at <= NOW()
                 OR (lease_expires_at IS NULL AND locked_at < NOW() - ($2 * INTERVAL '1 second'))
              )
            "#,
        )
        .bind(account_id)
        .bind(stale_after_secs.max(1))
        .execute(&self.pool)
        .await
        .context("reset_stale_core_work")?;
        if result.rows_affected() > 0 {
            crate::metrics::counter("core_work_stale_reset_total", result.rows_affected(), &[]);
            crate::metrics::counter(
                "core_work_lease_expired_reset_total",
                result.rows_affected(),
                &[],
            );
            log::warn!(
                target: "core_work",
                "{}",
                serde_json::json!({
                    "event": "core_work_lease_expired_reset",
                    "account_id": account_id,
                    "rows_affected": result.rows_affected(),
                    "stale_after_secs": stale_after_secs.max(1),
                })
            );
        }
        Ok(result.rows_affected())
    }

    pub async fn recover_orphaned_core_work_for_account(
        &self,
        account_id: &str,
        reason: &str,
    ) -> Result<u64> {
        let rows = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET status = 'failed',
                available_at = NOW(),
                locked_at = NULL,
                lease_expires_at = NULL,
                worker_id = NULL,
                last_error = LEFT($2, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND status = 'processing'
            RETURNING id, work_type
            "#,
        )
        .bind(account_id)
        .bind(reason)
        .fetch_all(&self.pool)
        .await
        .context("recover_orphaned_core_work")?;

        if rows.is_empty() {
            return Ok(0);
        }

        let recovered_ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
        sqlx::query(
            r#"
            UPDATE subagent_tasks
            SET status = 'pending',
                started_at = NULL,
                finished_at = NULL,
                error = LEFT($3, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND core_work_id = ANY($2)
              AND status = 'running'
            "#,
        )
        .bind(account_id)
        .bind(&recovered_ids)
        .bind(reason)
        .execute(&self.pool)
        .await
        .context("recover_orphaned_subagent_tasks")?;

        let mut by_work_type = std::collections::BTreeMap::<String, u64>::new();
        for row in &rows {
            let work_type = row.get::<String, _>("work_type");
            *by_work_type.entry(work_type).or_default() += 1;
        }
        for (work_type, count) in by_work_type {
            crate::metrics::counter(
                "core_work_startup_recovery_total",
                count,
                &[("work_type", work_type.as_str())],
            );
        }
        log::warn!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_startup_recovery",
                "account_id": account_id,
                "rows_affected": rows.len(),
                "reason": reason,
            })
        );
        Ok(rows.len() as u64)
    }

    pub async fn release_core_work_for_worker_for_account(
        &self,
        account_id: &str,
        worker_id: &str,
        reason: &str,
    ) -> Result<u64> {
        let rows = sqlx::query(
            r#"
            UPDATE core_work_queue
            SET status = 'failed',
                available_at = NOW(),
                locked_at = NULL,
                lease_expires_at = NULL,
                worker_id = NULL,
                last_error = LEFT($3, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND worker_id = $2
              AND status = 'processing'
            RETURNING id, work_type
            "#,
        )
        .bind(account_id)
        .bind(worker_id)
        .bind(reason)
        .fetch_all(&self.pool)
        .await
        .context("release_core_work_for_worker")?;

        if rows.is_empty() {
            return Ok(0);
        }

        let released_ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
        sqlx::query(
            r#"
            UPDATE subagent_tasks
            SET status = 'pending',
                started_at = NULL,
                finished_at = NULL,
                error = LEFT($3, 4000),
                updated_at = NOW()
            WHERE account_id = $1
              AND core_work_id = ANY($2)
              AND status = 'running'
            "#,
        )
        .bind(account_id)
        .bind(&released_ids)
        .bind(reason)
        .execute(&self.pool)
        .await
        .context("release_subagent_tasks_for_worker")?;

        let mut by_work_type = std::collections::BTreeMap::<String, u64>::new();
        for row in &rows {
            let work_type = row.get::<String, _>("work_type");
            *by_work_type.entry(work_type).or_default() += 1;
        }
        for (work_type, count) in by_work_type {
            crate::metrics::counter(
                "core_work_shutdown_release_total",
                count,
                &[("work_type", work_type.as_str())],
            );
        }
        log::warn!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_shutdown_release",
                "account_id": account_id,
                "worker_id": worker_id,
                "rows_affected": rows.len(),
                "reason": reason,
            })
        );
        Ok(rows.len() as u64)
    }

    pub async fn core_work_queue_depth_for_account(
        &self,
        account_id: &str,
    ) -> Result<CoreWorkQueueDepth> {
        let row = sqlx::query(
            r#"
            SELECT
              COUNT(*) FILTER (WHERE status = 'pending') AS pending,
              COUNT(*) FILTER (WHERE status = 'failed') AS failed,
              COUNT(*) FILTER (WHERE status = 'processing') AS processing,
              COUNT(*) FILTER (WHERE status = 'dead') AS dead
            FROM core_work_queue
            WHERE account_id = $1
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("core_work_queue_depth")?;
        Ok(CoreWorkQueueDepth {
            pending: row.get::<i64, _>("pending"),
            failed: row.get::<i64, _>("failed"),
            processing: row.get::<i64, _>("processing"),
            dead: row.get::<i64, _>("dead"),
        })
    }

    pub async fn core_work_queue_pressure_for_account(
        &self,
        account_id: &str,
    ) -> Result<CoreWorkQueuePressure> {
        self.core_work_queue_pressure_for_account_with_limit(
            account_id,
            CoreWorkBackpressureConfig::from_env().max_active,
        )
        .await
    }

    pub async fn core_work_queue_pressure_for_account_with_limit(
        &self,
        account_id: &str,
        max_active: i64,
    ) -> Result<CoreWorkQueuePressure> {
        let active = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM core_work_queue
            WHERE account_id = $1
              AND status IN ('pending', 'failed', 'processing')
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("core_work_queue_pressure")?;
        Ok(CoreWorkQueuePressure::new(active, max_active))
    }

    pub async fn has_active_core_work_type_for_account(
        &self,
        account_id: &str,
        work_type: CoreWorkType,
    ) -> Result<bool> {
        let active = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM core_work_queue
                WHERE account_id = $1
                  AND work_type = $2
                  AND status IN ('pending', 'processing', 'failed')
            )
            "#,
        )
        .bind(account_id)
        .bind(work_type.as_str())
        .fetch_one(&self.pool)
        .await
        .context("has_active_core_work_type")?;
        Ok(active)
    }

    pub async fn has_active_sync_work_for_account(&self, account_id: &str) -> Result<bool> {
        let active = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM core_work_queue
                WHERE account_id = $1
                  AND work_type IN ('sync_full', 'sync_incremental', 'sync_body')
                  AND status IN ('pending', 'processing')
            )
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("has_active_sync_work")?;
        Ok(active)
    }

    pub async fn core_work_status_for_account(
        &self,
        account_id: &str,
    ) -> Result<CoreWorkStatusSummary> {
        let queue_depth = self.core_work_queue_depth_for_account(account_id).await?;
        let queue_pressure = self
            .core_work_queue_pressure_for_account(account_id)
            .await?;
        crate::metrics::gauge(
            "core_work_queue_active",
            queue_pressure.active as f64,
            &[("account_id", account_id)],
        );
        crate::metrics::gauge(
            "core_work_queue_pressure",
            queue_pressure.active as f64 / queue_pressure.max_active as f64,
            &[("account_id", account_id)],
        );
        let active_work = self
            .list_core_work_status_items_for_account(
                account_id,
                "status = 'processing'",
                "locked_at DESC NULLS LAST, updated_at DESC",
                10,
            )
            .await
            .context("core active work")?;
        let recent_failures = self
            .list_core_work_status_items_for_account(
                account_id,
                "status IN ('failed', 'dead')",
                "updated_at DESC",
                10,
            )
            .await
            .context("core recent failures")?;
        let recent_completed = self
            .list_core_work_status_items_for_account(
                account_id,
                "status = 'done'",
                "completed_at DESC NULLS LAST, updated_at DESC",
                10,
            )
            .await
            .context("core recent completed")?;

        let pipeline_row = sqlx::query(
            r#"
            SELECT
              MAX(completed_at) FILTER (WHERE work_type IN ('sync_full', 'sync_incremental', 'sync_body')) AS last_sync,
              MAX(completed_at) FILTER (WHERE work_type IN ('analyze', 'embed')) AS last_analysis,
              MAX(completed_at) FILTER (WHERE work_type IN ('locate', 'file_preview', 'file_apply')) AS last_locate
            FROM core_work_queue
            WHERE account_id = $1
              AND status = 'done'
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .context("core pipeline timestamps")?;
        let pipeline = CorePipelineTimestamps {
            last_sync: pipeline_row.get::<Option<DateTime<Utc>>, _>("last_sync"),
            last_analysis: pipeline_row.get::<Option<DateTime<Utc>>, _>("last_analysis"),
            last_locate: pipeline_row.get::<Option<DateTime<Utc>>, _>("last_locate"),
        };

        let last_error = recent_failures
            .iter()
            .find_map(|item| item.last_error.clone());
        let state = core_state_from_work(&queue_depth, &active_work);

        Ok(CoreWorkStatusSummary {
            account_id: account_id.to_string(),
            state,
            queue_depth,
            queue_pressure,
            active_work,
            recent_failures,
            recent_completed,
            pipeline,
            last_error,
        })
    }

    async fn list_core_work_status_items_for_account(
        &self,
        account_id: &str,
        where_clause: &str,
        order_by: &str,
        limit: i64,
    ) -> Result<Vec<CoreWorkStatusItem>> {
        let sql = format!(
            r#"
            SELECT id, work_type, idempotency_key, payload, status, attempt_count,
                   max_attempts, available_at, locked_at, lease_expires_at,
                   worker_id, last_error, created_at, updated_at, completed_at
            FROM core_work_queue
            WHERE account_id = $1 AND {}
            ORDER BY {}
            LIMIT $2
            "#,
            where_clause, order_by
        );
        let rows = sqlx::query(&sql)
            .bind(account_id)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .context("list_core_work_status_items")?;

        Ok(rows.iter().map(core_work_status_item_from_row).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_state_from_work_distinguishes_pipeline_states() {
        let ready_depth = CoreWorkQueueDepth::default();
        assert_eq!(core_state_from_work(&ready_depth, &[]), "idle");

        let queued_depth = CoreWorkQueueDepth {
            pending: 1,
            ..Default::default()
        };
        assert_eq!(core_state_from_work(&queued_depth, &[]), "queued");

        let error_depth = CoreWorkQueueDepth {
            dead: 1,
            ..Default::default()
        };
        assert_eq!(core_state_from_work(&error_depth, &[]), "error");

        let active_sync = core_status_item_for_test("sync_incremental");
        assert_eq!(
            core_state_from_work(&ready_depth, &[active_sync]),
            "syncing"
        );

        let active_analysis = core_status_item_for_test("analyze");
        assert_eq!(
            core_state_from_work(&ready_depth, &[active_analysis]),
            "analyzing"
        );

        let active_locate = core_status_item_for_test("locate");
        assert_eq!(
            core_state_from_work(&ready_depth, &[active_locate]),
            "locating"
        );
    }

    #[test]
    fn queue_pressure_marks_active_count_at_or_above_limit_as_backpressured() {
        let under = CoreWorkQueuePressure::new(9, 10);
        assert_eq!(under.active, 9);
        assert_eq!(under.max_active, 10);
        assert!(!under.backpressured);

        let at_limit = CoreWorkQueuePressure::new(10, 10);
        assert_eq!(at_limit.active, 10);
        assert!(at_limit.backpressured);

        let invalid_limit = CoreWorkQueuePressure::new(1, 0);
        assert_eq!(invalid_limit.max_active, 1);
        assert!(invalid_limit.backpressured);
    }

    fn core_status_item_for_test(work_type: &str) -> CoreWorkStatusItem {
        let now = Utc::now();
        CoreWorkStatusItem {
            id: 1,
            work_type: work_type.to_string(),
            idempotency_key: "test".to_string(),
            status: "processing".to_string(),
            source: Some("test".to_string()),
            reason: Some("test".to_string()),
            attempt_count: 1,
            max_attempts: 3,
            worker_id: Some("test-worker".to_string()),
            last_error: None,
            available_at: now,
            locked_at: Some(now),
            lease_expires_at: Some(now + chrono::Duration::seconds(600)),
            created_at: now,
            updated_at: now,
            completed_at: None,
            payload: serde_json::json!({"source": "test", "reason": "test"}),
        }
    }
}
