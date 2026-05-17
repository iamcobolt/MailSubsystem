use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::Row;

use crate::ai::Message;
use crate::db::Database;

use super::spec::AgentSpec;

#[derive(Debug, Clone)]
pub struct AgentState {
    db: Arc<Database>,
    account_id: String,
    agent_name: String,
}

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub run_id: String,
    pub step: usize,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMessage {
    role: String,
    content: String,
}

impl AgentState {
    pub fn new(
        db: Arc<Database>,
        account_id: impl Into<String>,
        agent_name: impl Into<String>,
    ) -> Self {
        Self {
            db,
            account_id: account_id.into(),
            agent_name: agent_name.into(),
        }
    }

    pub async fn begin_run(&self, spec: &AgentSpec, task_id: &str) -> Result<String> {
        let existing = sqlx::query_scalar::<_, String>(
            r#"
            SELECT run_id
            FROM agent_runs
            WHERE account_id = $1
              AND task_id = $2
              AND agent_name = $3
              AND status = 'running'
            ORDER BY started_at DESC
            LIMIT 1
            "#,
        )
        .bind(&self.account_id)
        .bind(task_id)
        .bind(&self.agent_name)
        .fetch_optional(&self.db.pool)
        .await
        .context("query existing running agent run")?;

        if let Some(run_id) = existing {
            return Ok(run_id);
        }

        let run_id = build_run_id();
        let version = if spec.version.trim().is_empty() {
            None
        } else {
            Some(spec.version.as_str())
        };

        sqlx::query(
            r#"
            INSERT INTO agent_runs (run_id, account_id, agent_name, agent_version, task_id, status)
            VALUES ($1, $2, $3, $4, $5, 'running')
            "#,
        )
        .bind(&run_id)
        .bind(&self.account_id)
        .bind(&self.agent_name)
        .bind(version)
        .bind(task_id)
        .execute(&self.db.pool)
        .await
        .context("insert agent run")?;

        Ok(run_id)
    }

    pub async fn record_step(&self, run_id: &str) -> Result<()> {
        sqlx::query("UPDATE agent_runs SET steps = steps + 1 WHERE run_id = $1")
            .bind(run_id)
            .execute(&self.db.pool)
            .await
            .context("record agent step")?;
        Ok(())
    }

    pub async fn record_llm_call(
        &self,
        run_id: &str,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE agent_runs
            SET llm_calls = llm_calls + 1,
                input_tokens = CASE
                    WHEN $2 IS NULL THEN input_tokens
                    ELSE COALESCE(input_tokens, 0) + $2
                END,
                output_tokens = CASE
                    WHEN $3 IS NULL THEN output_tokens
                    ELSE COALESCE(output_tokens, 0) + $3
                END
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .bind(input_tokens.map(i64::from))
        .bind(output_tokens.map(i64::from))
        .execute(&self.db.pool)
        .await
        .context("record agent llm call")?;
        Ok(())
    }

    pub async fn record_tool_calls(&self, run_id: &str, count: usize) -> Result<()> {
        let count_i64 = i64::try_from(count).unwrap_or(i64::MAX);
        sqlx::query("UPDATE agent_runs SET tool_calls = tool_calls + $2 WHERE run_id = $1")
            .bind(run_id)
            .bind(count_i64)
            .execute(&self.db.pool)
            .await
            .context("record agent tool calls")?;
        Ok(())
    }

    pub async fn save_checkpoint(
        &self,
        run_id: &str,
        step: usize,
        messages: &[Message],
    ) -> Result<()> {
        let stored_messages: Vec<StoredMessage> = messages
            .iter()
            .map(|message| StoredMessage {
                role: message.role.clone(),
                content: message.content.clone(),
            })
            .collect();
        let step_i64 = i64::try_from(step).unwrap_or(i64::MAX);

        sqlx::query(
            r#"
            INSERT INTO agent_checkpoints (run_id, step, messages)
            VALUES ($1, $2, $3)
            ON CONFLICT (run_id, step)
            DO UPDATE SET
                messages = EXCLUDED.messages,
                created_at = NOW()
            "#,
        )
        .bind(run_id)
        .bind(step_i64)
        .bind(Json(stored_messages))
        .execute(&self.db.pool)
        .await
        .context("save agent checkpoint")?;
        Ok(())
    }

    pub async fn latest_checkpoint(&self, run_id: &str) -> Result<Option<Checkpoint>> {
        let row = sqlx::query(
            r#"
            SELECT run_id, step, messages
            FROM agent_checkpoints
            WHERE run_id = $1
            ORDER BY step DESC
            LIMIT 1
            "#,
        )
        .bind(run_id)
        .fetch_optional(&self.db.pool)
        .await
        .context("load latest checkpoint")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let stored: Json<Vec<StoredMessage>> = row
            .try_get("messages")
            .context("deserialize checkpoint messages")?;
        let messages = stored
            .0
            .into_iter()
            .map(|message| Message {
                role: message.role,
                content: message.content,
            })
            .collect();

        Ok(Some(Checkpoint {
            run_id: row.get("run_id"),
            step: row.get::<i32, _>("step").try_into().unwrap_or_default(),
            messages,
        }))
    }

    pub async fn log_tool_call(
        &self,
        run_id: &str,
        step: usize,
        tool_name: &str,
        arguments: &Value,
        result: &str,
        latency_ms: u64,
    ) -> Result<()> {
        let step_i64 = i64::try_from(step).unwrap_or(i64::MAX);
        let latency_i64 = i64::try_from(latency_ms).unwrap_or(i64::MAX);
        let truncated = truncate_chars(result, 4_000);

        sqlx::query(
            r#"
            INSERT INTO agent_tool_log (run_id, step, tool_name, arguments, result, latency_ms)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(run_id)
        .bind(step_i64)
        .bind(tool_name)
        .bind(Json(arguments.clone()))
        .bind(truncated)
        .bind(latency_i64)
        .execute(&self.db.pool)
        .await
        .context("insert agent tool log")?;
        Ok(())
    }

    pub async fn finish_run(
        &self,
        run_id: &str,
        started_at: Instant,
        result: &Value,
        escalated: bool,
        output_confidence: Option<f32>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE agent_runs
            SET status = 'completed',
                result = $2,
                error = NULL,
                finished_at = NOW(),
                duration_ms = $3,
                escalated = $4,
                output_confidence = $5
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .bind(Json(result.clone()))
        .bind(duration_ms(started_at))
        .bind(escalated)
        .bind(output_confidence)
        .execute(&self.db.pool)
        .await
        .context("finish agent run")?;
        Ok(())
    }

    pub async fn fail_run(&self, run_id: &str, started_at: Instant, error: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE agent_runs
            SET status = 'failed',
                error = $2,
                finished_at = NOW(),
                duration_ms = $3
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .bind(error)
        .bind(duration_ms(started_at))
        .execute(&self.db.pool)
        .await
        .context("mark agent run failed")?;
        Ok(())
    }

    pub async fn timeout_run(&self, run_id: &str, started_at: Instant) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE agent_runs
            SET status = 'timed_out',
                error = 'timed_out',
                finished_at = NOW(),
                duration_ms = $2
            WHERE run_id = $1
            "#,
        )
        .bind(run_id)
        .bind(duration_ms(started_at))
        .execute(&self.db.pool)
        .await
        .context("mark agent run timed out")?;
        Ok(())
    }

    pub async fn read_scratchpad(&self, key: &str) -> Result<Option<Value>> {
        let row = sqlx::query(
            r#"
            SELECT value
            FROM agent_state
            WHERE account_id = $1
              AND agent_name = $2
              AND key = $3
              AND (expires_at IS NULL OR expires_at > NOW())
            "#,
        )
        .bind(&self.account_id)
        .bind(&self.agent_name)
        .bind(key)
        .fetch_optional(&self.db.pool)
        .await
        .context("read scratchpad value")?;

        match row {
            Some(row) => {
                let value: Json<Value> = row.try_get("value").context("deserialize scratchpad")?;
                Ok(Some(value.0))
            }
            None => Ok(None),
        }
    }

    pub async fn write_scratchpad(
        &self,
        key: &str,
        value: Value,
        ttl_hours: Option<u64>,
    ) -> Result<()> {
        let expires_at = ttl_to_expiry(ttl_hours);

        sqlx::query(
            r#"
            INSERT INTO agent_state (account_id, agent_name, key, value, updated_at, expires_at)
            VALUES ($1, $2, $3, $4, NOW(), $5)
            ON CONFLICT (account_id, agent_name, key)
            DO UPDATE SET
                value = EXCLUDED.value,
                updated_at = NOW(),
                expires_at = EXCLUDED.expires_at
            "#,
        )
        .bind(&self.account_id)
        .bind(&self.agent_name)
        .bind(key)
        .bind(Json(value))
        .bind(expires_at)
        .execute(&self.db.pool)
        .await
        .context("write scratchpad value")?;
        Ok(())
    }

    pub async fn prune_expired_scratchpad(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM agent_state
            WHERE account_id = $1
              AND agent_name = $2
              AND expires_at IS NOT NULL
              AND expires_at < NOW()
            "#,
        )
        .bind(&self.account_id)
        .bind(&self.agent_name)
        .execute(&self.db.pool)
        .await
        .context("prune expired scratchpad")?;
        Ok(result.rows_affected())
    }
}

fn build_run_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn duration_ms(started_at: Instant) -> i32 {
    started_at.elapsed().as_millis().min(i32::MAX as u128) as i32
}

fn ttl_to_expiry(ttl_hours: Option<u64>) -> Option<DateTime<Utc>> {
    ttl_hours.map(|hours| {
        let capped_hours = hours.min((i64::MAX as u64) / 3600);
        Utc::now() + ChronoDuration::hours(capped_hours as i64)
    })
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::db::Database;
    use crate::harness::spec::{
        BudgetConfig, EscalationConfig, ExecutionConfig, FieldValidation, OutputConfig,
        ProviderConfig, ProviderTier, StateConfig,
    };

    fn minimal_spec() -> AgentSpec {
        let mut validation = std::collections::HashMap::new();
        validation.insert(
            "status".to_string(),
            FieldValidation {
                field_type: Some("string".to_string()),
                ..FieldValidation::default()
            },
        );

        AgentSpec {
            name: "test-agent".to_string(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            skills: Vec::new(),
            system_prompt: "Return JSON".to_string(),
            provider: ProviderConfig {
                tier: ProviderTier::Worker,
                prefer: "local".to_string(),
                fallback: "frontier".to_string(),
            },
            budget: BudgetConfig {
                max_llm_calls: 2,
                max_tool_calls: 1,
            },
            execution: ExecutionConfig {
                max_iterations: 2,
                timeout_secs: 5,
                temperature: 0.0,
                max_output_tokens: 512,
                checkpoint_every: 1,
            },
            output: OutputConfig {
                required_fields: vec!["status".to_string()],
                validation,
            },
            state: StateConfig {
                ttl_hours: 24,
                schema: Vec::new(),
            },
            escalation: EscalationConfig {
                confidence_threshold: 0.75,
                always_escalate_on_phishing: false,
                always_escalate_on_threat: Vec::new(),
            },
        }
    }

    async fn load_test_database() -> Option<Arc<Database>> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())?;
        let db = Database::new(&url).await.ok()?;
        let _ = sqlx::raw_sql(include_str!("../../schema.sql"))
            .execute(&db.pool)
            .await;
        Some(Arc::new(db))
    }

    #[tokio::test]
    #[ignore]
    async fn test_mark_run_escalated_sets_flag() {
        let Some(db) = load_test_database().await else {
            eprintln!(
                "Skipping agent state escalation test (no TEST_DATABASE_URL or DATABASE_URL)"
            );
            return;
        };

        let spec = minimal_spec();
        let state = AgentState::new(db.clone(), "default", spec.name.clone());
        let run_id = state
            .begin_run(&spec, "mark-escalated-task")
            .await
            .expect("begin run");

        state
            .finish_run(
                &run_id,
                std::time::Instant::now(),
                &serde_json::json!({}),
                true,
                None,
            )
            .await
            .expect("finish run with escalated=true");

        let escalated =
            sqlx::query_scalar::<_, bool>("SELECT escalated FROM agent_runs WHERE run_id = $1")
                .bind(&run_id)
                .fetch_one(&db.pool)
                .await
                .expect("fetch escalated flag");
        assert!(escalated);

        let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = $1")
            .bind(&run_id)
            .execute(&db.pool)
            .await;
    }
}
