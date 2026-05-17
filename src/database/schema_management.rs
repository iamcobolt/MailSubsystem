use std::collections::HashSet;
use std::fs;

use anyhow::{Context, Result};
use sqlx::Row;

use crate::database::config::SchemaMigrationMode;
use crate::db::Database;

pub(crate) const EMBEDDED_SCHEMA: &str = include_str!("../../schema.sql");
const SCHEMA_FINGERPRINT_METADATA_KEY: &str = "schema.sql.md5";
const REQUIRED_EMAIL_COLUMNS: &[&str] = &[
    "otp_code",
    "threat_level",
    "threat_indicators",
    "analyzed_at",
    "action_status",
    "action_applied_at",
    "analysis_attempts",
    "analysis_failed_at",
    "analysis_permanent_failure",
    "last_analysis_error",
    "batch_id",
    "reanalysis_requested",
    "reanalysis_reason",
];

struct SchemaReadiness {
    existing_tables: Vec<String>,
    missing_tables: Vec<&'static str>,
    missing_email_columns: Vec<&'static str>,
    stored_fingerprint: Option<String>,
    current_fingerprint: String,
}

impl SchemaReadiness {
    fn is_empty_database(&self) -> bool {
        self.existing_tables.is_empty()
    }

    fn has_required_tables(&self) -> bool {
        self.missing_tables.is_empty()
    }

    fn is_current(&self) -> bool {
        self.has_required_tables()
            && self.missing_email_columns.is_empty()
            && self
                .stored_fingerprint
                .as_deref()
                .map(|fingerprint| fingerprint == self.current_fingerprint)
                .unwrap_or(false)
    }

    fn issue_summary(&self) -> String {
        if self.is_empty_database() {
            return "database has no public tables".to_string();
        }

        let mut issues = Vec::new();
        if !self.missing_tables.is_empty() {
            issues.push(format!(
                "missing tables: {}",
                self.missing_tables.join(", ")
            ));
        }
        if !self.missing_email_columns.is_empty() {
            issues.push(format!(
                "missing email columns: {}",
                self.missing_email_columns.join(", ")
            ));
        }
        match self.stored_fingerprint.as_deref() {
            Some(fingerprint) if fingerprint != self.current_fingerprint => {
                issues.push("schema fingerprint differs from embedded schema.sql".to_string());
            }
            None if self.has_required_tables() && self.missing_email_columns.is_empty() => {
                issues.push("schema fingerprint metadata is missing".to_string());
            }
            _ => {}
        }
        if issues.is_empty() {
            "schema metadata is not current".to_string()
        } else {
            issues.join("; ")
        }
    }
}

impl Database {
    pub async fn ensure_tables_exist(&self) -> Result<bool> {
        self.ensure_tables_exist_with_mode(SchemaMigrationMode::from_env()?)
            .await
    }

    pub async fn ensure_tables_exist_with_mode(
        &self,
        schema_mode: SchemaMigrationMode,
    ) -> Result<bool> {
        let readiness = self.schema_readiness().await?;
        if readiness.is_current() {
            return Ok(true);
        }

        if readiness.is_empty_database() {
            if !schema_mode.allows_empty_database_bootstrap() {
                anyhow::bail!(
                    "Database schema is not initialized ({}). \
                     Schema mode validate never applies schema.sql. \
                     Review schema.sql and run `mailsubsystem migrate-schema --apply`, \
                     or set MAILSUBSYSTEM_SCHEMA_MODE=bootstrap for first-time setup.",
                    readiness.issue_summary()
                );
            }
        } else if !schema_mode.allows_existing_database_migration() {
            anyhow::bail!(
                "Database schema is not current ({}). \
                 Automatic migrations for existing databases are disabled by default. \
                 Review schema.sql and run `mailsubsystem migrate-schema --apply`, \
                 or set MAILSUBSYSTEM_SCHEMA_MODE=auto to allow startup migrations.",
                readiness.issue_summary()
            );
        }

        self.run_migration_file("schema.sql", Some(EMBEDDED_SCHEMA))
            .await
            .context("Failed to run schema migration")?;

        let readiness_after = self.schema_readiness().await?;
        if !readiness_after.has_required_tables() {
            anyhow::bail!(
                "Tables still missing after migration: {:?}",
                readiness_after.missing_tables
            );
        }
        if !readiness_after.missing_email_columns.is_empty() {
            anyhow::bail!(
                "Email columns still missing after migration: {:?}",
                readiness_after.missing_email_columns
            );
        }
        let current_fingerprint = self.embedded_schema_fingerprint().await?;
        self.set_system_metadata(SCHEMA_FINGERPRINT_METADATA_KEY, &current_fingerprint)
            .await?;
        Ok(readiness.has_required_tables())
    }

    async fn schema_readiness(&self) -> Result<SchemaReadiness> {
        let required_tables = required_tables();
        let existing_tables = self.list_tables().await?;
        let existing_set: HashSet<String> =
            existing_tables.iter().map(|s| s.to_lowercase()).collect();
        let missing_tables: Vec<_> = required_tables
            .iter()
            .copied()
            .filter(|table| !existing_set.contains((*table).to_lowercase().as_str()))
            .collect();
        let has_required_tables = missing_tables.is_empty();
        let email_columns = if existing_set.contains("emails") {
            self.list_table_columns("emails").await?
        } else {
            Vec::new()
        };
        let email_column_set: HashSet<String> = email_columns.into_iter().collect();
        let missing_email_columns: Vec<_> = REQUIRED_EMAIL_COLUMNS
            .iter()
            .copied()
            .filter(|column| !email_column_set.contains(*column))
            .collect();
        let current_fingerprint = self.embedded_schema_fingerprint().await?;
        let stored_fingerprint = if has_required_tables {
            self.get_system_metadata(SCHEMA_FINGERPRINT_METADATA_KEY)
                .await?
        } else {
            None
        };

        Ok(SchemaReadiness {
            existing_tables,
            missing_tables,
            missing_email_columns,
            stored_fingerprint,
            current_fingerprint,
        })
    }

    async fn embedded_schema_fingerprint(&self) -> Result<String> {
        sqlx::query_scalar("SELECT md5($1)")
            .bind(schema_fingerprint_source(EMBEDDED_SCHEMA))
            .fetch_one(&self.pool)
            .await
            .context("fingerprint embedded schema")
    }

    async fn run_migration_file(&self, filename: &str, fallback: Option<&str>) -> Result<()> {
        let sql = fs::read_to_string(filename)
            .or_else(|_| {
                fallback
                    .map(String::from)
                    .ok_or_else(|| anyhow::anyhow!("No fallback"))
            })
            .context("Could not load migration file")?;

        // Execute the migration as a whole script. This preserves PL/pgSQL blocks
        // (e.g. DO $$ ... $$) and fails fast on the first SQL error.
        sqlx::raw_sql(&sql)
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to execute migration script: {}", filename))?;
        Ok(())
    }

    pub async fn list_tables(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list tables")?;
        Ok(rows
            .iter()
            .map(|r| r.get::<String, _>("tablename"))
            .collect())
    }

    async fn list_table_columns(&self, table_name: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            SELECT column_name
            FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = $1
            ORDER BY ordinal_position
            "#,
        )
        .bind(table_name)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("list_table_columns({table_name})"))?;
        Ok(rows
            .iter()
            .map(|row| row.get::<String, _>("column_name"))
            .collect())
    }

    pub async fn table_exists(&self, name: &str) -> Result<bool> {
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(name.to_lowercase())
        .fetch_one(&self.pool)
        .await?;
        Ok(count.0 > 0)
    }

    pub(crate) async fn relation_exists(&self, schema_qualified_name: &str) -> Result<bool> {
        sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(schema_qualified_name)
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("check relation exists: {schema_qualified_name}"))
    }

    // ── System metadata ──────────────────────────────────────────────────────

    /// Get a value from the system_metadata key-value table.
    pub async fn get_system_metadata(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM system_metadata WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .context("get_system_metadata")?;
        Ok(row.map(|r| r.0))
    }

    /// Upsert a value into the system_metadata key-value table.
    pub async fn set_system_metadata(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO system_metadata (key, value, updated_at) VALUES ($1, $2, NOW()) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .context("set_system_metadata")?;
        Ok(())
    }
}

fn required_tables() -> &'static [&'static str] {
    &[
        "emails",
        "imap_folders",
        "emails_missing_message_id",
        "frontier_analysis_queue",
        "body_sync_queue",
        "sync_window_runs",
        "agent_runs",
        "agent_state",
        "agent_checkpoints",
        "agent_tool_log",
        "conversation_threads",
        "conversation_messages",
        "otp_codes",
        "analysis_batches",
        "system_metadata",
    ]
}

fn schema_fingerprint_source(schema: &str) -> String {
    schema
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::schema_fingerprint_source;

    #[test]
    fn schema_fingerprint_source_ignores_comment_only_changes() {
        let left = r#"
            -- First header
            CREATE TABLE IF NOT EXISTS example (id INTEGER);

            -- trailing note
        "#;
        let right = r#"
            -- Different header
            CREATE TABLE IF NOT EXISTS example (id INTEGER);
            -- another note
        "#;

        assert_eq!(
            schema_fingerprint_source(left),
            schema_fingerprint_source(right)
        );
    }

    #[test]
    fn schema_fingerprint_source_keeps_statement_changes() {
        let left = "CREATE TABLE IF NOT EXISTS example (id INTEGER);";
        let right = "CREATE TABLE IF NOT EXISTS example (id BIGINT);";

        assert_ne!(
            schema_fingerprint_source(left),
            schema_fingerprint_source(right)
        );
    }
}
