use anyhow::Context;
use futures::stream::{self, StreamExt};
use std::sync::Arc;

use crate::{
    ai,
    config::{AccountConfig, DEFAULT_ACCOUNT_ID},
    db, imap, location_analysis, metrics, rate_limit,
};

use super::shared::{load_agent_specs_dir, DEFAULT_ENV_PATH};

const DEFAULT_LOCATE_LIMIT: u32 = 50;

/// Normalize each segment of a folder path to Title Case to prevent
/// case-variant duplicates like "work" vs "Work".  Respects the
/// `FOLDER_CASING_POLICY` env var (default: "title-case").
fn normalize_folder_path(path: &str, delimiter: &str) -> String {
    let policy = std::env::var("FOLDER_CASING_POLICY").unwrap_or_else(|_| "title-case".to_string());
    path.split(delimiter)
        .filter(|s| !s.is_empty())
        .map(|segment| {
            let sanitized = sanitize_folder_segment_for_imap(segment);
            if policy == "preserve" {
                sanitized
            } else {
                canonical_folder_segment_casing(&to_title_case(&sanitized))
            }
        })
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(delimiter)
}

fn canonicalize_location_folder_path(path: &str, delimiter: &str) -> String {
    let normalized = normalize_folder_path(path, delimiter);
    let segments: Vec<&str> = normalized
        .split(delimiter)
        .filter(|segment| !segment.is_empty())
        .collect();
    let lower_segments: Vec<String> = segments
        .iter()
        .map(|segment| segment.to_ascii_lowercase())
        .collect();

    match (
        lower_segments.first().map(String::as_str),
        lower_segments.get(1).map(String::as_str),
        segments.len(),
    ) {
        (Some("security"), None, 1) => ["Personal", "Security", "Alerts"].join(delimiter),
        (Some("security"), Some(_), 2) => ["Personal", "Security", segments[1]].join(delimiter),
        (Some("personal"), Some("security"), 2) => {
            ["Personal", "Security", "Alerts"].join(delimiter)
        }
        (Some("personal"), None, 1) => ["Personal", "General"].join(delimiter),
        (Some("work"), None, 1) => ["Work", "General"].join(delimiter),
        (Some("financial"), None, 1) => ["Financial", "General"].join(delimiter),
        (Some("social"), None, 1) => ["Social", "General"].join(delimiter),
        (Some("education"), None, 1) => ["Education", "General"].join(delimiter),
        (Some("health"), None, 1) => ["Health", "General"].join(delimiter),
        (Some("travel"), None, 1) => ["Travel", "General"].join(delimiter),
        (Some("newsletters"), None, 1) => ["Newsletters", "General"].join(delimiter),
        (Some("personal"), Some("property"), 2) => {
            ["Personal", "Property", "General"].join(delimiter)
        }
        (Some("personal"), Some("shopping"), 2) => {
            ["Personal", "Shopping", "General"].join(delimiter)
        }
        _ => normalized,
    }
}

fn sanitize_folder_segment_for_imap(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for ch in segment.trim().chars() {
        match ch {
            '&' => out.push_str(" And "),
            '.' | ':' | ';' | '"' | '\'' | '`' | '*' | '?' | '<' | '>' | '|' | '\\' => {
                out.push(' ');
            }
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn to_title_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for ch in s.chars() {
        if ch == ' ' || ch == '-' || ch == '_' {
            result.push(ch);
            capitalize_next = true;
        } else if capitalize_next {
            result.extend(ch.to_uppercase());
            capitalize_next = false;
        } else {
            result.extend(ch.to_lowercase());
        }
    }
    result
}

fn canonical_folder_segment_casing(segment: &str) -> String {
    match segment.to_ascii_lowercase().as_str() {
        "github" => "GitHub".to_string(),
        "linkedin" => "LinkedIn".to_string(),
        "otp" => "OTP".to_string(),
        _ => segment.to_string(),
    }
}

fn effective_provider_name(config: &ai::AIConfig) -> String {
    let provider = config.provider.to_lowercase();
    if provider == "hybrid" {
        config
            .frontier_provider
            .clone()
            .unwrap_or_else(|| "gemini".to_string())
            .to_lowercase()
    } else {
        provider
    }
}

fn locate_concurrency_from_env(config: &ai::AIConfig, provider_is_local: bool) -> usize {
    if let Some(value) = std::env::var("LOCATE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        return value.max(1);
    }
    if let Some(value) = std::env::var("LOCAL_LLM_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        return value.max(1);
    }

    let provider = config.provider.to_lowercase();
    let local_provider = matches!(provider.as_str(), "lmstudio" | "local" | "ollama" | "omlx");
    if local_provider || provider_is_local {
        2
    } else {
        1
    }
}

struct LocateRecordOutcome {
    message_id: String,
    error: Option<anyhow::Error>,
}

async fn locate_one_email_for_account(
    db: Arc<db::Database>,
    agent: &location_analysis::LocationAgent,
    account_id: &str,
    email: db::EmailRecord,
) -> LocateRecordOutcome {
    let started = std::time::Instant::now();
    let message_id = email.message_id.clone();

    let outcome = match agent.recommend_location(&email).await {
        Ok(rec) => {
            let normalized = canonicalize_location_folder_path(&rec.location_recommendation, "/");
            if let Err(error) = db
                .update_location_recommendation_for_account(
                    account_id,
                    &message_id,
                    &normalized,
                    rec.create_if_missing,
                )
                .await
            {
                eprintln!("Failed to save {}: {}", message_id, error);
                metrics::counter("locate_save_failed_total", 1, &[]);
                Some(error)
            } else {
                if let Some(reason) = rec.reason.as_deref().filter(|reason| !reason.is_empty()) {
                    println!(
                        "Located: {} -> {} (create_if_missing: {}) - {}",
                        message_id, normalized, rec.create_if_missing, reason
                    );
                } else {
                    println!(
                        "Located: {} -> {} (create_if_missing: {})",
                        message_id, normalized, rec.create_if_missing
                    );
                }
                metrics::counter("locate_success_total", 1, &[]);
                None
            }
        }
        Err(error) => {
            eprintln!("Failed {}: {}", message_id, error);
            metrics::counter("locate_failed_total", 1, &[]);
            Some(error)
        }
    };

    metrics::histogram(
        "locate_latency_seconds",
        started.elapsed().as_secs_f64(),
        &[],
    );

    LocateRecordOutcome {
        message_id,
        error: outcome,
    }
}

/// Find rows with location IS NULL (orphans), search all folders by Message-ID, update location when found.
pub async fn run_resolve_orphans() -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;

    let orphans = database
        .get_orphan_message_ids()
        .await
        .context("Get orphan message_ids")?;
    if orphans.is_empty() {
        println!("No orphan messages (location IS NULL).");
        return Ok(());
    }
    println!(
        "Found {} orphan message_id(s); searching folders...",
        orphans.len()
    );

    let (server, username, password) = imap::load_imap_from_env().context("Load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let folders: Vec<String> = database
        .list_imap_folders()
        .await
        .context("List folders")?
        .into_iter()
        .filter(|f| !f.is_noselect)
        .map(|f| f.folder_name)
        .collect();

    if folders.is_empty() {
        println!("No folders in imap_folders. Run sync first.");
        return Ok(());
    }

    let mut resolved = 0u32;
    let mut not_found: Vec<&str> = Vec::new();
    for message_id in &orphans {
        let mut found = false;
        for folder in &folders {
            if let Ok(uids) = client.search_uids_by_message_id(folder, message_id).await {
                if let Some(&uid) = uids.first() {
                    let uid_validity = database
                        .get_folder_uid_validity(folder)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(0);
                    if database
                        .update_email_location(message_id, folder, uid as i32, uid_validity)
                        .await
                        .is_ok()
                    {
                        println!("  {} -> {} (uid {})", message_id, folder, uid);
                        resolved += 1;
                        found = true;
                        break;
                    }
                }
            }
        }
        if !found {
            not_found.push(message_id.as_str());
        }
    }

    for message_id in &not_found {
        if database
            .mark_email_deleted_from_server(message_id)
            .await
            .is_ok()
        {
            println!(
                "  {} -> deleted from server (not found in any folder)",
                message_id
            );
        }
    }
    let deleted_count = not_found.len() as u32;
    println!(
        "Resolved {} orphan(s), marked {} as deleted from server (of {} total).",
        resolved,
        deleted_count,
        orphans.len()
    );
    Ok(())
}

pub async fn run_locate_with_limit(
    message_id: Option<String>,
    force: bool,
    limit_override: Option<usize>,
) -> anyhow::Result<()> {
    run_locate_with_limit_for_account(message_id, force, limit_override, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_locate_with_limit_for_account(
    message_id: Option<String>,
    force: bool,
    limit_override: Option<usize>,
    account_id: &str,
) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let limit: u32 = limit_override
        .map(|v| v.min(u32::MAX as usize) as u32)
        .unwrap_or_else(|| {
            std::env::var("LOCATE_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_LOCATE_LIMIT)
        });
    let max_iterations = std::env::var("AI_MAX_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);

    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let provider_name = effective_provider_name(&ai_config);
    let provider_box = ai::create_provider(&ai_config).context("Create AI provider")?;
    let provider: Arc<dyn ai::AIProvider> = Arc::from(provider_box);
    let provider_is_local = provider.is_local();
    let locate_concurrency = locate_concurrency_from_env(&ai_config, provider_is_local);
    let provider = if provider_is_local {
        provider
    } else {
        rate_limit::wrap_ai_provider(provider, ai_config.rate_limit_for_provider(&provider_name))
    };

    let rag = super::shared::create_rag_builder(db.clone(), Some(&ai_config)).await?;
    let agents_dir = load_agent_specs_dir();
    let agent = Arc::new(
        location_analysis::LocationAgent::new(provider, db.clone(), max_iterations)
            .with_rag(rag)
            .with_agent_specs(agents_dir)
            .with_account_id(account_id),
    );

    let emails: Vec<db::EmailRecord> = if let Some(id) = message_id {
        if force {
            println!("--force is ignored for single-message locate.");
        }
        let email = db
            .get_email_by_message_id_for_account(account_id, &id)
            .await
            .context("Fetch email by message_id")?
            .ok_or_else(|| anyhow::anyhow!("No email found with message_id: {}", id))?;
        println!("Locating one record: {}", id);
        vec![email]
    } else {
        let list = db
            .get_emails_needing_location_for_account(account_id, limit, force)
            .await
            .context("Get emails needing location")?;
        if force {
            println!(
                "Locating {} emails (limit {}, force mode: relaunch location selection)",
                list.len(),
                limit
            );
        } else {
            println!("Locating {} emails (limit {})", list.len(), limit);
        }
        list
    };

    let single = emails.len() == 1;
    let concurrency = if single {
        1
    } else {
        locate_concurrency.max(1).min(emails.len().max(1))
    };
    if concurrency > 1 {
        println!("Locating with concurrency {}", concurrency);
    }

    let email_count = emails.len();
    let outcomes = if concurrency == 1 {
        let mut outcomes = Vec::with_capacity(emails.len());
        for email in &emails {
            let email = email.clone();
            outcomes.push(
                locate_one_email_for_account(db.clone(), agent.as_ref(), account_id, email).await,
            );
        }
        outcomes
    } else {
        let email_stream =
            stream::iter(
                emails.into_iter().map(|email| {
                    let db = db.clone();
                    let agent = agent.clone();
                    async move {
                        locate_one_email_for_account(db, agent.as_ref(), account_id, email).await
                    }
                }),
            );
        email_stream
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await
    };

    let ok = outcomes
        .iter()
        .filter(|outcome| outcome.error.is_none())
        .count();
    let err = outcomes.len().saturating_sub(ok);
    for outcome in outcomes.iter().filter(|outcome| outcome.error.is_some()) {
        log::warn!(
            "[locate] completed batch with failed item {}",
            outcome.message_id
        );
    }
    println!("Located {} of {} emails ({} errors)", ok, email_count, err);
    Ok(())
}

/// Apply location recommendations: create folder if needed, MOVE email, update DB. --dry-run only prints.
pub async fn run_file(dry_run: bool) -> anyhow::Result<()> {
    run_file_for_account(dry_run, DEFAULT_ACCOUNT_ID).await
}

/// Check whether the leaf folder in a path is selectable. Intermediate
/// NOSELECT segments (container-only folders) are normal in IMAP and do
/// NOT prevent the leaf from being selectable.
fn is_path_selectable_in_folders(path: &str, folders: &[db::ImapFolder], _delimiter: &str) -> bool {
    if let Some(folder) = folders
        .iter()
        .find(|f| f.folder_name.eq_ignore_ascii_case(path))
    {
        return !folder.is_noselect;
    }
    // Folder not in DB yet — it will be created, so treat as selectable.
    true
}

fn find_folder_case_insensitive<'a>(
    folders: &'a [db::ImapFolder],
    path: &str,
) -> Option<&'a db::ImapFolder> {
    folders
        .iter()
        .find(|f| f.folder_name.eq_ignore_ascii_case(path))
}

fn should_create_missing_folder_for_filing(
    target: &str,
    delimiter: &str,
    create_if_missing: bool,
) -> bool {
    if create_if_missing {
        return true;
    }
    let top_level = target.split(delimiter).find(|segment| !segment.is_empty());
    matches!(
        top_level,
        Some("Work")
            | Some("Personal")
            | Some("Financial")
            | Some("Social")
            | Some("Education")
            | Some("Health")
            | Some("Travel")
            | Some("Newsletters")
            | Some("Security")
    )
}

fn filing_move_cooldown_hours_from_env() -> i64 {
    std::env::var("MAIL_ASSISTANT_MOVE_COOLDOWN_HOURS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(720)
        .max(1)
}

async fn refresh_folders_from_imap_for_account(
    client: &imap::ImapClient,
    db: &Arc<db::Database>,
    account_id: &str,
) -> anyhow::Result<Vec<db::ImapFolder>> {
    let list = client
        .refresh_mailboxes_with_attributes()
        .await
        .context("refresh IMAP folder list")?;
    db.sync_folders_from_imap_for_account(account_id, &list)
        .await
        .context("sync refreshed IMAP folder list")?;
    db.list_imap_folders_for_account(account_id)
        .await
        .context("reload refreshed IMAP folder list")
}

fn selectable_folder_name_after_create(
    folders: &[db::ImapFolder],
    requested: &str,
) -> anyhow::Result<Option<String>> {
    let Some(existing) = find_folder_case_insensitive(folders, requested) else {
        return Ok(None);
    };
    if existing.is_noselect {
        anyhow::bail!("folder {} exists but is NOSELECT", existing.folder_name);
    }
    Ok(Some(existing.folder_name.clone()))
}

async fn ensure_folder_path_for_filing(
    client: &imap::ImapClient,
    db: &Arc<db::Database>,
    account_id: &str,
    target: &str,
    delimiter: &str,
    mut folders: Vec<db::ImapFolder>,
) -> anyhow::Result<(String, Vec<db::ImapFolder>)> {
    if let Some(existing) = selectable_folder_name_after_create(&folders, target)? {
        return Ok((existing, folders));
    }

    let mut direct_create_error = None;
    match client.raw_create_folder(target).await {
        Ok(()) => {
            folders = refresh_folders_from_imap_for_account(client, db, account_id).await?;
            if let Some(existing) = selectable_folder_name_after_create(&folders, target)? {
                return Ok((existing, folders));
            }
        }
        Err(error) => {
            direct_create_error = Some(format!("{:#}", error));
            folders = refresh_folders_from_imap_for_account(client, db, account_id).await?;
            if let Some(existing) = selectable_folder_name_after_create(&folders, target)? {
                return Ok((existing, folders));
            }
        }
    }

    let segments: Vec<&str> = target
        .split(delimiter)
        .filter(|segment| !segment.is_empty())
        .collect();
    let last_index = segments.len().saturating_sub(1);
    let mut prefix = String::new();

    for (index, segment) in segments.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push_str(delimiter);
        }
        prefix.push_str(segment);

        if let Some(existing) = find_folder_case_insensitive(&folders, &prefix) {
            prefix = existing.folder_name.clone();
            if index == last_index && existing.is_noselect {
                anyhow::bail!("folder {} exists but is NOSELECT", existing.folder_name);
            }
            continue;
        }

        let requested_prefix = prefix.clone();
        match client.raw_create_folder(&requested_prefix).await {
            Ok(()) => {}
            Err(error) => {
                folders = refresh_folders_from_imap_for_account(client, db, account_id).await?;
                if let Some(existing) = find_folder_case_insensitive(&folders, &requested_prefix) {
                    prefix = existing.folder_name.clone();
                    if index == last_index && existing.is_noselect {
                        anyhow::bail!("folder {} exists but is NOSELECT", existing.folder_name);
                    }
                    continue;
                }
                let mut message = format!("CREATE {} failed: {:#}", requested_prefix, error);
                if let Some(direct_error) = &direct_create_error {
                    message.push_str(&format!(
                        "; direct CREATE {} also failed: {}",
                        target, direct_error
                    ));
                }
                anyhow::bail!(message);
            }
        }

        folders = refresh_folders_from_imap_for_account(client, db, account_id).await?;
        let Some(existing) = find_folder_case_insensitive(&folders, &requested_prefix) else {
            anyhow::bail!(
                "CREATE {} succeeded but folder was not visible after refresh",
                requested_prefix
            );
        };
        prefix = existing.folder_name.clone();
        if index == last_index && existing.is_noselect {
            anyhow::bail!("folder {} exists but is NOSELECT", existing.folder_name);
        }
    }

    Ok((prefix, folders))
}

pub async fn run_file_for_account(dry_run: bool, account_id: &str) -> anyhow::Result<()> {
    let started_all = std::time::Instant::now();
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);

    let pending: Vec<db::PendingLocationApply> = db
        .get_emails_with_pending_location_for_account(account_id)
        .await
        .context("Get pending location")?;
    if pending.is_empty() {
        println!("No emails with pending location recommendation.");
        return Ok(());
    }

    let folders_before = db.list_imap_folders_for_account(account_id).await?;
    let delimiter = folders_before
        .first()
        .and_then(|f| f.delimiter.as_deref())
        .unwrap_or("/");

    if dry_run {
        let mut planned_moves = Vec::new();
        let mut already_in_place = 0usize;
        for p in &pending {
            let target =
                canonicalize_location_folder_path(p.location_recommendation.trim(), delimiter);
            if p.location
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(&target))
            {
                already_in_place += 1;
                continue;
            }
            let normalized_note = if target != p.location_recommendation {
                format!(" (normalized from {})", p.location_recommendation)
            } else {
                String::new()
            };
            planned_moves.push(format!(
                "  {} | {} -> {}{} | create_if_missing: {:?}",
                p.message_id,
                p.location.as_deref().unwrap_or("(null)"),
                target,
                normalized_note,
                p.location_create_if_missing
            ));
        }
        println!("Dry run: would move {} email(s)", planned_moves.len());
        if already_in_place > 0 {
            println!(
                "  {} email(s) already match their canonical target after normalization",
                already_in_place
            );
        }
        metrics::counter("file_dry_run_total", planned_moves.len() as u64, &[]);
        for line in planned_moves {
            println!("{}", line);
        }
        return Ok(());
    }

    let account = AccountConfig::load(account_id).context("Load account config")?;
    let client = imap::ImapClient::new(
        account.imap_server(),
        account.username.clone(),
        account.password.clone(),
    );

    let mut filed = 0usize;
    let mut skipped = 0usize;
    let mut already_in_place = 0usize;
    let filing_cooldown_hours = filing_move_cooldown_hours_from_env();

    for p in &pending {
        let current = p.location.as_deref().unwrap_or("");
        let mut target =
            canonicalize_location_folder_path(p.location_recommendation.trim(), delimiter);
        if target != p.location_recommendation {
            db.update_location_recommendation_for_account(
                account_id,
                &p.message_id,
                &target,
                p.location_create_if_missing.unwrap_or(false),
            )
            .await
            .with_context(|| {
                format!(
                    "normalize stored location recommendation for {}",
                    p.message_id
                )
            })?;
        }
        if current.eq_ignore_ascii_case(&target) {
            already_in_place += 1;
            continue;
        }
        let mut folders = db.list_imap_folders_for_account(account_id).await?;
        if let Some(existing) = find_folder_case_insensitive(&folders, &target) {
            target = existing.folder_name.clone();
        } else {
            folders = refresh_folders_from_imap_for_account(&client, &db, account_id).await?;
            if let Some(existing) = find_folder_case_insensitive(&folders, &target) {
                target = existing.folder_name.clone();
            }
        }
        if !is_path_selectable_in_folders(&target, &folders, delimiter) {
            eprintln!(
                "Skip {}: target folder is not selectable: {}",
                p.message_id, target
            );
            skipped += 1;
            continue;
        }
        let create_if_missing = p.location_create_if_missing.unwrap_or(false);
        let target_exists = folders
            .iter()
            .any(|f| f.folder_name.eq_ignore_ascii_case(&target));
        let should_create =
            should_create_missing_folder_for_filing(&target, delimiter, create_if_missing);
        if !target_exists && !should_create {
            eprintln!(
                "Skip {}: target folder {} does not exist and create_if_missing is false",
                p.message_id, target
            );
            skipped += 1;
            continue;
        }
        if !target_exists {
            if !create_if_missing {
                println!(
                    "Creating missing canonical folder for {}: {}",
                    p.message_id, target
                );
            }
            match ensure_folder_path_for_filing(
                &client, &db, account_id, &target, delimiter, folders,
            )
            .await
            {
                Ok((actual_target, refreshed_folders)) => {
                    target = actual_target;
                    folders = refreshed_folders;
                }
                Err(error) => {
                    eprintln!(
                        "Skip {}: ensure target folder {} failed: {:#}",
                        p.message_id, target, error
                    );
                    skipped += 1;
                    continue;
                }
            }
        }
        if !is_path_selectable_in_folders(&target, &folders, delimiter) {
            eprintln!(
                "Skip {}: target folder {} is not selectable",
                p.message_id, target
            );
            skipped += 1;
            continue;
        }

        let from_mailbox: &str = match p.location.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                eprintln!("Skip {}: no current location", p.message_id);
                skipped += 1;
                continue;
            }
        };
        let uid = match p.uid {
            Some(u) => u as u32,
            None => {
                eprintln!("Skip {}: no UID", p.message_id);
                skipped += 1;
                continue;
            }
        };
        match client.raw_uid_move(from_mailbox, uid, &target).await {
            Ok(Some(move_result)) => {
                if db
                    .record_system_filing_move_for_account(db::SystemFilingMoveRecord {
                        account_id,
                        message_id: &p.message_id,
                        location: &target,
                        uid: move_result.new_uid as i32,
                        uid_validity: move_result.new_uid_validity as i32,
                        actor: "core",
                        cooldown_hours: filing_cooldown_hours,
                    })
                    .await
                    .is_ok()
                {
                    println!(
                        "Filed: {} -> {} (uid {})",
                        p.message_id, target, move_result.new_uid
                    );
                    filed += 1;
                    metrics::counter("file_move_success_total", 1, &[]);
                } else {
                    eprintln!("Filed {} but failed to update DB", p.message_id);
                    skipped += 1;
                    metrics::counter("file_move_failed_total", 1, &[]);
                }
            }
            Ok(None) => match client
                .search_uids_by_message_id(&target, &p.message_id)
                .await
            {
                Ok(uids) if !uids.is_empty() => {
                    let new_uid = uids[0] as i32;
                    let new_uid_validity = db
                        .get_folder_uid_validity_for_account(account_id, &target)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(0);
                    if db
                        .record_system_filing_move_for_account(db::SystemFilingMoveRecord {
                            account_id,
                            message_id: &p.message_id,
                            location: &target,
                            uid: new_uid,
                            uid_validity: new_uid_validity,
                            actor: "core",
                            cooldown_hours: filing_cooldown_hours,
                        })
                        .await
                        .is_ok()
                    {
                        println!(
                            "Filed (search fallback): {} -> {} (uid {})",
                            p.message_id, target, new_uid
                        );
                        filed += 1;
                        metrics::counter("file_move_success_total", 1, &[]);
                    } else {
                        eprintln!(
                            "Filed {} but failed to update DB after search fallback",
                            p.message_id
                        );
                        skipped += 1;
                        metrics::counter("file_move_failed_total", 1, &[]);
                    }
                }
                Ok(_) => {
                    eprintln!(
                        "Skip {}: MOVE returned no COPYUID and search in {} found no UID",
                        p.message_id, target
                    );
                    skipped += 1;
                    metrics::counter("file_move_failed_total", 1, &[]);
                }
                Err(e) => {
                    eprintln!(
                        "Skip {}: MOVE returned no COPYUID and search in {} failed: {}",
                        p.message_id, target, e
                    );
                    skipped += 1;
                    metrics::counter("file_move_failed_total", 1, &[]);
                }
            },
            Err(e) => {
                eprintln!("Skip {}: MOVE failed: {}", p.message_id, e);
                skipped += 1;
                metrics::counter("file_move_failed_total", 1, &[]);
            }
        }
    }

    println!(
        "Filed {} of {} emails ({} skipped, {} already in place)",
        filed,
        pending.len(),
        skipped,
        already_in_place
    );
    metrics::histogram(
        "file_apply_latency_seconds",
        started_all.elapsed().as_secs_f64(),
        &[],
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, is_noselect: bool) -> db::ImapFolder {
        db::ImapFolder {
            folder_name: name.to_string(),
            delimiter: Some("/".to_string()),
            is_noselect,
            attributes: Vec::new(),
            last_synced_uid: None,
            last_full_sync_uid: None,
            message_count: None,
            priority: None,
        }
    }

    #[test]
    fn canonical_filing_paths_can_be_auto_created() {
        assert!(should_create_missing_folder_for_filing(
            "Work/Dev", "/", false
        ));
        assert!(should_create_missing_folder_for_filing(
            "Education",
            "/",
            false
        ));
        assert!(!should_create_missing_folder_for_filing(
            "Trash", "/", false
        ));
    }

    #[test]
    fn create_if_missing_still_allows_noncanonical_paths() {
        assert!(should_create_missing_folder_for_filing(
            "Projects/Foo",
            "/",
            true
        ));
    }

    #[test]
    fn filing_cooldown_has_safe_default() {
        std::env::remove_var("MAIL_ASSISTANT_MOVE_COOLDOWN_HOURS");
        assert_eq!(filing_move_cooldown_hours_from_env(), 720);
    }

    #[test]
    fn security_paths_are_rewritten_to_selectable_personal_security_leaves() {
        assert_eq!(
            canonicalize_location_folder_path("Security", "/"),
            "Personal/Security/Alerts"
        );
        assert_eq!(
            canonicalize_location_folder_path("Security/OTP", "/"),
            "Personal/Security/OTP"
        );
        assert_eq!(
            canonicalize_location_folder_path("Personal/Security", "/"),
            "Personal/Security/Alerts"
        );
    }

    #[test]
    fn container_paths_are_rewritten_to_selectable_general_leaves() {
        assert_eq!(
            canonicalize_location_folder_path("Personal", "/"),
            "Personal/General"
        );
        assert_eq!(
            canonicalize_location_folder_path("Personal/Property", "/"),
            "Personal/Property/General"
        );
        assert_eq!(
            canonicalize_location_folder_path("Work", "/"),
            "Work/General"
        );
    }

    #[test]
    fn filing_paths_are_sanitized_for_dovecot() {
        assert_eq!(
            canonicalize_location_folder_path("Travel/Booking.com", "/"),
            "Travel/Booking Com"
        );
        assert_eq!(
            canonicalize_location_folder_path("Personal/Shopping/Abel & Cole", "/"),
            "Personal/Shopping/Abel And Cole"
        );
        assert_eq!(
            canonicalize_location_folder_path("Work/Primer Labs Inc.", "/"),
            "Work/Primer Labs Inc"
        );
        assert_eq!(
            canonicalize_location_folder_path("Social/linkedin", "/"),
            "Social/LinkedIn"
        );
        assert_eq!(
            canonicalize_location_folder_path("Work/Dev/github", "/"),
            "Work/Dev/GitHub"
        );
    }

    #[test]
    fn selectable_check_allows_noselect_intermediate_parent() {
        let folders = vec![folder("Work", true), folder("Work/Dev", false)];
        assert!(is_path_selectable_in_folders("Work/Dev", &folders, "/"));
    }

    #[test]
    fn selectable_check_rejects_noselect_leaf() {
        let folders = vec![folder("Work/Dev", true)];
        assert!(!is_path_selectable_in_folders("Work/Dev", &folders, "/"));
    }
}
