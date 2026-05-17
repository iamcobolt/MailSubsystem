use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{types::Json, Postgres, QueryBuilder, Row};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::json_column_or_default;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct ScratchpadEntry {
    pub account_id: String,
    pub agent_name: String,
    pub key: String,
    pub value: Value,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScratchpadStats {
    pub agent_name: String,
    pub key_count: i64,
    pub total_size_bytes: i64,
    pub oldest_entry: Option<DateTime<Utc>>,
    pub newest_entry: Option<DateTime<Utc>>,
}

impl Database {
    pub async fn list_scratchpad_entries(
        &self,
        account_id: &str,
        agent_name: Option<&str>,
        key: Option<&str>,
    ) -> Result<Vec<ScratchpadEntry>> {
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT account_id, agent_name, key, value, updated_at, expires_at FROM agent_state WHERE account_id = ",
        );
        query.push_bind(account_id);
        query.push(" AND (expires_at IS NULL OR expires_at > NOW())");

        if let Some(agent_name) = agent_name {
            query.push(" AND agent_name = ");
            query.push_bind(agent_name);
        }
        if let Some(key) = key {
            query.push(" AND key = ");
            query.push_bind(key);
        }

        query.push(" ORDER BY agent_name ASC, key ASC");

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .context("list_scratchpad_entries")?;

        Ok(rows
            .iter()
            .map(|row| ScratchpadEntry {
                account_id: row.get("account_id"),
                agent_name: row.get("agent_name"),
                key: row.get("key"),
                value: json_column_or_default(row, "value", Value::Null),
                updated_at: row.get("updated_at"),
                expires_at: row.get("expires_at"),
            })
            .collect())
    }

    pub async fn get_scratchpad_entry(&self, agent_name: &str, key: &str) -> Result<Option<Value>> {
        self.get_scratchpad_entry_for_account(DEFAULT_ACCOUNT_ID, agent_name, key)
            .await
    }

    pub async fn get_scratchpad_entry_for_account(
        &self,
        account_id: &str,
        agent_name: &str,
        key: &str,
    ) -> Result<Option<Value>> {
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
        .bind(account_id)
        .bind(agent_name)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .context("get_scratchpad_entry_for_account")?;

        Ok(row.map(|row| {
            row.try_get::<Json<Value>, _>("value")
                .map(|json| json.0)
                .unwrap_or(Value::Null)
        }))
    }

    pub async fn get_scratchpad_stats(&self) -> Result<Vec<ScratchpadStats>> {
        self.get_scratchpad_stats_for_account(DEFAULT_ACCOUNT_ID)
            .await
    }

    pub async fn get_scratchpad_stats_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<ScratchpadStats>> {
        let rows = sqlx::query(
            r#"
            SELECT
                agent_name,
                COUNT(*)::bigint AS key_count,
                COALESCE(SUM(octet_length(value::text)), 0)::bigint AS total_size_bytes,
                MIN(updated_at) AS oldest_entry,
                MAX(updated_at) AS newest_entry
            FROM agent_state
            WHERE account_id = $1
              AND (expires_at IS NULL OR expires_at > NOW())
            GROUP BY agent_name
            ORDER BY agent_name ASC
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .context("get_scratchpad_stats_for_account")?;

        Ok(rows
            .iter()
            .map(|row| ScratchpadStats {
                agent_name: row.get("agent_name"),
                key_count: row.get("key_count"),
                total_size_bytes: row.get("total_size_bytes"),
                oldest_entry: row.get("oldest_entry"),
                newest_entry: row.get("newest_entry"),
            })
            .collect())
    }

    pub async fn update_scratchpad_entry(
        &self,
        agent_name: &str,
        key: &str,
        value: &Value,
    ) -> Result<u64> {
        self.update_scratchpad_entry_for_account(DEFAULT_ACCOUNT_ID, agent_name, key, value)
            .await
    }

    pub async fn update_scratchpad_entry_for_account(
        &self,
        account_id: &str,
        agent_name: &str,
        key: &str,
        value: &Value,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE agent_state
            SET value = $4,
                updated_at = NOW()
            WHERE account_id = $1
              AND agent_name = $2
              AND key = $3
            "#,
        )
        .bind(account_id)
        .bind(agent_name)
        .bind(key)
        .bind(Json(value.clone()))
        .execute(&self.pool)
        .await
        .context("update_scratchpad_entry_for_account")?;
        Ok(result.rows_affected())
    }

    pub async fn delete_scratchpad_entry(
        &self,
        account_id: &str,
        agent_name: &str,
        key: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM agent_state WHERE account_id = $1 AND agent_name = $2 AND key = $3",
        )
        .bind(account_id)
        .bind(agent_name)
        .bind(key)
        .execute(&self.pool)
        .await
        .context("delete_scratchpad_entry")?;
        Ok(result.rows_affected() > 0)
    }
}
