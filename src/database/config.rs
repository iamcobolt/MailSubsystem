use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use url::Url;

fn migrated_connection_cache() -> &'static Mutex<HashSet<String>> {
    static CACHE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn should_run_schema_migration(connection_string: &str) -> bool {
    if std::env::var("MAILSUBSYSTEM_MIGRATE_EVERY_CONNECT")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }

    migrated_connection_cache()
        .lock()
        .map(|cache| !cache.contains(connection_string))
        .unwrap_or(true)
}

pub(crate) fn mark_schema_migrated(connection_string: &str) {
    if let Ok(mut cache) = migrated_connection_cache().lock() {
        cache.insert(connection_string.to_string());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMigrationMode {
    /// Apply schema.sql only when bootstrapping an empty database.
    Bootstrap,
    /// Apply schema.sql whenever the embedded schema is not current.
    Auto,
    /// Validate only; never create or migrate schema objects.
    Validate,
}

impl SchemaMigrationMode {
    pub(crate) fn from_env() -> Result<Self> {
        match env::var("MAILSUBSYSTEM_SCHEMA_MODE") {
            Ok(value) => Self::parse(&value),
            Err(_) => Ok(Self::Bootstrap),
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "bootstrap" => Ok(Self::Bootstrap),
            "auto" | "migrate" | "always" => Ok(Self::Auto),
            "validate" | "manual" | "off" | "none" => Ok(Self::Validate),
            other => anyhow::bail!(
                "Invalid MAILSUBSYSTEM_SCHEMA_MODE '{}'. Expected bootstrap, auto, or validate.",
                other
            ),
        }
    }

    pub(crate) fn allows_database_creation(self) -> bool {
        matches!(self, Self::Bootstrap | Self::Auto)
    }

    pub(crate) fn allows_empty_database_bootstrap(self) -> bool {
        matches!(self, Self::Bootstrap | Self::Auto)
    }

    pub(crate) fn allows_existing_database_migration(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Database connection config. Load from env via `DatabaseConfig::load()`.
#[derive(Clone, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub sslmode: String,
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("sslmode", &self.sslmode)
            .finish()
    }
}

impl DatabaseConfig {
    pub fn connection_string(&self) -> String {
        let user = utf8_percent_encode(&self.user, NON_ALPHANUMERIC).to_string();
        let password = utf8_percent_encode(&self.password, NON_ALPHANUMERIC).to_string();
        let database = utf8_percent_encode(&self.database, NON_ALPHANUMERIC).to_string();
        let sslmode = utf8_percent_encode(&self.sslmode, NON_ALPHANUMERIC).to_string();
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };

        format!(
            "postgresql://{}:{}@{}:{}/{}?sslmode={}",
            user, password, host, self.port, database, sslmode
        )
    }

    /// Load from DATABASE_URL, database.toml, or env vars (DB_HOST, DB_PORT, etc.)
    pub fn load() -> Result<Self> {
        if let Ok(database_url) = env::var("DATABASE_URL") {
            return Self::from_url(&database_url);
        }

        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432);
        let database = env::var("DB_NAME").unwrap_or_else(|_| "mailsubsystem".to_string());
        let user = env::var("DB_USER").unwrap_or_else(|_| "mailsubsystem".to_string());
        let password = env::var("DB_PASSWORD").unwrap_or_default();
        let sslmode = env::var("DB_SSLMODE").unwrap_or_else(|_| "disable".to_string());

        if let Ok(toml_content) = fs::read_to_string("database.toml") {
            if let Ok(toml_config) = toml::from_str::<DatabaseToml>(&toml_content) {
                let mut db_config = toml_config.database;
                if env::var("DB_HOST").is_ok() {
                    db_config.host = host;
                }
                if env::var("DB_PORT").is_ok() {
                    db_config.port = port;
                }
                if env::var("DB_NAME").is_ok() {
                    db_config.database = database;
                }
                if env::var("DB_USER").is_ok() {
                    db_config.user = user;
                }
                if env::var("DB_PASSWORD").is_ok() {
                    db_config.password = password;
                }
                if env::var("DB_SSLMODE").is_ok() {
                    db_config.sslmode = sslmode;
                } else if db_config.password.is_empty() {
                    db_config.password = env::var("DB_PASSWORD").unwrap_or_default();
                }
                return Ok(db_config);
            }
        }

        Ok(DatabaseConfig {
            host,
            port,
            database,
            user,
            password,
            sslmode,
        })
    }

    fn from_url(url: &str) -> Result<Self> {
        let parsed = Url::parse(url).context("Invalid DATABASE_URL format")?;
        if !matches!(parsed.scheme(), "postgresql" | "postgres") {
            anyhow::bail!(
                "Invalid DATABASE_URL: unsupported scheme '{}'",
                parsed.scheme()
            );
        }

        let host = parsed
            .host_str()
            .context("Invalid DATABASE_URL: missing host")?
            .to_string();
        let port = parsed.port().unwrap_or(5432);

        let user = parsed.username();
        if user.is_empty() {
            anyhow::bail!("Invalid DATABASE_URL: missing username");
        }
        let user = percent_decode_str(user)
            .decode_utf8()
            .context("Invalid DATABASE_URL: username is not valid UTF-8")?
            .into_owned();

        let password = parsed.password().unwrap_or_default();
        let password = percent_decode_str(password)
            .decode_utf8()
            .context("Invalid DATABASE_URL: password is not valid UTF-8")?
            .into_owned();

        let path = parsed.path().trim_start_matches('/');
        if path.is_empty() {
            anyhow::bail!("Invalid DATABASE_URL: missing database name");
        }
        if path.contains('/') {
            anyhow::bail!("Invalid DATABASE_URL: database name must be a single path segment");
        }
        let database = percent_decode_str(path)
            .decode_utf8()
            .context("Invalid DATABASE_URL: database name is not valid UTF-8")?
            .into_owned();

        let sslmode = parsed
            .query_pairs()
            .find_map(|(key, value)| {
                if key.eq_ignore_ascii_case("sslmode") {
                    Some(value.into_owned())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "disable".to_string());

        Ok(DatabaseConfig {
            host,
            port,
            database,
            user,
            password,
            sslmode,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DatabaseToml {
    database: DatabaseConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn database_config_debug_redacts_password() {
        let config = DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "mailsubsystem".to_string(),
            user: "mailsubsystem".to_string(),
            password: "db-super-secret".to_string(),
            sslmode: "disable".to_string(),
        };

        let debug = format!("{config:?}");

        assert!(debug.contains("password: \"<redacted>\""));
        assert!(!debug.contains("db-super-secret"));
    }

    #[test]
    fn schema_migration_cache_marks_connection_once() {
        let key = format!(
            "postgres://mailsubsystem:test@localhost/test_{}",
            Uuid::new_v4()
        );
        assert!(should_run_schema_migration(&key));
        mark_schema_migrated(&key);
        assert!(!should_run_schema_migration(&key));
    }

    #[test]
    fn schema_migration_mode_parses_release_safe_defaults() {
        assert_eq!(
            SchemaMigrationMode::parse("").expect("parse empty"),
            SchemaMigrationMode::Bootstrap
        );
        assert_eq!(
            SchemaMigrationMode::parse("bootstrap").expect("parse bootstrap"),
            SchemaMigrationMode::Bootstrap
        );
        assert_eq!(
            SchemaMigrationMode::parse("auto").expect("parse auto"),
            SchemaMigrationMode::Auto
        );
        assert_eq!(
            SchemaMigrationMode::parse("manual").expect("parse manual"),
            SchemaMigrationMode::Validate
        );
        assert!(SchemaMigrationMode::parse("surprise").is_err());
    }

    #[test]
    fn database_config_from_url_parses_standard_dev_url() {
        let config = DatabaseConfig::from_url(
            "postgresql://mailsubsystem:devpassword@localhost:5432/mailsubsystem?sslmode=disable",
        )
        .expect("parse standard dev DATABASE_URL");

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 5432);
        assert_eq!(config.database, "mailsubsystem");
        assert_eq!(config.user, "mailsubsystem");
        assert_eq!(config.password, "devpassword");
        assert_eq!(config.sslmode, "disable");
    }

    #[test]
    fn database_config_from_url_decodes_reserved_chars_in_password() {
        let config = DatabaseConfig::from_url(
            "postgresql://mailer:p%40ss%3Awo%2Frd%3F@db.example.com:5432/maildb?sslmode=require",
        )
        .expect("parse DATABASE_URL with encoded password");

        assert_eq!(config.user, "mailer");
        assert_eq!(config.password, "p@ss:wo/rd?");
        assert_eq!(config.sslmode, "require");

        let round_trip = DatabaseConfig::from_url(&config.connection_string())
            .expect("round-trip encoded credentials");
        assert_eq!(round_trip.password, "p@ss:wo/rd?");
    }

    #[test]
    fn database_config_from_url_decodes_reserved_chars_in_username() {
        let config = DatabaseConfig::from_url(
            "postgresql://u%3Aser%40team%2Fops%3F:password@localhost:5432/maildb",
        )
        .expect("parse DATABASE_URL with encoded username");

        assert_eq!(config.user, "u:ser@team/ops?");
        assert_eq!(config.password, "password");
    }

    #[test]
    fn database_config_from_url_errors_for_missing_database() {
        let err = DatabaseConfig::from_url("postgresql://user:pass@localhost:5432")
            .expect_err("missing database name should fail")
            .to_string();

        assert!(
            err.contains("missing database name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn database_config_from_url_errors_for_malformed_url() {
        let err = DatabaseConfig::from_url("postgresql://[::1")
            .expect_err("malformed URL should fail")
            .to_string();

        assert!(
            err.contains("Invalid DATABASE_URL format"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn database_config_from_url_extracts_sslmode() {
        let config = DatabaseConfig::from_url(
            "postgresql://user:pass@localhost:5432/maildb?application_name=cli&sslmode=verify-full",
        )
        .expect("parse DATABASE_URL with sslmode");

        assert_eq!(config.sslmode, "verify-full");
    }
}
