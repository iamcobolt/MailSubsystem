use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;

use crate::ai;
use crate::config::{AccountConfig, DEFAULT_ACCOUNT_ID};
use crate::db;
use crate::harness::{build_location_tools, resolve_provider, AgentHarness, AgentSpec};
use crate::imap;
use crate::rate_limit::{build_frontier_ai_provider, build_local_ai_provider};

use super::shared::{load_agent_specs_dir, DEFAULT_ENV_PATH};

const SYSTEM_FOLDERS: [&str; 8] = [
    "INBOX", "Sent", "Trash", "Junk", "Drafts", "[Gmail]", "Spam", "Archive",
];

#[derive(Debug, Deserialize)]
struct ConsolidationOutput {
    #[serde(default)]
    consolidation_proposals: Vec<ConsolidationProposal>,
    #[serde(default)]
    empty_folders: Vec<String>,
    summary: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ConsolidationProposal {
    source_folder: String,
    target_folder: String,
    action: String,
    reason: String,
    email_count: i64,
    confidence: f64,
}

fn is_system_folder(folder: &str) -> bool {
    SYSTEM_FOLDERS
        .iter()
        .any(|system_folder| system_folder.eq_ignore_ascii_case(folder))
}

fn is_actionable_merge(proposal: &ConsolidationProposal) -> bool {
    proposal.action.eq_ignore_ascii_case("merge")
        && !proposal.source_folder.trim().is_empty()
        && !proposal.target_folder.trim().is_empty()
        && !proposal
            .source_folder
            .eq_ignore_ascii_case(&proposal.target_folder)
}

fn filter_actionable_proposals(
    account_id: &str,
    proposals: Vec<ConsolidationProposal>,
) -> Vec<ConsolidationProposal> {
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    let mut actionable = Vec::new();

    for proposal in proposals {
        if !is_actionable_merge(&proposal) {
            log::warn!(
                "[account={}] [consolidate] skipping invalid proposal source='{}' target='{}' action='{}'",
                account_id,
                proposal.source_folder,
                proposal.target_folder,
                proposal.action
            );
            continue;
        }
        if is_system_folder(&proposal.source_folder) || is_system_folder(&proposal.target_folder) {
            log::warn!(
                "[account={}] [consolidate] skipping system-folder proposal {} -> {}",
                account_id,
                proposal.source_folder,
                proposal.target_folder
            );
            continue;
        }

        let key = (
            proposal.source_folder.to_lowercase(),
            proposal.target_folder.to_lowercase(),
        );
        if !seen_pairs.insert(key) {
            log::warn!(
                "[account={}] [consolidate] skipping duplicate proposal {} -> {}",
                account_id,
                proposal.source_folder,
                proposal.target_folder
            );
            continue;
        }
        actionable.push(proposal);
    }

    actionable
}

async fn apply_proposals(
    account_id: &str,
    db: Arc<db::Database>,
    proposals: &[ConsolidationProposal],
) -> anyhow::Result<()> {
    if proposals.is_empty() {
        println!("Nothing to apply.");
        return Ok(());
    }

    let account = AccountConfig::load(account_id).context("Load account config")?;
    let client = imap::ImapClient::new(
        account.imap_server(),
        account.username.clone(),
        account.password.clone(),
    );

    println!("Applying {} proposal(s)...", proposals.len());

    let mut merged = 0usize;
    let mut moved_total = 0usize;

    for proposal in proposals {
        let source = proposal.source_folder.as_str();
        let target = proposal.target_folder.as_str();
        let message_ids = db
            .get_message_ids_by_location_for_account(account_id, source)
            .await
            .with_context(|| {
                format!(
                    "load source message ids for merge {} -> {}",
                    proposal.source_folder, proposal.target_folder
                )
            })?;

        let mut moved = 0usize;
        for message_id in &message_ids {
            let email = match db
                .get_email_by_message_id_for_account(account_id, message_id)
                .await
                .with_context(|| format!("load email record for {}", message_id))?
            {
                Some(email) => email,
                None => {
                    log::warn!(
                        "[account={}] [consolidate] skipping missing email record {}",
                        account_id,
                        message_id
                    );
                    continue;
                }
            };

            let uid = match email.uid {
                Some(uid) => uid as u32,
                None => {
                    log::warn!(
                        "[account={}] [consolidate] skipping {} due to missing UID",
                        account_id,
                        message_id
                    );
                    continue;
                }
            };

            match client.raw_uid_move(source, uid, target).await {
                Ok(Some(move_result)) => {
                    db.update_email_location_for_account(
                        account_id,
                        message_id,
                        target,
                        move_result.new_uid as i32,
                        move_result.new_uid_validity as i32,
                    )
                    .await
                    .with_context(|| {
                        format!("update moved email location for {} -> {}", source, target)
                    })?;
                    moved += 1;
                    moved_total += 1;
                }
                Ok(None) => {
                    log::warn!(
                        "[account={}] [consolidate] move returned no COPYUID for {} ({} -> {})",
                        account_id,
                        message_id,
                        source,
                        target
                    );
                }
                Err(error) => {
                    log::warn!(
                        "[account={}] [consolidate] failed moving {} ({} -> {}): {}",
                        account_id,
                        message_id,
                        source,
                        target,
                        error
                    );
                }
            }
        }

        match client.raw_delete_folder(source).await {
            Ok(()) => {
                if let Err(error) = db.delete_imap_folder_for_account(account_id, source).await {
                    log::warn!(
                        "[account={}] [consolidate] merged {} -> {} but failed db folder cleanup: {}",
                        account_id,
                        source,
                        target,
                        error
                    );
                }
                println!(
                    "Merged: {} -> {} (moved {} email(s))",
                    source, target, moved
                );
                merged += 1;
            }
            Err(error) => {
                log::warn!(
                    "[account={}] [consolidate] merged {} -> {} with {} moved but failed to delete source folder: {}",
                    account_id,
                    source,
                    target,
                    moved,
                    error
                );
                println!(
                    "Applied moves for {} -> {} ({} email(s)); source folder delete failed",
                    source, target, moved
                );
            }
        }
    }

    println!(
        "Consolidation complete: {} proposal(s) merged, {} email(s) moved.",
        merged, moved_total
    );
    Ok(())
}

pub async fn run_consolidate(dry_run: bool, account_id: Option<&str>) -> anyhow::Result<()> {
    let account_id = account_id.unwrap_or(DEFAULT_ACCOUNT_ID);
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);

    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let local = build_local_ai_provider(&ai_config);
    let frontier = build_frontier_ai_provider(&ai_config)?;

    let agents_dir = load_agent_specs_dir();
    let spec_path = Path::new(&agents_dir).join("folder-consolidator.md");
    let spec = AgentSpec::parse_file(&spec_path)
        .with_context(|| format!("load folder-consolidator spec: {}", spec_path.display()))?;
    let provider =
        resolve_provider(&spec, local, frontier).map_err(|error| anyhow::anyhow!(error))?;

    let tools = build_location_tools(db.clone(), None, account_id);
    let task_id = format!("consolidate-{}", Utc::now().timestamp());
    let task_input = serde_json::json!({
        "account_id": account_id,
        "dry_run": dry_run,
    });

    let mut harness = AgentHarness::new(spec, account_id, db.clone(), provider, tools);
    let result = harness
        .run(&task_id, task_input)
        .await
        .context("run folder-consolidator")?;
    let output: ConsolidationOutput =
        serde_json::from_value(result.output).context("parse folder-consolidator output")?;

    let actionable = filter_actionable_proposals(account_id, output.consolidation_proposals);

    if actionable.is_empty() {
        println!("No consolidation proposals.");
    } else {
        println!(
            "{:<30} {:<30} {:<8} {:<6} REASON",
            "SOURCE", "TARGET", "EMAILS", "CONF"
        );
        for proposal in &actionable {
            println!(
                "{:<30} {:<30} {:<8} {:<6.2} {}",
                proposal.source_folder,
                proposal.target_folder,
                proposal.email_count,
                proposal.confidence,
                proposal.reason
            );
        }
    }

    if !output.empty_folders.is_empty() {
        println!("\nEmpty folders: {}", output.empty_folders.join(", "));
    }
    println!("\n{}", output.summary);

    if dry_run {
        println!("\nDry run only; re-run with --apply to execute moves.");
        return Ok(());
    }

    apply_proposals(account_id, db, &actionable).await
}
