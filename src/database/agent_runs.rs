use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::{agent_run_summary_from_row, json_column, json_column_or_default};
use crate::db::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
}

impl AgentRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }
}

impl FromStr for AgentRunStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "timed_out" => Ok(Self::TimedOut),
            _ => anyhow::bail!(
                "invalid agent run status '{}'; expected one of: running, completed, failed, timed_out",
                value
            ),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentRunSummary {
    pub run_id: String,
    pub agent_name: String,
    pub task_id: String,
    pub status: String,
    pub steps: i32,
    pub llm_calls: i32,
    pub tool_calls: i32,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub error: Option<String>,
    pub escalated: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentToolLogEntry {
    pub step: i32,
    pub tool_name: String,
    pub arguments: Value,
    pub result: String,
    pub latency_ms: i64,
    pub called_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentRunDetail {
    pub summary: AgentRunSummary,
    pub account_id: String,
    pub agent_version: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result: Option<Value>,
    pub tool_log: Vec<AgentToolLogEntry>,
}

#[derive(Debug, Clone)]
pub struct AgentRunStats {
    pub agent_name: String,
    pub completed: i64,
    pub failed: i64,
    pub timed_out: i64,
    pub avg_steps: f64,
    pub avg_tokens: f64,
    pub avg_duration_ms: f64,
    pub escalation_rate: f64,
}

#[derive(Debug, Clone)]
pub struct ConfidenceStats {
    pub agent_name: String,
    pub sample_count: i64,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
    pub escalation_rate: f64,
}

impl Database {
    pub async fn list_agent_runs(
        &self,
        limit: usize,
        status: Option<AgentRunStatus>,
        agent_name: Option<&str>,
    ) -> Result<Vec<AgentRunSummary>> {
        self.list_agent_runs_for_account(DEFAULT_ACCOUNT_ID, limit, status, agent_name)
            .await
    }

    pub async fn list_agent_runs_for_account(
        &self,
        account_id: &str,
        limit: usize,
        status: Option<AgentRunStatus>,
        agent_name: Option<&str>,
    ) -> Result<Vec<AgentRunSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT run_id, agent_name, task_id, status, steps, llm_calls, tool_calls, input_tokens, output_tokens, duration_ms, started_at, error, escalated FROM agent_runs WHERE account_id = ",
        );
        query.push_bind(account_id);

        if let Some(status) = status {
            query.push(" AND ");
            query.push("status = ");
            query.push_bind(status.as_str());
        }
        if let Some(agent_name) = agent_name {
            query.push(" AND ");
            query.push("agent_name = ");
            query.push_bind(agent_name);
        }

        query.push(" ORDER BY started_at DESC LIMIT ");
        query.push_bind(limit as i64);

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .context("list_agent_runs_for_account")?;
        Ok(rows.iter().map(agent_run_summary_from_row).collect())
    }

    pub async fn get_agent_run(&self, run_id: &str) -> Result<Option<AgentRunDetail>> {
        let row = sqlx::query(
            r#"
            SELECT
                run_id,
                account_id,
                agent_name,
                agent_version,
                task_id,
                status,
                steps,
                llm_calls,
                tool_calls,
                input_tokens,
                output_tokens,
                duration_ms,
                started_at,
                finished_at,
                result,
                error,
                escalated
            FROM agent_runs
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_agent_run")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let tool_rows = sqlx::query(
            r#"
            SELECT step, tool_name, arguments, result, latency_ms, created_at
            FROM agent_tool_log
            WHERE run_id = $1
            ORDER BY step ASC, id ASC
            "#,
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .context("get_agent_run tool_log")?;

        let summary = agent_run_summary_from_row(&row);
        let tool_log = tool_rows
            .iter()
            .map(|tool_row| AgentToolLogEntry {
                step: tool_row.get("step"),
                tool_name: tool_row.get("tool_name"),
                arguments: json_column_or_default(tool_row, "arguments", Value::Null),
                result: tool_row
                    .get::<Option<String>, _>("result")
                    .unwrap_or_default(),
                latency_ms: tool_row
                    .try_get::<Option<i32>, _>("latency_ms")
                    .ok()
                    .flatten()
                    .map(i64::from)
                    .unwrap_or_default(),
                called_at: tool_row.get("created_at"),
            })
            .collect();

        Ok(Some(AgentRunDetail {
            summary,
            account_id: row.get("account_id"),
            agent_version: row.get("agent_version"),
            finished_at: row.get("finished_at"),
            result: json_column(&row, "result"),
            tool_log,
        }))
    }

    pub async fn get_agent_run_for_account(
        &self,
        account_id: &str,
        run_id: &str,
    ) -> Result<Option<AgentRunDetail>> {
        Ok(self
            .get_agent_run(run_id)
            .await?
            .filter(|detail| detail.account_id == account_id))
    }

    pub async fn get_latest_agent_run_for_task(
        &self,
        account_id: &str,
        task_id: &str,
    ) -> Result<Option<AgentRunDetail>> {
        let row = sqlx::query(
            r#"
            SELECT run_id
            FROM agent_runs
            WHERE account_id = $1
              AND task_id = $2
            ORDER BY started_at DESC
            LIMIT 1
            "#,
        )
        .bind(account_id)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_latest_agent_run_for_task")?;

        let Some(row) = row else {
            return Ok(None);
        };
        let run_id: String = row.get("run_id");
        self.get_agent_run(&run_id).await
    }

    pub async fn get_confidence_stats(
        &self,
        agent_name: &str,
        sample_limit: i64,
    ) -> Result<Option<ConfidenceStats>> {
        self.get_confidence_stats_for_account(DEFAULT_ACCOUNT_ID, agent_name, sample_limit)
            .await
    }

    pub async fn get_confidence_stats_for_account(
        &self,
        account_id: &str,
        agent_name: &str,
        sample_limit: i64,
    ) -> Result<Option<ConfidenceStats>> {
        let row = sqlx::query(
            r#"
            WITH recent AS (
                SELECT output_confidence, escalated
                FROM agent_runs
                WHERE account_id = $1
                  AND agent_name = $2
                  AND status = 'completed'
                  AND output_confidence IS NOT NULL
                ORDER BY started_at DESC
                LIMIT $3
            )
            SELECT
                COUNT(*)::bigint AS sample_count,
                PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY output_confidence) AS p50,
                PERCENTILE_CONT(0.75) WITHIN GROUP (ORDER BY output_confidence) AS p75,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY output_confidence) AS p95,
                COALESCE(
                    SUM(CASE WHEN escalated THEN 1 ELSE 0 END)::double precision
                    / NULLIF(COUNT(*), 0)::double precision,
                    0
                ) AS escalation_rate
            FROM recent
            "#,
        )
        .bind(account_id)
        .bind(agent_name)
        .bind(sample_limit)
        .fetch_one(&self.pool)
        .await
        .context("get_confidence_stats_for_account")?;

        let sample_count: i64 = row.get("sample_count");
        if sample_count == 0 {
            return Ok(None);
        }

        Ok(Some(ConfidenceStats {
            agent_name: agent_name.to_string(),
            sample_count,
            p50: row.get::<f64, _>("p50"),
            p75: row.get::<f64, _>("p75"),
            p95: row.get::<f64, _>("p95"),
            escalation_rate: row.get::<f64, _>("escalation_rate"),
        }))
    }

    pub async fn get_agent_run_stats(&self, since: DateTime<Utc>) -> Result<Vec<AgentRunStats>> {
        self.get_agent_run_stats_for_account(DEFAULT_ACCOUNT_ID, since)
            .await
    }

    pub async fn get_agent_run_stats_for_account(
        &self,
        account_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<AgentRunStats>> {
        let rows = sqlx::query(
            r#"
            SELECT
                agent_name,
                COUNT(*) FILTER (WHERE status = 'completed') AS completed,
                COUNT(*) FILTER (WHERE status = 'failed') AS failed,
                COUNT(*) FILTER (WHERE status = 'timed_out') AS timed_out,
                COALESCE(AVG(steps::float) FILTER (WHERE status = 'completed'), 0) AS avg_steps,
                COALESCE(
                    AVG((COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0))::float)
                        FILTER (WHERE status = 'completed'),
                    0
                ) AS avg_tokens,
                COALESCE(AVG(duration_ms::float) FILTER (WHERE status = 'completed'), 0) AS avg_duration_ms,
                COALESCE(
                    SUM(CASE WHEN escalated THEN 1 ELSE 0 END)::float
                        / NULLIF(COUNT(*) FILTER (WHERE status = 'completed'), 0),
                    0
                ) AS escalation_rate
            FROM agent_runs
            WHERE account_id = $1
              AND started_at >= $2
            GROUP BY agent_name
            ORDER BY agent_name ASC
            "#,
        )
        .bind(account_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .context("get_agent_run_stats_for_account")?;

        Ok(rows
            .iter()
            .map(|row| AgentRunStats {
                agent_name: row.get("agent_name"),
                completed: row.get("completed"),
                failed: row.get("failed"),
                timed_out: row.get("timed_out"),
                avg_steps: row.get("avg_steps"),
                avg_tokens: row.get("avg_tokens"),
                avg_duration_ms: row.get("avg_duration_ms"),
                escalation_rate: row.get("escalation_rate"),
            })
            .collect())
    }
}
