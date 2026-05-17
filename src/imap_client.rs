//! Minimal IMAP client: connect, list UIDs, modseq, headers, raw email.
//! Add other functionality (move, delete, flags, etc.) as needed.

use anyhow::{Context, Result};
use async_imap::types::NameAttribute;
use async_imap::Session;
use async_native_tls::{Certificate, Protocol, TlsConnector};
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use mailparse::*;
use regex::Regex;
use rustls_pki_types::{pem::PemObject, CertificateDer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore};

/// Build a native TLS connector with normal certificate and hostname validation.
fn build_tls_connector() -> Result<TlsConnector> {
    let connector = TlsConnector::new().min_protocol_version(Some(Protocol::Tlsv12));
    add_imap_ca_cert_file(connector)
}

fn add_imap_ca_cert_file(mut connector: TlsConnector) -> Result<TlsConnector> {
    let Some(path) = std::env::var_os("IMAP_TLS_CA_CERT_FILE") else {
        return Ok(connector);
    };
    let path = PathBuf::from(path);
    let certs = CertificateDer::pem_file_iter(&path).with_context(|| {
        format!(
            "open IMAP_TLS_CA_CERT_FILE certificate bundle at {}",
            path.display()
        )
    })?;
    let mut added = 0usize;
    let mut ignored = 0usize;
    for cert in certs {
        match cert {
            Ok(cert) => {
                let cert = Certificate::from_der(cert.as_ref()).with_context(|| {
                    format!(
                        "parse certificate from IMAP_TLS_CA_CERT_FILE={}",
                        path.display()
                    )
                })?;
                connector = connector.add_root_certificate(cert);
                added += 1;
            }
            Err(error) => {
                ignored += 1;
                log::warn!(
                    "ignored invalid certificate in IMAP_TLS_CA_CERT_FILE={}: {}",
                    path.display(),
                    error
                );
            }
        }
    }
    if added == 0 {
        anyhow::bail!(
            "IMAP_TLS_CA_CERT_FILE at {} did not contain any valid X.509 certificates",
            path.display()
        );
    }
    if ignored > 0 {
        log::warn!(
            "ignored {} invalid certificates from IMAP_TLS_CA_CERT_FILE={}",
            ignored,
            path.display()
        );
    }
    log::info!(
        "loaded {} IMAP TLS root certificates from {}",
        added,
        path.display()
    );
    Ok(connector)
}

fn tls_server_name_for_host(host: &str) -> Result<String> {
    let server_name = std::env::var("IMAP_TLS_SERVER_NAME").unwrap_or_else(|_| host.to_string());
    let server_name = server_name.trim();
    if server_name.is_empty() {
        anyhow::bail!("IMAP_TLS_SERVER_NAME must not be empty when set");
    }
    Ok(server_name.to_string())
}

type TlsTcpStream = async_native_tls::TlsStream<TcpStream>;

async fn connect_tls_tcp(host: &str, port: u16) -> Result<TlsTcpStream> {
    let tls = build_tls_connector()?;
    let tls_server_name = tls_server_name_for_host(host)?;
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("TCP connect to IMAP server {host}:{port}"))?;
    tls.connect(tls_server_name.as_str(), tcp)
        .await
        .with_context(|| {
            format!(
                "Failed to establish TLS connection to IMAP server {host}:{port} using certificate name {tls_server_name}. Verify IMAP_SERVER matches the certificate hostname, set IMAP_TLS_SERVER_NAME for a safe hostname override, or set IMAP_TLS_CA_CERT_FILE to a PEM CA bundle for private/self-signed servers"
            )
        })
}

pub type ImapSession = Session<TlsTcpStream>;

/// Result of list_mailboxes_with_attributes: (folder_name, is_noselect, delimiter).
pub type MailboxWithAttrs = (String, bool, Option<String>);

/// Load IMAP credentials from environment variables.
/// Returns (server, username, password).
pub fn load_imap_from_env() -> Result<(String, String, String)> {
    let imap_server =
        std::env::var("IMAP_SERVER").context("IMAP_SERVER environment variable not set")?;
    let username =
        std::env::var("IMAP_USERNAME").context("IMAP_USERNAME environment variable not set")?;
    let password =
        std::env::var("IMAP_PASSWORD").context("IMAP_PASSWORD environment variable not set")?;
    Ok((imap_server, username, password))
}

/// Mailbox state from SELECT: exists, uid_validity, uid_next. For envelope sync.
#[derive(Debug, Clone, Default)]
pub struct MailboxState {
    pub exists: u32,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
}

/// Result of fetch_envelope (RFC 3501 ALL): parsed headers from server, no body.
#[derive(Debug, Clone, Default)]
pub struct FetchEnvelopeResult {
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<DateTime<Utc>>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Result of full message fetch (slow sync): raw content, parsed headers, body, flags.
#[derive(Debug, Clone, Default)]
pub struct FullMessageResult {
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_date: Option<DateTime<Utc>>,
    pub recipients_to: Vec<String>,
    pub recipients_cc: Vec<String>,
    pub recipients_bcc: Vec<String>,
    pub raw_email_content: String,
    pub body_text: Option<String>,
    pub is_read: bool,
    pub message_size: Option<u32>,
    pub modseq: Option<u64>,
    pub list_unsubscribe: Option<String>,
    pub list_id: Option<String>,
    pub x_priority: Option<String>,
    pub precedence: Option<String>,
    pub return_path: Option<String>,
    pub reply_to: Option<String>,
    pub custom_headers: serde_json::Value,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EmailHeaders {
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub date: Option<DateTime<Utc>>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub recipients_to: Vec<String>,
    pub recipients_cc: Vec<String>,
    pub recipients_bcc: Vec<String>,
    pub list_unsubscribe: Option<String>,
    pub list_id: Option<String>,
    pub precedence: Option<String>,
    pub x_priority: Option<String>,
    pub return_path: Option<String>,
    pub reply_to: Option<String>,
    pub custom_headers: serde_json::Value,
}

/// Result of UID MOVE: new UID and UID validity in the destination mailbox (from COPYUID response).
#[derive(Debug, Clone)]
pub struct MoveResult {
    pub new_uid: u32,
    pub new_uid_validity: u32,
}

struct PooledSession {
    session: ImapSession,
    selected_mailbox: Option<String>,
    last_used_at: Instant,
}

struct ActiveImapState {
    session_pool: Vec<PooledSession>,
    borrowed_count: usize,
    mailbox_cache: Option<(Instant, Vec<MailboxWithAttrs>)>,
    mailbox_cache_ttl: Duration,
    min_command_interval: Duration,
    last_command_at: Option<Instant>,
}

impl ActiveImapState {
    fn new(pool_size: usize) -> Self {
        let min_command_interval = std::env::var("IMAP_MIN_COMMAND_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(500));
        Self {
            session_pool: Vec::with_capacity(pool_size),
            borrowed_count: 0,
            mailbox_cache: None,
            mailbox_cache_ttl: Duration::from_secs(300),
            min_command_interval,
            last_command_at: None,
        }
    }

    async fn throttle_global(&mut self, _op_name: &'static str) {
        let now = Instant::now();
        if let Some(last) = self.last_command_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < self.min_command_interval {
                let remaining = self.min_command_interval - elapsed;
                tokio::time::sleep(remaining).await;
            }
        }
        self.last_command_at = Some(Instant::now());
    }
}

impl PooledSession {
    fn new(session: ImapSession) -> Self {
        Self {
            session,
            selected_mailbox: None,
            last_used_at: Instant::now(),
        }
    }

    fn touch(&mut self) {
        self.last_used_at = Instant::now();
    }
}

struct SessionStateShim {
    selected_mailbox: Option<String>,
    mailbox_cache: Option<(Instant, Vec<MailboxWithAttrs>)>,
    mailbox_cache_ttl: Duration,
}

impl SessionStateShim {
    async fn ensure_selected(
        &mut self,
        session: &mut ImapSession,
        mailbox_name: &str,
    ) -> Result<()> {
        if self.selected_mailbox.as_deref() == Some(mailbox_name) {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(10), session.select(mailbox_name))
            .await
            .context("Timeout selecting mailbox")?
            .context("Failed to select mailbox")?;
        self.selected_mailbox = Some(mailbox_name.to_string());
        Ok(())
    }
}

#[derive(Clone)]
pub struct ImapClient {
    server: String,
    username: String,
    password: String,
    active: Arc<Mutex<ActiveImapState>>,
    pool_semaphore: Arc<Semaphore>,
}

impl ImapClient {
    pub fn new(server: String, username: String, password: String) -> Self {
        Self::new_with_pool_size(server, username, password, None)
    }

    pub fn new_with_pool_size(
        server: String,
        username: String,
        password: String,
        pool_size: Option<usize>,
    ) -> Self {
        let pool_size = pool_size.unwrap_or_else(|| {
            std::env::var("IMAP_POOL_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3)
        });
        ImapClient {
            server,
            username,
            password,
            active: Arc::new(Mutex::new(ActiveImapState::new(pool_size))),
            pool_semaphore: Arc::new(Semaphore::new(pool_size)),
        }
    }

    async fn borrow_session(&self, op_name: &'static str) -> Result<PooledSession> {
        let mut state = self.active.lock().await;
        state.throttle_global(op_name).await;

        if let Some(mut pooled) = state.session_pool.pop() {
            state.borrowed_count += 1;
            drop(state);
            pooled.touch();
            return Ok(pooled);
        }

        state.borrowed_count += 1;
        drop(state);

        let session = self.connect().await?;
        Ok(PooledSession::new(session))
    }

    async fn return_session(&self, session: PooledSession, success: bool, _op_name: &'static str) {
        let mut state = self.active.lock().await;
        state.borrowed_count = state.borrowed_count.saturating_sub(1);
        if success {
            state.session_pool.push(session);
        }
    }

    async fn with_active_session<T, F>(&self, op_name: &'static str, f: F) -> Result<T>
    where
        for<'a> F:
            FnOnce(&'a mut ImapSession, &'a mut SessionStateShim) -> BoxFuture<'a, Result<T>>,
    {
        let _permit = self
            .pool_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("Pool semaphore closed"))?;

        let mut pooled = self.borrow_session(op_name).await?;

        let (initial_selected, mailbox_cache, mailbox_cache_ttl) = {
            let state = self.active.lock().await;
            (
                pooled.selected_mailbox.clone(),
                state.mailbox_cache.clone(),
                state.mailbox_cache_ttl,
            )
        };

        let mut shim = SessionStateShim {
            selected_mailbox: initial_selected,
            mailbox_cache,
            mailbox_cache_ttl,
        };

        let res = f(&mut pooled.session, &mut shim).await;
        pooled.selected_mailbox = shim.selected_mailbox;

        if shim.mailbox_cache.is_some() {
            let mut state = self.active.lock().await;
            state.mailbox_cache = shim.mailbox_cache;
        }

        let success = res.is_ok();
        self.return_session(pooled, success, op_name).await;
        res
    }

    async fn invalidate_mailbox_cache(&self) {
        let mut state = self.active.lock().await;
        state.mailbox_cache = None;
    }

    pub async fn refresh_mailboxes_with_attributes(&self) -> Result<Vec<MailboxWithAttrs>> {
        self.invalidate_mailbox_cache().await;
        self.list_mailboxes_with_attributes().await
    }

    pub async fn disconnect(&self) -> Result<()> {
        let mut state = self.active.lock().await;
        for mut pooled in state.session_pool.drain(..) {
            let _ = pooled.session.logout().await;
        }
        state.mailbox_cache = None;
        Ok(())
    }

    pub async fn connect(&self) -> Result<ImapSession> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port in IMAP_SERVER")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let client = async_imap::Client::new(tls_stream);

        let session = client
            .login(&self.username, &self.password)
            .await
            .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;

        Ok(session)
    }

    /// List mailboxes with (name, is_noselect, delimiter). For db::sync_folders_from_imap.
    pub async fn list_mailboxes_with_attributes(&self) -> Result<Vec<MailboxWithAttrs>> {
        self.with_active_session("list_mailboxes_with_attributes", |session, state| {
            Box::pin(async move {
                if let Some((cached_at, cached)) = &state.mailbox_cache {
                    if cached_at.elapsed() < state.mailbox_cache_ttl {
                        return Ok(cached.clone());
                    }
                }

                let mailboxes_stream = session
                    .list(None, Some("*"))
                    .await
                    .context("Failed to list mailboxes")?;

                use futures_util::TryStreamExt;
                let mailboxes: Vec<_> = mailboxes_stream
                    .try_collect()
                    .await
                    .context("Failed to collect mailboxes")?;

                let result: Vec<MailboxWithAttrs> = mailboxes
                    .iter()
                    .map(|mb| {
                        let is_noselect = mb
                            .attributes()
                            .iter()
                            .any(|a| matches!(a, NameAttribute::NoSelect));
                        let delimiter = mb.delimiter().map(|s| s.to_string());
                        (mb.name().to_string(), is_noselect, delimiter)
                    })
                    .collect();

                state.mailbox_cache = Some((Instant::now(), result.clone()));
                Ok(result)
            })
        })
        .await
    }

    /// Ensure a mailbox exists. Returns success if it already exists.
    pub async fn ensure_mailbox(&self, mailbox_name: &str) -> Result<()> {
        let mailboxes = self.list_mailboxes_with_attributes().await?;
        if mailboxes.iter().any(|(name, _, _)| name == mailbox_name) {
            return Ok(());
        }

        match self.raw_create_folder(mailbox_name).await {
            Ok(()) => {}
            Err(error) => {
                self.invalidate_mailbox_cache().await;
                let refreshed = self.list_mailboxes_with_attributes().await?;
                if refreshed.iter().any(|(name, _, _)| name == mailbox_name) {
                    return Ok(());
                }
                return Err(error).with_context(|| format!("create mailbox {}", mailbox_name));
            }
        }

        self.invalidate_mailbox_cache().await;
        let refreshed = self.list_mailboxes_with_attributes().await?;
        if refreshed.iter().any(|(name, _, _)| name == mailbox_name) {
            Ok(())
        } else {
            anyhow::bail!("Mailbox {} was not visible after CREATE", mailbox_name);
        }
    }

    /// Search for UIDs of messages with the given Message-ID in a mailbox (for resolving moved messages).
    pub async fn search_uids_by_message_id(
        &self,
        mailbox_name: &str,
        message_id: &str,
    ) -> Result<Vec<u32>> {
        let search_value = message_id.trim();
        let search_value = if search_value.starts_with('<') && search_value.ends_with('>') {
            search_value.to_string()
        } else {
            format!("<{}>", search_value)
        };
        // IMAP SEARCH HEADER "Message-ID" "value" – value must be quoted if it contains spaces/specials
        let query = format!(
            r#"HEADER "Message-ID" "{}""#,
            search_value.replace('\\', "\\\\").replace('"', "\\\"")
        );
        self.search_uids(mailbox_name, &query).await
    }

    /// UID SEARCH: returns UIDs matching query, sorted ascending. Query e.g. "ALL" or "UID 5:*".
    pub async fn search_uids(&self, mailbox_name: &str, query: &str) -> Result<Vec<u32>> {
        let mailbox_name = mailbox_name.to_string();
        let query = query.to_string();
        self.with_active_session("search_uids", |session, state| {
            Box::pin(async move {
                state.ensure_selected(session, &mailbox_name).await?;
                let uids =
                    tokio::time::timeout(Duration::from_secs(30), session.uid_search(&query))
                        .await
                        .context("Timeout searching UIDs")?
                        .context("Failed to search UIDs")?;
                let mut vec: Vec<u32> = uids.into_iter().collect();
                vec.sort_unstable();
                Ok(vec)
            })
        })
        .await
    }

    /// Raw UID FETCH envelopes: fetch BODY.PEEK[HEADER], parse headers to FetchEnvelopeResult. Returns (MailboxState, envelopes).
    pub async fn raw_uid_fetch_envelopes(
        &self,
        mailbox_name: &str,
        start_uid: u32,
        max_uids: usize,
    ) -> Result<(MailboxState, Vec<(u32, FetchEnvelopeResult)>)> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);

        let mut buf = String::new();
        reader.read_line(&mut buf).await?;

        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            self.username.replace('\\', "\\\\").replace('"', "\\\""),
            self.password.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a001 ") {
                break;
            }
        }

        let sel = format!(
            "a002 SELECT \"{}\"\r\n",
            mailbox_name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;

        let exists_re = Regex::new(r"^\* (\d+) EXISTS").unwrap();
        let uid_next_re = Regex::new(r"\[UIDNEXT (\d+)\]").unwrap();
        let uid_validity_re = Regex::new(r"\[UIDVALIDITY (\d+)\]").unwrap();
        let mut mailbox_state = MailboxState {
            exists: 0,
            uid_validity: None,
            uid_next: None,
        };

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a002 ") {
                if let Some(c) = uid_next_re.captures(&buf) {
                    mailbox_state.uid_next = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
                if let Some(c) = uid_validity_re.captures(&buf) {
                    mailbox_state.uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
                break;
            }
            if let Some(c) = exists_re.captures(&buf) {
                mailbox_state.exists = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            }
            if buf.contains("[UIDNEXT ") {
                if let Some(c) = uid_next_re.captures(&buf) {
                    mailbox_state.uid_next = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
            }
            if buf.contains("[UIDVALIDITY ") {
                if let Some(c) = uid_validity_re.captures(&buf) {
                    mailbox_state.uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
            }
        }

        if mailbox_state.exists == 0 {
            return Ok((mailbox_state, Vec::new()));
        }
        let uid_end = mailbox_state
            .uid_next
            .map(|n| n.saturating_sub(1))
            .unwrap_or(0);
        if uid_end == 0 {
            return Ok((mailbox_state, Vec::new()));
        }

        let start = if start_uid == 0 { 1 } else { start_uid };
        let end = start
            .saturating_add(max_uids as u32)
            .saturating_sub(1)
            .min(uid_end);

        if start > end {
            return Ok((mailbox_state, Vec::new()));
        }
        let uid_range = format!("{}:{}", start, end);
        eprintln!(
            "  {}: UID FETCH {} (exists={})",
            mailbox_name, uid_range, mailbox_state.exists
        );

        // Header-only fetch for envelope sync (lighter than full BODY.PEEK[]).
        let fetch = format!("a003 UID FETCH {} (UID BODY.PEEK[HEADER])\r\n", uid_range);
        writer.write_all(fetch.as_bytes()).await?;
        writer.flush().await?;

        let fetch_uid_body_re =
            Regex::new(r"^\* \d+ FETCH \(UID (\d+) BODY\[HEADER\] \{(\d+)\}").unwrap();
        let fetch_uid_body_alt =
            Regex::new(r"^\* \d+ FETCH \(UID (\d+) BODY\[HEADER\] NIL").unwrap();
        let mut results = Vec::new();

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a003 ") {
                break;
            }
            if buf.starts_with("* ") && buf.contains(" FETCH ") {
                if let Some(caps) = fetch_uid_body_re.captures(&buf) {
                    let uid: u32 = caps
                        .get(1)
                        .context("parse IMAP response: missing UID capture in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    let lit_len: usize = caps
                        .get(2)
                        .context("parse IMAP response: missing literal length in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    let mut lit = vec![0u8; lit_len];
                    reader.read_exact(&mut lit).await?;
                    let full_str = String::from_utf8_lossy(&lit);
                    let h = Self::parse_email_headers(&full_str).unwrap_or_default();
                    results.push((
                        uid,
                        FetchEnvelopeResult {
                            message_id: h.message_id,
                            subject: h.subject,
                            from: h.sender,
                            date: h.date,
                            to: h.recipients_to,
                            cc: h.recipients_cc,
                            bcc: h.recipients_bcc,
                        },
                    ));
                    buf.clear();
                    reader.read_line(&mut buf).await?;
                } else if let Some(caps) = fetch_uid_body_alt.captures(&buf) {
                    let uid: u32 = caps
                        .get(1)
                        .context("parse IMAP response: missing UID capture in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    results.push((uid, FetchEnvelopeResult::default()));
                }
            }
        }

        eprintln!(
            "  {}: UID FETCH returned {} messages",
            mailbox_name,
            results.len()
        );
        Ok((mailbox_state, results))
    }

    /// Header-only envelope fetch by UID list (non-contiguous).
    pub async fn raw_uid_fetch_envelopes_by_uids(
        &self,
        mailbox_name: &str,
        uids: &[u32],
    ) -> Result<(MailboxState, Vec<(u32, FetchEnvelopeResult)>)> {
        if uids.is_empty() {
            return Ok((MailboxState::default(), Vec::new()));
        }

        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);

        let mut buf = String::new();
        reader.read_line(&mut buf).await?;

        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            self.username.replace('\\', "\\\\").replace('"', "\\\""),
            self.password.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }

        let sel = format!(
            "a002 SELECT \"{}\"\r\n",
            mailbox_name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;

        let exists_re = Regex::new(r"^\* (\d+) EXISTS").unwrap();
        let uid_next_re = Regex::new(r"\[UIDNEXT (\d+)\]").unwrap();
        let uid_validity_re = Regex::new(r"\[UIDVALIDITY (\d+)\]").unwrap();
        let mut mailbox_state = MailboxState::default();

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a002 ") {
                if let Some(c) = uid_next_re.captures(&buf) {
                    mailbox_state.uid_next = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
                if let Some(c) = uid_validity_re.captures(&buf) {
                    mailbox_state.uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
                break;
            }
            if let Some(c) = exists_re.captures(&buf) {
                mailbox_state.exists = c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            }
            if buf.contains("[UIDNEXT ") {
                if let Some(c) = uid_next_re.captures(&buf) {
                    mailbox_state.uid_next = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
            }
            if buf.contains("[UIDVALIDITY ") {
                if let Some(c) = uid_validity_re.captures(&buf) {
                    mailbox_state.uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
            }
        }

        let uid_list = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        eprintln!(
            "  {}: UID FETCH {} (exists={})",
            mailbox_name, uid_list, mailbox_state.exists
        );

        let fetch = format!("a003 UID FETCH {} (UID BODY.PEEK[HEADER])\r\n", uid_list);
        writer.write_all(fetch.as_bytes()).await?;
        writer.flush().await?;

        let fetch_uid_body_re =
            Regex::new(r"^\* \d+ FETCH \(UID (\d+) BODY\[HEADER\] \{(\d+)\}").unwrap();
        let fetch_uid_body_alt =
            Regex::new(r"^\* \d+ FETCH \(UID (\d+) BODY\[HEADER\] NIL").unwrap();
        let mut results = Vec::new();

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a003 ") {
                break;
            }
            if buf.starts_with("* ") && buf.contains(" FETCH ") {
                if let Some(caps) = fetch_uid_body_re.captures(&buf) {
                    let uid: u32 = caps
                        .get(1)
                        .context("parse IMAP response: missing UID capture in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    let lit_len: usize = caps
                        .get(2)
                        .context("parse IMAP response: missing literal length in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    let mut lit = vec![0u8; lit_len];
                    reader.read_exact(&mut lit).await?;
                    let header_str = String::from_utf8_lossy(&lit);
                    let h = Self::parse_email_headers(&header_str).unwrap_or_default();
                    results.push((
                        uid,
                        FetchEnvelopeResult {
                            message_id: h.message_id,
                            subject: h.subject,
                            from: h.sender,
                            date: h.date,
                            to: h.recipients_to,
                            cc: h.recipients_cc,
                            bcc: h.recipients_bcc,
                        },
                    ));
                    buf.clear();
                    reader.read_line(&mut buf).await?;
                } else if let Some(caps) = fetch_uid_body_alt.captures(&buf) {
                    let uid: u32 = caps
                        .get(1)
                        .context("parse IMAP response: missing UID capture in header fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    results.push((uid, FetchEnvelopeResult::default()));
                }
            }
        }

        eprintln!(
            "  {}: UID FETCH returned {} messages",
            mailbox_name,
            results.len()
        );
        Ok((mailbox_state, results))
    }

    fn extract_email_addresses(header_value: Option<String>) -> Vec<String> {
        header_value
            .as_deref()
            .map(|s| {
                s.split(',')
                    .flat_map(|part| {
                        let part = part.trim();
                        if let Some(start) = part.find('<') {
                            if let Some(end) = part.find('>') {
                                vec![part[start + 1..end].trim().to_string()]
                            } else {
                                vec![part.to_string()]
                            }
                        } else if part.contains('@') {
                            vec![part.to_string()]
                        } else {
                            vec![]
                        }
                    })
                    .filter(|e| !e.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Parse headers from raw email content.
    pub fn parse_email_headers(email_content: &str) -> Result<EmailHeaders> {
        let bytes = if email_content.contains("\r\n\r\n") || email_content.contains("\n\n") {
            email_content.as_bytes().to_vec()
        } else {
            [email_content.as_bytes(), b"\r\n\r\n"].concat()
        };
        let parsed = parse_mail(&bytes).context("Failed to parse email")?;
        let headers = parsed.get_headers();

        let message_id = headers
            .get_first_value("Message-ID")
            .or_else(|| headers.get_first_value("message-id"))
            .map(|s| s.trim().trim_matches('<').trim_matches('>').to_string());

        let subject = headers
            .get_first_value("Subject")
            .or_else(|| headers.get_first_value("subject"))
            .map(|s| s.trim().to_string());

        let sender = headers
            .get_first_value("From")
            .or_else(|| headers.get_first_value("from"))
            .or_else(|| headers.get_first_value("Sender"))
            .map(|s| {
                if let Some(start) = s.find('<') {
                    if let Some(end) = s.find('>') {
                        return s[start + 1..end].trim().to_string();
                    }
                }
                s.trim().to_string()
            });

        let date = headers
            .get_first_value("Date")
            .or_else(|| headers.get_first_value("date"))
            .and_then(|s| {
                mailparse::dateparse(&s)
                    .ok()
                    .map(|ts| DateTime::from_timestamp(ts, 0).unwrap_or_else(Utc::now))
            });

        let in_reply_to = headers
            .get_first_value("In-Reply-To")
            .or_else(|| headers.get_first_value("in-reply-to"))
            .map(|s| s.trim().trim_matches('<').trim_matches('>').to_string());

        let references = headers
            .get_first_value("References")
            .or_else(|| headers.get_first_value("references"))
            .map(|s| {
                s.split_whitespace()
                    .map(|m| m.trim().trim_matches('<').trim_matches('>').to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let recipients_to = Self::extract_email_addresses(
            headers
                .get_first_value("To")
                .or_else(|| headers.get_first_value("to")),
        );
        let recipients_cc = Self::extract_email_addresses(
            headers
                .get_first_value("Cc")
                .or_else(|| headers.get_first_value("cc")),
        );
        let recipients_bcc = Self::extract_email_addresses(
            headers
                .get_first_value("Bcc")
                .or_else(|| headers.get_first_value("bcc")),
        );

        let list_unsubscribe = headers
            .get_first_value("List-Unsubscribe")
            .or_else(|| headers.get_first_value("list-unsubscribe"))
            .map(|s| s.trim().to_string());
        let list_id = headers
            .get_first_value("List-Id")
            .or_else(|| headers.get_first_value("list-id"))
            .map(|s| s.trim().to_string());
        let precedence = headers
            .get_first_value("Precedence")
            .or_else(|| headers.get_first_value("precedence"))
            .map(|s| s.trim().to_string());
        let x_priority = headers
            .get_first_value("X-Priority")
            .or_else(|| headers.get_first_value("x-priority"))
            .map(|s| s.trim().to_string());
        let return_path = headers
            .get_first_value("Return-Path")
            .or_else(|| headers.get_first_value("return-path"))
            .map(|s| s.trim().trim_matches('<').trim_matches('>').to_string());
        let reply_to = headers
            .get_first_value("Reply-To")
            .or_else(|| headers.get_first_value("reply-to"))
            .map(|s| {
                if let Some(start) = s.find('<') {
                    if let Some(end) = s.find('>') {
                        return s[start + 1..end].trim().to_string();
                    }
                }
                s.trim().to_string()
            });

        let mut custom_headers = serde_json::Map::new();
        for name in &[
            "X-Mailer",
            "X-Original-Sender",
            "X-Sender",
            "X-Spam-Status",
            "X-Spam-Level",
        ] {
            if let Some(v) = headers
                .get_first_value(name)
                .or_else(|| headers.get_first_value(&name.to_lowercase()))
            {
                custom_headers.insert(
                    name.to_lowercase(),
                    serde_json::Value::String(v.trim().to_string()),
                );
            }
        }

        Ok(EmailHeaders {
            message_id,
            subject,
            sender,
            date,
            in_reply_to,
            references,
            recipients_to,
            recipients_cc,
            recipients_bcc,
            list_unsubscribe,
            list_id,
            precedence,
            x_priority,
            return_path,
            reply_to,
            custom_headers: serde_json::Value::Object(custom_headers),
        })
    }

    /// Extract plain-text body from parsed MIME message. Prefers text/plain over text/html.
    fn extract_body_text_from_parsed(parsed: &ParsedMail) -> Option<String> {
        let mimetype = parsed.ctype.mimetype.to_lowercase();

        if mimetype.starts_with("text/plain") {
            return parsed.get_body().ok();
        }
        if mimetype.starts_with("text/html") {
            return parsed.get_body().ok();
        }
        if mimetype.starts_with("multipart/") {
            let mut plain: Option<String> = None;
            let mut html: Option<String> = None;
            for sub in &parsed.subparts {
                if let Some(t) = Self::extract_body_text_from_parsed(sub) {
                    let sub_mt = sub.ctype.mimetype.to_lowercase();
                    if sub_mt.starts_with("text/html") {
                        html = Some(t);
                    } else {
                        plain = Some(t);
                    }
                }
            }
            return plain.or(html);
        }
        None
    }

    /// Check IMAP tagged response: bail on NO/BAD so we don't silently ignore server errors.
    fn check_imap_tag_response(buf: &str, tag: &str) -> Result<()> {
        if !buf.starts_with(tag) {
            return Ok(());
        }
        let after_tag = buf[tag.len()..].trim_start();
        if after_tag.starts_with("OK") {
            return Ok(());
        }
        if after_tag.starts_with("NO") {
            anyhow::bail!("IMAP {}: {}", tag.trim(), after_tag);
        }
        if after_tag.starts_with("BAD") {
            anyhow::bail!("IMAP {}: {}", tag.trim(), after_tag);
        }
        Ok(())
    }

    fn is_mailbox_already_exists_response(buf: &str) -> bool {
        let lower = buf.to_ascii_lowercase();
        lower.contains("[alreadyexists]") || lower.contains("already exists")
    }

    /// Parse IMAP INTERNALDATE: "DD-Mon-YYYY HH:MM:SS +HHMM"
    fn parse_internaldate(s: &str) -> Option<DateTime<Utc>> {
        let s = s.trim().trim_matches('"');
        chrono::DateTime::parse_from_str(s, "%d-%b-%Y %H:%M:%S %z")
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Raw UID FETCH full messages: UID FLAGS INTERNALDATE BODY.PEEK[] for given UIDs.
    /// Returns parsed full message results for upsert.
    pub async fn raw_uid_fetch_full_messages(
        &self,
        mailbox_name: &str,
        uids: &[u32],
        uid_validity: u32,
    ) -> Result<Vec<(u32, FullMessageResult)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);

        let mut buf = String::new();
        reader.read_line(&mut buf).await?;

        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            self.username.replace('\\', "\\\\").replace('"', "\\\""),
            self.password.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }

        let sel = format!(
            "a002 SELECT \"{}\"\r\n",
            mailbox_name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;

        let uid_validity_re = Regex::new(r"\[UIDVALIDITY (\d+)\]").unwrap();
        let mut select_uid_validity: Option<u32> = None;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a002 ") {
                if let Some(c) = uid_validity_re.captures(&buf) {
                    select_uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP SELECT failed")?;
                break;
            }
            if buf.contains("[UIDVALIDITY ") {
                if let Some(c) = uid_validity_re.captures(&buf) {
                    select_uid_validity = c.get(1).and_then(|m| m.as_str().parse().ok());
                }
            }
        }

        if uid_validity != 0 {
            if let Some(select_uv) = select_uid_validity {
                if select_uv != uid_validity {
                    anyhow::bail!(
                        "UIDVALIDITY mismatch for {}: expected {}, got {}",
                        mailbox_name,
                        uid_validity,
                        select_uv
                    );
                }
            }
        }

        let uid_list = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetch = format!(
            "a003 UID FETCH {} (UID FLAGS INTERNALDATE RFC822.SIZE MODSEQ BODY.PEEK[])\r\n",
            uid_list
        );
        eprintln!(
            "  {}: UID FETCH {} (full message, {} uids)",
            mailbox_name,
            uid_list,
            uids.len()
        );
        writer.write_all(fetch.as_bytes()).await?;
        writer.flush().await?;

        let fetch_uid_re = Regex::new(r"UID (\d+)").unwrap();
        let fetch_flags_re = Regex::new(r"FLAGS \(([^)]*)\)").unwrap();
        let fetch_internaldate_re = Regex::new(r#"INTERNALDATE "([^"]+)"#).unwrap();
        let fetch_size_re = Regex::new(r"RFC822\.SIZE (\d+)").unwrap();
        let fetch_modseq_re = Regex::new(r"MODSEQ \(([^)]+)\)").unwrap();
        let fetch_body_re = Regex::new(r"BODY\[\] \{(\d+)\}").unwrap();
        let fetch_body_nil_re = Regex::new(r"BODY\[\] NIL").unwrap();
        let mut results = Vec::new();

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a003 ") {
                Self::check_imap_tag_response(&buf, "a003 ").context("IMAP UID FETCH failed")?;
                break;
            }
            if buf.starts_with("* ") && buf.contains(" FETCH ") {
                let line = buf.clone();
                let uid: u32 = fetch_uid_re
                    .captures(&line)
                    .and_then(|c| c.get(1))
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0);
                let is_read = fetch_flags_re
                    .captures(&line)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().contains("\\Seen"))
                    .unwrap_or(false);
                let internal_date = fetch_internaldate_re
                    .captures(&line)
                    .and_then(|c| c.get(1))
                    .and_then(|m| Self::parse_internaldate(m.as_str()));
                let message_size = fetch_size_re
                    .captures(&line)
                    .and_then(|c| c.get(1))
                    .and_then(|m| m.as_str().parse::<u32>().ok());
                let modseq = fetch_modseq_re
                    .captures(&line)
                    .and_then(|c| c.get(1))
                    .and_then(|m| {
                        // MODSEQ format: (modseq_value) or (modseq_value permsg-modseq)
                        m.as_str()
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse::<u64>().ok())
                    });

                if let Some(caps) = fetch_body_re.captures(&line) {
                    let lit_len: usize = caps
                        .get(1)
                        .context("parse IMAP response: missing literal length in BODY[] fetch")?
                        .as_str()
                        .parse()
                        .unwrap_or(0);
                    let mut lit = vec![0u8; lit_len];
                    reader.read_exact(&mut lit).await?;
                    let full_str = String::from_utf8_lossy(&lit);
                    let raw_content = full_str.to_string();

                    let mut res = FullMessageResult {
                        raw_email_content: raw_content.clone(),
                        is_read,
                        ..Default::default()
                    };

                    res.message_size = message_size;
                    res.modseq = modseq;

                    if let Ok(parsed) = parse_mail(raw_content.as_bytes()) {
                        if let Ok(h) = Self::parse_email_headers(&raw_content) {
                            res.message_id = h.message_id;
                            res.subject = h.subject;
                            res.sender = h.sender;
                            res.received_date = h.date.or(internal_date);
                            res.recipients_to = h.recipients_to;
                            res.recipients_cc = h.recipients_cc;
                            res.recipients_bcc = h.recipients_bcc;
                            res.list_unsubscribe = h.list_unsubscribe;
                            res.list_id = h.list_id;
                            res.x_priority = h.x_priority;
                            res.precedence = h.precedence;
                            res.return_path = h.return_path;
                            res.reply_to = h.reply_to;
                            res.custom_headers = h.custom_headers;
                            res.in_reply_to = h.in_reply_to;
                            res.references = h.references;
                        }
                        res.body_text = Self::extract_body_text_from_parsed(&parsed);
                    } else {
                        res.received_date = internal_date;
                    }

                    results.push((uid, res));
                    buf.clear();
                    reader.read_line(&mut buf).await?;
                } else if fetch_body_nil_re.is_match(&line) {
                    results.push((
                        uid,
                        FullMessageResult {
                            received_date: internal_date,
                            is_read,
                            message_size,
                            modseq,
                            ..Default::default()
                        },
                    ));
                }
            }
        }

        eprintln!(
            "  {}: full fetch returned {} messages",
            mailbox_name,
            results.len()
        );
        Ok(results)
    }

    /// Escape mailbox name for IMAP command arguments.
    /// Handles backslash, double-quote, and `&` (IMAP modified UTF-7: `&` → `&-`).
    fn escape_mailbox(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('&', "&-")
    }

    fn raw_imap_command_timeout() -> Duration {
        let secs = std::env::var("IMAP_RAW_COMMAND_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30)
            .max(1);
        Duration::from_secs(secs)
    }

    async fn read_raw_imap_line<R>(
        reader: &mut BufReader<R>,
        buf: &mut String,
        action: &str,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
    {
        let read = tokio::time::timeout(Self::raw_imap_command_timeout(), reader.read_line(buf))
            .await
            .with_context(|| format!("IMAP raw command timed out during {action}"))?
            .with_context(|| format!("IMAP raw command read failed during {action}"))?;
        if read == 0 {
            anyhow::bail!("IMAP raw command connection closed during {action}");
        }
        Ok(())
    }

    /// Raw IMAP CREATE folder. Uses dedicated connection (raw TCP). For use when creating folders for filing.
    pub async fn raw_create_folder(&self, folder_path: &str) -> Result<()> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };
        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);
        let mut buf = String::new();
        Self::read_raw_imap_line(&mut reader, &mut buf, "CREATE greeting").await?;
        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            Self::escape_mailbox(&self.username),
            Self::escape_mailbox(&self.password)
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "CREATE login response").await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }
        let create = format!("a002 CREATE \"{}\"\r\n", Self::escape_mailbox(folder_path));
        writer.write_all(create.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "CREATE response").await?;
            if buf.starts_with("a002 ") {
                if Self::is_mailbox_already_exists_response(&buf) {
                    return Ok(());
                }
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP CREATE failed")?;
                return Ok(());
            }
        }
    }

    /// Raw IMAP DELETE folder. Used to remove accidentally-created NOSELECT folders.
    pub async fn raw_delete_folder(&self, folder_path: &str) -> Result<()> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };
        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);
        let mut buf = String::new();
        Self::read_raw_imap_line(&mut reader, &mut buf, "DELETE greeting").await?;
        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            Self::escape_mailbox(&self.username),
            Self::escape_mailbox(&self.password)
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "DELETE login response").await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }
        let delete = format!("a002 DELETE \"{}\"\r\n", Self::escape_mailbox(folder_path));
        writer.write_all(delete.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "DELETE response").await?;
            if buf.starts_with("a002 ") {
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP DELETE failed")?;
                return Ok(());
            }
        }
    }

    /// Raw IMAP UID MOVE. Returns new UID and UID validity from COPYUID in the tagged OK (RFC 6851).
    pub async fn raw_uid_move(
        &self,
        from_mailbox: &str,
        uid: u32,
        to_mailbox: &str,
    ) -> Result<Option<MoveResult>> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };
        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);
        let mut buf = String::new();
        Self::read_raw_imap_line(&mut reader, &mut buf, "UID MOVE greeting").await?;
        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            Self::escape_mailbox(&self.username),
            Self::escape_mailbox(&self.password)
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "UID MOVE login response").await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }
        let sel = format!("a002 SELECT \"{}\"\r\n", Self::escape_mailbox(from_mailbox));
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "UID MOVE select response").await?;
            if buf.starts_with("a002 ") {
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP SELECT failed")?;
                break;
            }
        }
        let move_cmd = format!(
            "a003 UID MOVE {} \"{}\"\r\n",
            uid,
            Self::escape_mailbox(to_mailbox)
        );
        writer.write_all(move_cmd.as_bytes()).await?;
        writer.flush().await?;
        // COPYUID format: [COPYUID uidvalidity source-uids dest-uids].
        // Dovecot sends COPYUID on an untagged `* OK` line before the tagged `a003 OK`,
        // so we must check every response line, not just the tagged one.
        let copyuid_re = Regex::new(r"\[COPYUID\s+(\d+)\s+[^\s]+\s+([0-9:,]+)\]").unwrap();
        let mut move_result: Option<MoveResult> = None;
        loop {
            buf.clear();
            Self::read_raw_imap_line(&mut reader, &mut buf, "UID MOVE response").await?;
            // Check for COPYUID on any line (untagged or tagged)
            if move_result.is_none() {
                if let Some(caps) = copyuid_re.captures(&buf) {
                    let new_uid_validity: u32 = caps
                        .get(1)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(0);
                    let dest_uids = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    let new_uid = dest_uids
                        .split(|c: char| !c.is_ascii_digit())
                        .rfind(|s| !s.is_empty())
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    if new_uid > 0 {
                        move_result = Some(MoveResult {
                            new_uid,
                            new_uid_validity,
                        });
                    }
                }
            }
            if buf.starts_with("a003 ") {
                Self::check_imap_tag_response(&buf, "a003 ").context("IMAP UID MOVE failed")?;
                return Ok(move_result);
            }
        }
    }

    /// Move a message by UID from one mailbox to another using COPY + STORE + EXPUNGE fallback.
    pub async fn move_message(
        &self,
        source_mailbox: &str,
        uid: u32,
        dest_mailbox: &str,
    ) -> Result<()> {
        self.ensure_mailbox(dest_mailbox).await?;

        if self
            .raw_uid_move(source_mailbox, uid, dest_mailbox)
            .await
            .is_ok()
        {
            self.invalidate_mailbox_cache().await;
            return Ok(());
        }

        let source_mailbox = source_mailbox.to_string();
        let dest_mailbox = dest_mailbox.to_string();
        let uid_set = uid.to_string();
        self.with_active_session("move_message", |session, state| {
            Box::pin(async move {
                use futures_util::TryStreamExt;

                state.ensure_selected(session, &source_mailbox).await?;
                session
                    .uid_copy(
                        &uid_set,
                        format!("\"{}\"", Self::escape_mailbox(&dest_mailbox)),
                    )
                    .await
                    .context("UID COPY failed")?;
                session
                    .uid_store(&uid_set, "+FLAGS.SILENT (\\Deleted)")
                    .await
                    .context("UID STORE \\Deleted failed")?
                    .try_collect::<Vec<_>>()
                    .await
                    .context("collect UID STORE responses")?;
                let uid_expunge_error = match session.uid_expunge(&uid_set).await {
                    Ok(stream) => {
                        stream
                            .try_collect::<Vec<_>>()
                            .await
                            .context("collect UID EXPUNGE responses")?;
                        None
                    }
                    Err(error) => Some(error),
                };
                if let Some(error) = uid_expunge_error {
                    log::warn!(
                        "[imap] UID EXPUNGE failed for {} in {}: {}; falling back to EXPUNGE",
                        uid,
                        source_mailbox,
                        error
                    );
                    session
                        .expunge()
                        .await
                        .context("EXPUNGE failed")?
                        .try_collect::<Vec<_>>()
                        .await
                        .context("collect EXPUNGE responses")?;
                }
                Ok(())
            })
        })
        .await
    }

    /// Query UIDs with MODSEQ greater than threshold (for incremental sync).
    /// Uses UID SEARCH with MODSEQ criteria.
    pub async fn search_uids_by_modseq(
        &self,
        mailbox_name: &str,
        modseq_threshold: u64,
    ) -> Result<Vec<u32>> {
        let mailbox_name = mailbox_name.to_string();
        self.with_active_session("search_uids_by_modseq", |session, state| {
            Box::pin(async move {
                state.ensure_selected(session, &mailbox_name).await?;

                // UID SEARCH MODSEQ <threshold>:* returns UIDs with MODSEQ >= threshold
                let query = format!("MODSEQ {}:*", modseq_threshold);
                let uids =
                    tokio::time::timeout(Duration::from_secs(30), session.uid_search(&query))
                        .await
                        .context("Timeout searching by MODSEQ")?
                        .context("Failed to search UIDs by MODSEQ")?;
                let mut vec: Vec<u32> = uids.into_iter().collect();
                vec.sort_unstable();
                Ok(vec)
            })
        })
        .await
    }

    /// Fetch flags and MODSEQ for multiple UIDs (for detecting flag changes).
    pub async fn fetch_flags_and_modseq(
        &self,
        mailbox_name: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, bool, Option<u64>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let mailbox_name = mailbox_name.to_string();
        let uids = uids.to_vec();
        self.with_active_session("fetch_flags_and_modseq", |session, state| {
            Box::pin(async move {
                state.ensure_selected(session, &mailbox_name).await?;

                let uid_list = uids
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let fetch_stream = tokio::time::timeout(
                    Duration::from_secs(30),
                    session.uid_fetch(&uid_list, "FLAGS MODSEQ"),
                )
                .await
                .context("Timeout fetching flags and MODSEQ")?
                .context("Failed to fetch flags and MODSEQ")?;

                use futures_util::TryStreamExt;
                let messages: Vec<_> = fetch_stream
                    .try_collect()
                    .await
                    .context("Failed to collect fetch stream")?;

                let mut results = Vec::new();
                for msg in messages {
                    let uid = msg
                        .uid
                        .ok_or_else(|| anyhow::anyhow!("Message missing UID"))?;
                    let flags: Vec<_> = msg.flags().collect();
                    let is_read = flags
                        .iter()
                        .any(|f| matches!(f, async_imap::types::Flag::Seen));
                    let modseq = msg.modseq;
                    results.push((uid, is_read, modseq));
                }
                Ok(results)
            })
        })
        .await
    }

    /// Get highest MODSEQ for a mailbox by parsing SELECT response with CONDSTORE.
    /// Falls back to fetching MODSEQ from messages if HIGHESTMODSEQ not in response.
    pub async fn get_highest_modseq(&self, mailbox_name: &str) -> Result<Option<u64>> {
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);

        let mut buf = String::new();
        reader.read_line(&mut buf).await?;

        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            self.username.replace('\\', "\\\\").replace('"', "\\\""),
            self.password.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }

        // SELECT with CONDSTORE to get HIGHESTMODSEQ
        let sel = format!(
            "a002 SELECT \"{}\" (CONDSTORE)\r\n",
            mailbox_name.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;

        let highestmodseq_re = Regex::new(r"HIGHESTMODSEQ (\d+)").unwrap();
        let mut highest_modseq = None;

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a002 ") {
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP SELECT failed")?;
                break;
            }
            // Look for HIGHESTMODSEQ in response
            if let Some(caps) = highestmodseq_re.captures(&buf) {
                if let Ok(modseq) = caps
                    .get(1)
                    .context("parse IMAP response: missing HIGHESTMODSEQ value")?
                    .as_str()
                    .parse::<u64>()
                {
                    highest_modseq = Some(modseq);
                }
            }
        }

        Ok(highest_modseq)
    }

    /// Get mailbox state and check for new UIDs (UIDs > last_synced_uid).
    pub async fn get_new_uids(&self, mailbox_name: &str, last_synced_uid: u32) -> Result<Vec<u32>> {
        let mailbox_name = mailbox_name.to_string();
        self.with_active_session("get_new_uids", |session, state| {
            Box::pin(async move {
                let mailbox =
                    tokio::time::timeout(Duration::from_secs(10), session.select(&mailbox_name))
                        .await
                        .context("Timeout selecting mailbox")?
                        .context("Failed to select mailbox")?;
                state.selected_mailbox = Some(mailbox_name.clone());

                let Some(query) = new_uid_search_query(last_synced_uid, mailbox.uid_next) else {
                    return Ok(Vec::new());
                };
                let uids =
                    tokio::time::timeout(Duration::from_secs(30), session.uid_search(&query))
                        .await
                        .context("Timeout searching for new UIDs")?
                        .context("Failed to search for new UIDs")?;
                let mut vec: Vec<u32> = uids
                    .into_iter()
                    .filter(|uid| *uid > last_synced_uid)
                    .collect();
                vec.sort_unstable();
                Ok(vec)
            })
        })
        .await
    }

    /// Get expunged UIDs using QRESYNC SELECT. Returns UIDs that were deleted from the mailbox.
    pub async fn get_expunged_uids_qresync(
        &self,
        mailbox_name: &str,
        uid_validity: u32,
        known_uids: &[u32],
    ) -> Result<Vec<u32>> {
        // Use QRESYNC SELECT to get expunged UIDs
        // Format: SELECT mailbox (QRESYNC (uidvalidity known_uids known_modseq))
        let (host, port) = if let Some((h, p)) = self.server.split_once(':') {
            (h, p.parse::<u16>().context("Invalid port")?)
        } else {
            (self.server.as_str(), 993)
        };

        let tls_stream = connect_tls_tcp(host, port).await?;
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = BufReader::new(reader);

        let mut buf = String::new();
        reader.read_line(&mut buf).await?;

        let login = format!(
            "a001 LOGIN \"{}\" \"{}\"\r\n",
            self.username.replace('\\', "\\\\").replace('"', "\\\""),
            self.password.replace('\\', "\\\\").replace('"', "\\\"")
        );
        writer.write_all(login.as_bytes()).await?;
        writer.flush().await?;
        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a001 ") {
                Self::check_imap_tag_response(&buf, "a001 ").context("IMAP LOGIN failed")?;
                break;
            }
        }

        // QRESYNC SELECT: SELECT mailbox (QRESYNC (uidvalidity known_uids known_modseq))
        // For simplicity, we'll use VANISHED which returns expunged UIDs
        // Format: VANISHED (earlier) <uid_set>
        let known_uids_str = if known_uids.is_empty() {
            "1:*".to_string()
        } else {
            known_uids
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let qresync = format!("(QRESYNC ({} {} 1))", uid_validity, known_uids_str);
        let sel = format!(
            "a002 SELECT \"{}\" {}\r\n",
            mailbox_name.replace('\\', "\\\\").replace('"', "\\\""),
            qresync
        );
        writer.write_all(sel.as_bytes()).await?;
        writer.flush().await?;

        let vanished_re = Regex::new(r"VANISHED(?: \(EARLIER\))? \(([^)]+)\)").unwrap();
        let mut expunged_uids = Vec::new();

        loop {
            buf.clear();
            reader.read_line(&mut buf).await?;
            if buf.starts_with("a002 ") {
                Self::check_imap_tag_response(&buf, "a002 ").context("IMAP SELECT failed")?;
                break;
            }
            // Parse VANISHED response
            if buf.contains("VANISHED") {
                if let Some(caps) = vanished_re.captures(&buf) {
                    let uid_set = caps
                        .get(1)
                        .context("parse IMAP response: missing VANISHED UID set")?
                        .as_str();
                    // Parse UID set (e.g., "1,2,3" or "1:5")
                    for part in uid_set.split(',') {
                        if let Some((start, end)) = part.split_once(':') {
                            let start_uid: u32 = start.parse().unwrap_or(0);
                            let end_uid: u32 = end.parse().unwrap_or(start_uid);
                            for uid in start_uid..=end_uid {
                                expunged_uids.push(uid);
                            }
                        } else if let Ok(uid) = part.parse::<u32>() {
                            expunged_uids.push(uid);
                        }
                    }
                }
            }
        }

        Ok(expunged_uids)
    }

    /// Detect expunged UIDs by comparing current UIDs with known UIDs (fallback if QRESYNC not available).
    pub async fn detect_expunged_uids_by_comparison(
        &self,
        mailbox_name: &str,
        known_uids: &[u32],
    ) -> Result<Vec<u32>> {
        // Get all current UIDs in mailbox
        let current_uids = match self.search_uids(mailbox_name, "ALL").await {
            Ok(uids) => uids,
            Err(_) => return Ok(Vec::new()),
        };

        let current_set: std::collections::HashSet<u32> = current_uids.into_iter().collect();
        let known_set: std::collections::HashSet<u32> = known_uids.iter().copied().collect();

        // UIDs that were in known but not in current = expunged
        let expunged: Vec<u32> = known_set.difference(&current_set).copied().collect();

        Ok(expunged)
    }
}

fn new_uid_search_query(last_synced_uid: u32, uid_next: Option<u32>) -> Option<String> {
    let start_uid = last_synced_uid.saturating_add(1);
    if let Some(uid_next) = uid_next {
        if start_uid >= uid_next {
            return None;
        }
        return Some(format!("UID {}:{}", start_uid, uid_next.saturating_sub(1)));
    }
    Some(format!("UID {}:*", start_uid))
}

#[cfg(test)]
mod tests {
    use super::{new_uid_search_query, tls_server_name_for_host, ImapClient};
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    fn clear_tls_test_env() {
        std::env::remove_var("IMAP_TLS_SERVER_NAME");
    }

    #[test]
    fn create_already_exists_response_is_success_equivalent() {
        assert!(ImapClient::is_mailbox_already_exists_response(
            "a002 NO [ALREADYEXISTS] Mailbox already exists"
        ));
        assert!(ImapClient::is_mailbox_already_exists_response(
            "a002 NO Mailbox already exists"
        ));
        assert!(!ImapClient::is_mailbox_already_exists_response(
            "a002 NO Permission denied"
        ));
    }

    #[test]
    fn new_uid_search_query_avoids_reversed_open_ended_ranges() {
        assert_eq!(new_uid_search_query(3278, Some(3279)), None);
        assert_eq!(
            new_uid_search_query(3278, Some(3281)).as_deref(),
            Some("UID 3279:3280")
        );
        assert_eq!(
            new_uid_search_query(3278, None).as_deref(),
            Some("UID 3279:*")
        );
    }

    #[test]
    fn tls_server_name_defaults_to_connection_host() {
        let _guard = env_lock().lock().expect("env lock");
        clear_tls_test_env();

        assert_eq!(
            tls_server_name_for_host("imap.example.com").expect("server name"),
            "imap.example.com"
        );
    }

    #[test]
    fn tls_server_name_uses_explicit_override() {
        let _guard = env_lock().lock().expect("env lock");
        clear_tls_test_env();
        std::env::set_var("IMAP_TLS_SERVER_NAME", " mail.example.com ");

        assert_eq!(
            tls_server_name_for_host("10.0.0.20").expect("server name"),
            "mail.example.com"
        );

        clear_tls_test_env();
    }

    #[test]
    fn tls_server_name_rejects_empty_override() {
        let _guard = env_lock().lock().expect("env lock");
        clear_tls_test_env();
        std::env::set_var("IMAP_TLS_SERVER_NAME", " ");

        assert!(tls_server_name_for_host("imap.example.com").is_err());

        clear_tls_test_env();
    }
}
