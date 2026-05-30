use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{types::Json, Row};
use std::env;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentTaskRecord {
    pub task_id: String,
    pub task_kind: String,
    pub worker_name: String,
    pub skill_bundle: String,
    pub message_ids: Vec<String>,
    pub input_context: Value,
    pub priority: i32,
    pub correlation_id: String,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResultRecord {
    pub task_id: String,
    pub worker_name: String,
    pub task_kind: String,
    pub result_json: Value,
    pub confidence: Option<f32>,
    pub evidence: Value,
    pub recommended_actions: Value,
    pub requires_review: bool,
    pub agent_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSkillLessonRecord {
    pub skill_bundle: String,
    pub lesson_key: String,
    pub lesson_type: String,
    pub status: String,
    pub summary: String,
    pub evidence: Value,
    pub score: Option<f32>,
    pub support_count: i32,
    pub negative_count: i32,
    pub source_task_id: Option<String>,
    pub source_result_id: Option<i64>,
    pub source_run_id: Option<String>,
    pub worker_name: Option<String>,
    pub agent_spec_version: Option<String>,
}

pub struct AssistantInsightInsert<'a> {
    pub account_id: &'a str,
    pub insight_type: &'a str,
    pub severity: &'a str,
    pub message: &'a str,
    pub related_message_id: Option<&'a str>,
    pub related_folder: Option<&'a str>,
    pub metadata: Value,
}

impl Database {
    pub async fn list_pending_subagent_tasks_without_active_core_work_for_account(
        &self,
        account_id: &str,
        limit: i64,
    ) -> Result<Vec<SubagentTaskRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT
                task_id, task_kind, worker_name, skill_bundle, message_ids,
                input_context, priority, correlation_id, created_by
            FROM subagent_tasks task
            WHERE task.account_id = $1
              AND task.status = 'pending'
              AND NOT EXISTS (
                  SELECT 1
                  FROM core_work_queue work
                  WHERE work.account_id = task.account_id
                    AND work.work_type = 'subagent_task'
                    AND work.idempotency_key = task.task_id
                    AND work.status IN ('pending', 'failed', 'processing')
              )
            ORDER BY task.priority DESC, task.created_at ASC
            LIMIT $2
            "#,
        )
        .bind(account_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await
        .context("list_pending_subagent_tasks_without_active_core_work")?;

        Ok(rows
            .into_iter()
            .map(|row| SubagentTaskRecord {
                task_id: row.get("task_id"),
                task_kind: row.get("task_kind"),
                worker_name: row.get("worker_name"),
                skill_bundle: row.get("skill_bundle"),
                message_ids: row.get("message_ids"),
                input_context: row.get::<Json<Value>, _>("input_context").0,
                priority: row.get("priority"),
                correlation_id: row.get("correlation_id"),
                created_by: row.get("created_by"),
            })
            .collect())
    }

    pub async fn upsert_subagent_task_for_account(
        &self,
        account_id: &str,
        task: &SubagentTaskRecord,
        core_work_id: Option<i64>,
        status: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            INSERT INTO subagent_tasks (
                account_id, task_id, task_kind, worker_name, skill_bundle,
                message_ids, input_context, priority, correlation_id, created_by,
                status, core_work_id, started_at, updated_at
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, CASE WHEN $11 = 'running' THEN NOW() ELSE NULL END, NOW()
            )
            ON CONFLICT (account_id, task_id) DO UPDATE SET
                task_kind = EXCLUDED.task_kind,
                worker_name = EXCLUDED.worker_name,
                skill_bundle = EXCLUDED.skill_bundle,
                message_ids = EXCLUDED.message_ids,
                input_context = EXCLUDED.input_context,
                priority = EXCLUDED.priority,
                correlation_id = EXCLUDED.correlation_id,
                created_by = EXCLUDED.created_by,
                status = EXCLUDED.status,
                core_work_id = COALESCE(EXCLUDED.core_work_id, subagent_tasks.core_work_id),
                started_at = CASE
                    WHEN EXCLUDED.status = 'running' THEN COALESCE(subagent_tasks.started_at, NOW())
                    WHEN EXCLUDED.status = 'pending' THEN NULL
                    ELSE subagent_tasks.started_at
                END,
                finished_at = CASE
                    WHEN EXCLUDED.status IN ('pending', 'running') THEN NULL
                    WHEN EXCLUDED.status IN ('completed', 'failed', 'cancelled') THEN NOW()
                    ELSE subagent_tasks.finished_at
                END,
                error = CASE
                    WHEN EXCLUDED.status IN ('pending', 'running') THEN NULL
                    ELSE subagent_tasks.error
                END,
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(&task.task_id)
        .bind(&task.task_kind)
        .bind(&task.worker_name)
        .bind(&task.skill_bundle)
        .bind(&task.message_ids)
        .bind(Json(task.input_context.clone()))
        .bind(task.priority)
        .bind(&task.correlation_id)
        .bind(&task.created_by)
        .bind(status)
        .bind(core_work_id)
        .execute(&self.pool)
        .await
        .context("upsert_subagent_task")?;
        Ok(result.rows_affected())
    }

    pub async fn mark_subagent_task_finished_for_account(
        &self,
        account_id: &str,
        task_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE subagent_tasks
            SET status = $3,
                error = $4,
                finished_at = NOW(),
                updated_at = NOW()
            WHERE account_id = $1
              AND task_id = $2
            "#,
        )
        .bind(account_id)
        .bind(task_id)
        .bind(status)
        .bind(error)
        .execute(&self.pool)
        .await
        .context("mark_subagent_task_finished")?;
        Ok(result.rows_affected())
    }

    pub async fn insert_subagent_result_for_account(
        &self,
        account_id: &str,
        result: &SubagentResultRecord,
    ) -> Result<i64> {
        let row = sqlx::query(
            r#"
            INSERT INTO subagent_results (
                account_id, task_id, worker_name, task_kind, result_json,
                confidence, evidence, recommended_actions, requires_review, agent_run_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            RETURNING result_id
            "#,
        )
        .bind(account_id)
        .bind(&result.task_id)
        .bind(&result.worker_name)
        .bind(&result.task_kind)
        .bind(Json(result.result_json.clone()))
        .bind(result.confidence)
        .bind(Json(result.evidence.clone()))
        .bind(Json(result.recommended_actions.clone()))
        .bind(result.requires_review)
        .bind(result.agent_run_id.as_deref())
        .fetch_one(&self.pool)
        .await
        .context("insert_subagent_result")?;
        Ok(row.get("result_id"))
    }

    pub async fn upsert_subagent_skill_lesson_for_account(
        &self,
        account_id: &str,
        lesson: &SubagentSkillLessonRecord,
    ) -> Result<u64> {
        let promotion_support = env::var("SUBAGENT_SKILL_LESSON_PROMOTION_SUPPORT")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(2)
            .max(2);
        let status = if lesson.status == "active" && lesson.support_count >= promotion_support {
            "active"
        } else {
            "candidate"
        };
        let result = sqlx::query(
            r#"
            INSERT INTO subagent_skill_lessons (
                account_id, skill_bundle, lesson_key, lesson_type,
                status, summary, evidence, score, support_count, negative_count,
                source_task_id, source_result_id, source_run_id, worker_name,
                agent_spec_version, promoted_at, last_seen_at
            )
            VALUES (
                $1, $2, $3, $4,
                CASE
                    WHEN $5 = 'active' AND GREATEST($9, 1) >= $16 AND GREATEST($10, 0) = 0 THEN 'active'
                    ELSE 'candidate'
                END,
                $6, $7, $8, GREATEST($9, 1), GREATEST($10, 0),
                $11, $12, $13, $14,
                $15,
                CASE
                    WHEN $5 = 'active' AND GREATEST($9, 1) >= $16 AND GREATEST($10, 0) = 0 THEN NOW()
                    ELSE NULL
                END,
                NOW()
            )
            ON CONFLICT (account_id, skill_bundle, lesson_key) DO UPDATE SET
                lesson_type = EXCLUDED.lesson_type,
                status = CASE
                    WHEN subagent_skill_lessons.status IN ('paused', 'superseded', 'discarded')
                    THEN subagent_skill_lessons.status
                    WHEN subagent_skill_lessons.negative_count + EXCLUDED.negative_count > 0
                    THEN 'candidate'
                    WHEN subagent_skill_lessons.support_count + EXCLUDED.support_count >= $16
                    THEN 'active'
                    ELSE 'candidate'
                END,
                summary = EXCLUDED.summary,
                evidence = CASE
                    WHEN jsonb_typeof(EXCLUDED.evidence) = 'array'
                     AND jsonb_array_length(EXCLUDED.evidence) = 0
                    THEN subagent_skill_lessons.evidence
                    ELSE EXCLUDED.evidence
                END,
                score = COALESCE(EXCLUDED.score, subagent_skill_lessons.score),
                support_count = subagent_skill_lessons.support_count + EXCLUDED.support_count,
                negative_count = subagent_skill_lessons.negative_count + EXCLUDED.negative_count,
                source_task_id = COALESCE(EXCLUDED.source_task_id, subagent_skill_lessons.source_task_id),
                source_result_id = COALESCE(EXCLUDED.source_result_id, subagent_skill_lessons.source_result_id),
                source_run_id = COALESCE(EXCLUDED.source_run_id, subagent_skill_lessons.source_run_id),
                worker_name = COALESCE(EXCLUDED.worker_name, subagent_skill_lessons.worker_name),
                agent_spec_version = COALESCE(EXCLUDED.agent_spec_version, subagent_skill_lessons.agent_spec_version),
                promoted_at = CASE
                    WHEN subagent_skill_lessons.promoted_at IS NULL
                     AND subagent_skill_lessons.status <> 'active'
                     AND subagent_skill_lessons.status NOT IN ('paused', 'superseded', 'discarded')
                     AND subagent_skill_lessons.negative_count + EXCLUDED.negative_count = 0
                     AND subagent_skill_lessons.support_count + EXCLUDED.support_count >= $16
                    THEN NOW()
                    ELSE subagent_skill_lessons.promoted_at
                END,
                last_seen_at = NOW(),
                updated_at = NOW()
            "#,
        )
        .bind(account_id)
        .bind(&lesson.skill_bundle)
        .bind(&lesson.lesson_key)
        .bind(&lesson.lesson_type)
        .bind(status)
        .bind(&lesson.summary)
        .bind(Json(lesson.evidence.clone()))
        .bind(lesson.score)
        .bind(lesson.support_count.max(1))
        .bind(lesson.negative_count.max(0))
        .bind(lesson.source_task_id.as_deref())
        .bind(lesson.source_result_id)
        .bind(lesson.source_run_id.as_deref())
        .bind(lesson.worker_name.as_deref())
        .bind(lesson.agent_spec_version.as_deref())
        .bind(promotion_support)
        .execute(&self.pool)
        .await
        .context("upsert_subagent_skill_lesson")?;
        Ok(result.rows_affected())
    }

    pub async fn list_active_subagent_skill_lessons_for_account(
        &self,
        account_id: &str,
        skill_bundle: &str,
        limit: i64,
    ) -> Result<Vec<SubagentSkillLessonRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT
                skill_bundle, lesson_key, lesson_type, status, summary, evidence,
                score, support_count, negative_count, source_task_id, source_result_id,
                source_run_id, worker_name, agent_spec_version
            FROM subagent_skill_lessons
            WHERE account_id = $1
              AND skill_bundle = $2
              AND status = 'active'
            ORDER BY support_count DESC, updated_at DESC
            LIMIT $3
            "#,
        )
        .bind(account_id)
        .bind(skill_bundle)
        .bind(limit.clamp(1, 50))
        .fetch_all(&self.pool)
        .await
        .context("list_active_subagent_skill_lessons")?;

        let lesson_keys = rows
            .iter()
            .map(|row| row.get::<String, _>("lesson_key"))
            .collect::<Vec<_>>();
        if !lesson_keys.is_empty() {
            sqlx::query(
                r#"
                UPDATE subagent_skill_lessons
                SET last_used_at = NOW()
                WHERE account_id = $1
                  AND skill_bundle = $2
                  AND lesson_key = ANY($3)
                "#,
            )
            .bind(account_id)
            .bind(skill_bundle)
            .bind(&lesson_keys)
            .execute(&self.pool)
            .await
            .context("mark_active_subagent_skill_lessons_used")?;
        }

        Ok(rows
            .into_iter()
            .map(|row| SubagentSkillLessonRecord {
                skill_bundle: row.get("skill_bundle"),
                lesson_key: row.get("lesson_key"),
                lesson_type: row.get("lesson_type"),
                status: row.get("status"),
                summary: row.get("summary"),
                evidence: row.get::<Json<Value>, _>("evidence").0,
                score: row.get("score"),
                support_count: row.get("support_count"),
                negative_count: row.get("negative_count"),
                source_task_id: row.get("source_task_id"),
                source_result_id: row.get("source_result_id"),
                source_run_id: row.get("source_run_id"),
                worker_name: row.get("worker_name"),
                agent_spec_version: row.get("agent_spec_version"),
            })
            .collect())
    }

    pub async fn insert_assistant_insight_for_account(
        &self,
        insight: AssistantInsightInsert<'_>,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            INSERT INTO assistant_insights (
                account_id, insight_type, severity, message,
                related_message_id, related_folder, metadata
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(insight.account_id)
        .bind(insight.insight_type)
        .bind(insight.severity)
        .bind(insight.message)
        .bind(insight.related_message_id)
        .bind(insight.related_folder)
        .bind(Json(insight.metadata))
        .execute(&self.pool)
        .await
        .context("insert_assistant_insight")?;
        Ok(result.rows_affected())
    }
}
