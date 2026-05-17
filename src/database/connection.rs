use anyhow::{Context, Result};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions, PgPool,
};
use std::time::Duration;

use crate::database::config::{
    mark_schema_migrated, should_run_schema_migration, SchemaMigrationMode,
};

#[derive(Debug, Clone)]
pub struct Database {
    pub(crate) pool: PgPool,
}

impl Database {
    pub async fn new(connection_string: &str) -> Result<Self> {
        let schema_mode = SchemaMigrationMode::from_env()?;
        Self::new_with_schema_mode(connection_string, schema_mode).await
    }

    pub async fn new_with_schema_mode(
        connection_string: &str,
        schema_mode: SchemaMigrationMode,
    ) -> Result<Self> {
        let connect_options = Self::connect_options(connection_string)?;
        let pool_options = PgPoolOptions::new()
            .max_connections(20)
            .min_connections(2)
            .acquire_timeout(Duration::from_secs(30))
            .idle_timeout(Some(Duration::from_secs(600)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .test_before_acquire(true);

        let pool = match pool_options
            .clone()
            .connect_with(connect_options.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("does not exist") || error_msg.contains("database") {
                    if !schema_mode.allows_database_creation() {
                        return Err(e).context(
                            "PostgreSQL database does not exist and schema mode is validate. \
                             Create the database first, run `mailsubsystem migrate-schema --apply`, \
                             or set MAILSUBSYSTEM_SCHEMA_MODE=bootstrap for first-time setup.",
                        );
                    }
                    Self::create_database_if_not_exists(connection_string).await?;
                    pool_options
                        .connect_with(connect_options)
                        .await
                        .context("Failed to connect to PostgreSQL database after creation")?
                } else {
                    return Err(e).context("Failed to connect to PostgreSQL database");
                }
            }
        };

        let db = Database { pool };
        if should_run_schema_migration(connection_string) {
            db.ensure_tables_exist_with_mode(schema_mode)
                .await
                .context("Validate database schema")?;
            mark_schema_migrated(connection_string);
        }
        Ok(db)
    }

    fn connect_options(connection_string: &str) -> Result<PgConnectOptions> {
        let options = connection_string
            .parse::<PgConnectOptions>()
            .context("Failed to parse PostgreSQL connection options")?;
        Ok(options
            .log_statements(log::LevelFilter::Debug)
            .log_slow_statements(log::LevelFilter::Debug, Duration::from_secs(5)))
    }

    async fn create_database_if_not_exists(connection_string: &str) -> Result<()> {
        let db_name = connection_string
            .split('/')
            .next_back()
            .and_then(|s| s.split('?').next())
            .context("Could not parse database name from connection string")?;

        if !db_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            anyhow::bail!(
                "Invalid database name: '{}'. Only alphanumeric characters and underscores are allowed.",
                db_name
            );
        }

        let postgres_conn = connection_string
            .replace(&format!("/{}", db_name), "/postgres")
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(Self::connect_options(&postgres_conn)?)
            .await
            .context("Failed to connect to postgres database")?;

        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
                .bind(db_name)
                .fetch_one(&pool)
                .await
                .context("Failed to check if database exists")?;

        if !exists {
            sqlx::query(&format!(r#"CREATE DATABASE "{}""#, db_name))
                .execute(&pool)
                .await
                .context("Failed to create database. Make sure the database user has CREATE DATABASE permission.")?;
        }
        Ok(())
    }
}
