use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_ACCOUNT_ID: &str = "default";
pub const DEFAULT_API_BIND: &str = "127.0.0.1:3100";
const DEFAULT_IMAP_PORT: u16 = 993;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiBindScope {
    Loopback,
    Tailscale,
}

#[derive(Clone, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub label: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub username: String,
    #[serde(skip)]
    pub password: String,
}

impl fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountConfig")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct AccountsFile {
    accounts: Vec<AccountConfig>,
}

impl AccountConfig {
    /// Load all accounts from accounts.toml if present, else fall back to single-account env config.
    pub fn load_all() -> Result<Vec<AccountConfig>> {
        let path = Path::new("accounts.toml");
        if path.exists() {
            Self::load_all_from_path(path)
        } else {
            Ok(vec![Self::load_from_env()?])
        }
    }

    /// Load single account by ID. Errors if not found.
    pub fn load(id: &str) -> Result<AccountConfig> {
        Self::load_all()?
            .into_iter()
            .find(|account| account.id == id)
            .with_context(|| format!("account '{}' not found", id))
    }

    pub fn imap_server(&self) -> String {
        format!("{}:{}", self.imap_host, self.imap_port)
    }

    fn load_all_from_path(path: &Path) -> Result<Vec<AccountConfig>> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("read accounts config {}", path.display()))?;
        let parsed: AccountsFile = toml::from_str(&content).context("parse accounts.toml")?;
        if parsed.accounts.is_empty() {
            anyhow::bail!("accounts.toml must define at least one [[accounts]] entry");
        }

        parsed
            .accounts
            .into_iter()
            .map(|account| account.resolve_password())
            .collect()
    }

    fn load_from_env() -> Result<AccountConfig> {
        let server_value =
            env_first(&["IMAP_SERVER"]).context("IMAP_SERVER environment variable not set")?;
        let (imap_host, parsed_port) = parse_imap_server(&server_value)?;
        let imap_port = env_first(&["IMAP_PORT"])
            .map(|value| {
                value
                    .parse::<u16>()
                    .with_context(|| format!("invalid IMAP_PORT '{}'", value))
            })
            .transpose()?
            .unwrap_or(parsed_port);
        let username =
            env_first(&["IMAP_USERNAME"]).context("IMAP_USERNAME environment variable not set")?;
        let password =
            env_first(&["IMAP_PASSWORD"]).context("IMAP_PASSWORD environment variable not set")?;

        Ok(AccountConfig {
            id: DEFAULT_ACCOUNT_ID.to_string(),
            label: "Default".to_string(),
            imap_host,
            imap_port,
            username,
            password,
        })
    }

    fn resolve_password(mut self) -> Result<AccountConfig> {
        let specific_key = account_password_env_key(&self.id);
        let missing_password_context = if self.id == DEFAULT_ACCOUNT_ID {
            format!(
                "{} environment variable not set and IMAP_PASSWORD is not available for the default account",
                specific_key
            )
        } else {
            format!(
                "{} environment variable not set (named accounts do not fall back to IMAP_PASSWORD)",
                specific_key
            )
        };
        self.password = std::env::var(&specific_key)
            .ok()
            .or_else(|| {
                if self.id == DEFAULT_ACCOUNT_ID {
                    env_first(&["IMAP_PASSWORD"])
                } else {
                    None
                }
            })
            .with_context(|| missing_password_context)?;
        Ok(self)
    }
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| std::env::var(key).ok())
}

pub fn api_bind_addr() -> String {
    env_first(&["API_BIND"]).unwrap_or_else(|| DEFAULT_API_BIND.to_string())
}

pub fn api_auth_token() -> Option<String> {
    env_first(&["API_AUTH_TOKEN", "MAILSUBSYSTEM_API_TOKEN"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn validate_api_bind_security(bind: &str) -> Result<ApiBindScope> {
    let scope = classify_api_bind_addr(bind)?;
    if scope == ApiBindScope::Tailscale && api_auth_token().is_none() {
        anyhow::bail!(
            "API_AUTH_TOKEN is required when binding the API to a Tailscale address ({})",
            bind
        );
    }
    Ok(scope)
}

pub fn api_allowed_origins() -> Vec<String> {
    env_first(&["API_ALLOWED_ORIGINS", "API_ALLOW_ORIGIN"])
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn classify_api_bind_addr(bind: &str) -> Result<ApiBindScope> {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return classify_bind_ip(addr.ip()).with_context(|| {
            format!(
                "API bind address '{}' is not allowed. Bind to loopback or a Tailscale address.",
                bind
            )
        });
    }

    let host = bind_host(bind)?;
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(ApiBindScope::Loopback);
    }

    let ip = host.parse::<IpAddr>().with_context(|| {
        format!(
            "API bind host '{}' must be localhost or an IP address",
            host
        )
    })?;
    classify_bind_ip(ip).with_context(|| {
        format!(
            "API bind address '{}' is not allowed. Bind to loopback or a Tailscale address.",
            bind
        )
    })
}

fn bind_host(bind: &str) -> Result<&str> {
    let trimmed = bind.trim();
    if trimmed.is_empty() {
        anyhow::bail!("API bind address cannot be empty");
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, _) = rest
            .split_once(']')
            .context("invalid bracketed IPv6 API bind address")?;
        return Ok(host);
    }
    trimmed
        .rsplit_once(':')
        .map(|(host, _)| host)
        .filter(|host| !host.is_empty())
        .context("API bind address must include host and port")
}

fn classify_bind_ip(ip: IpAddr) -> Option<ApiBindScope> {
    if ip.is_loopback() {
        return Some(ApiBindScope::Loopback);
    }
    match ip {
        IpAddr::V4(ip) if is_tailscale_ipv4(ip) => Some(ApiBindScope::Tailscale),
        IpAddr::V6(ip) if is_tailscale_ipv6(ip) => Some(ApiBindScope::Tailscale),
        _ => None,
    }
}

fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_tailscale_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
}

fn parse_imap_server(value: &str) -> Result<(String, u16)> {
    if let Some((host, port)) = value.rsplit_once(':') {
        if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return Ok((
                host.to_string(),
                port.parse::<u16>()
                    .with_context(|| format!("invalid IMAP server port in '{}'", value))?,
            ));
        }
    }
    Ok((value.to_string(), DEFAULT_IMAP_PORT))
}

fn account_password_env_key(id: &str) -> String {
    let normalized = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("MAILSUBSYSTEM_ACCOUNT_{}_PASSWORD", normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    fn clear_test_env() {
        for key in [
            "API_BIND",
            "API_AUTH_TOKEN",
            "MAILSUBSYSTEM_API_TOKEN",
            "API_ALLOWED_ORIGINS",
            "API_ALLOW_ORIGIN",
            "IMAP_SERVER",
            "IMAP_PORT",
            "IMAP_USERNAME",
            "IMAP_PASSWORD",
            "MAILSUBSYSTEM_ACCOUNT_PRIMARY_PASSWORD",
        ] {
            std::env::remove_var(key);
        }
    }

    fn with_temp_workdir<T>(f: impl FnOnce() -> T) -> T {
        struct CurrentDirGuard {
            original_dir: std::path::PathBuf,
            temp_dir: std::path::PathBuf,
        }

        impl Drop for CurrentDirGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.original_dir);
                let _ = fs::remove_dir(&self.temp_dir);
            }
        }

        let original_dir = std::env::current_dir().expect("current dir");
        let temp_dir = std::env::temp_dir().join(format!(
            "mailsubsystem-config-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        std::env::set_current_dir(&temp_dir).expect("set temp current dir");
        let _guard = CurrentDirGuard {
            original_dir,
            temp_dir,
        };

        f()
    }

    #[test]
    fn test_account_config_debug_redacts_password() {
        let account = AccountConfig {
            id: "primary".to_string(),
            label: "Primary".to_string(),
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            username: "user@example.com".to_string(),
            password: "imap-super-secret".to_string(),
        };

        let debug = format!("{account:?}");

        assert!(debug.contains("password: \"<redacted>\""));
        assert!(!debug.contains("imap-super-secret"));
    }

    #[test]
    fn test_load_all_falls_back_to_env() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();
        std::env::set_var("IMAP_SERVER", "imap.example.com");
        std::env::set_var("IMAP_PORT", "1993");
        std::env::set_var("IMAP_USERNAME", "user@example.com");
        std::env::set_var("IMAP_PASSWORD", "secret");

        let accounts = with_temp_workdir(AccountConfig::load_all).expect("load fallback account");
        assert_eq!(accounts.len(), 1);
        let account = &accounts[0];
        assert_eq!(account.id, DEFAULT_ACCOUNT_ID);
        assert_eq!(account.label, "Default");
        assert_eq!(account.imap_host, "imap.example.com");
        assert_eq!(account.imap_port, 1993);
        assert_eq!(account.username, "user@example.com");
        assert_eq!(account.password, "secret");
    }

    #[test]
    fn test_account_config_from_toml() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();
        std::env::set_var("MAILSUBSYSTEM_ACCOUNT_PRIMARY_PASSWORD", "toml-secret");

        let temp_path = std::env::temp_dir().join(format!(
            "mailsubsystem-accounts-{}.toml",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &temp_path,
            r#"
[[accounts]]
id = "primary"
label = "Primary"
imap_host = "imap.example.com"
imap_port = 993
username = "primary@example.com"
"#,
        )
        .expect("write temp accounts.toml");

        let accounts = AccountConfig::load_all_from_path(&temp_path).expect("load accounts");
        assert_eq!(accounts.len(), 1);
        let account = &accounts[0];
        assert_eq!(account.id, "primary");
        assert_eq!(account.label, "Primary");
        assert_eq!(account.imap_host, "imap.example.com");
        assert_eq!(account.imap_port, 993);
        assert_eq!(account.username, "primary@example.com");
        assert_eq!(account.password, "toml-secret");

        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_api_bind_addr_defaults_and_overrides() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();

        assert_eq!(api_bind_addr(), DEFAULT_API_BIND);

        std::env::set_var("API_BIND", "127.0.0.1:4100");
        assert_eq!(api_bind_addr(), "127.0.0.1:4100");
    }

    #[test]
    fn test_api_bind_security_allows_loopback_without_token() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();

        assert_eq!(
            validate_api_bind_security("127.0.0.1:3100").expect("loopback allowed"),
            ApiBindScope::Loopback
        );
        assert_eq!(
            validate_api_bind_security("[::1]:3100").expect("ipv6 loopback allowed"),
            ApiBindScope::Loopback
        );
        assert_eq!(
            validate_api_bind_security("localhost:3100").expect("localhost allowed"),
            ApiBindScope::Loopback
        );
    }

    #[test]
    fn test_api_bind_security_requires_token_for_tailscale() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();

        assert!(validate_api_bind_security("100.64.1.2:3100").is_err());

        std::env::set_var("API_AUTH_TOKEN", "dev-token");
        assert_eq!(
            validate_api_bind_security("100.64.1.2:3100").expect("tailscale allowed"),
            ApiBindScope::Tailscale
        );
        assert_eq!(
            validate_api_bind_security("[fd7a:115c:a1e0::1]:3100").expect("tailscale ipv6 allowed"),
            ApiBindScope::Tailscale
        );
    }

    #[test]
    fn test_api_bind_security_rejects_public_bind_addresses() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();
        std::env::set_var("API_AUTH_TOKEN", "dev-token");

        assert!(validate_api_bind_security("0.0.0.0:3100").is_err());
        assert!(validate_api_bind_security("192.168.1.10:3100").is_err());
        assert!(validate_api_bind_security("[::]:3100").is_err());
    }

    #[test]
    fn test_api_allowed_origins_defaults_and_parses_csv() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();

        assert!(api_allowed_origins().is_empty());

        std::env::set_var(
            "API_ALLOWED_ORIGINS",
            " http://127.0.0.1:5173 , http://localhost:3000 ,, ",
        );
        assert_eq!(
            api_allowed_origins(),
            vec![
                "http://127.0.0.1:5173".to_string(),
                "http://localhost:3000".to_string(),
            ]
        );
    }

    #[test]
    fn test_api_allowed_origins_supports_legacy_single_origin_env() {
        let _guard = env_lock().lock().expect("env lock");
        clear_test_env();

        std::env::set_var("API_ALLOW_ORIGIN", "http://localhost:5173");
        assert_eq!(
            api_allowed_origins(),
            vec!["http://localhost:5173".to_string()]
        );
    }
}
