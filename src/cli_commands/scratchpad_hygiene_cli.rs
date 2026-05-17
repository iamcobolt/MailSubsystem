use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use serde_json::{json, Value};

use crate::ai;
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{self, ScratchpadEntry, ScratchpadStats};
use crate::harness::{build_orchestrator_tools, resolve_provider, AgentHarness, AgentSpec};
use crate::rate_limit::{build_frontier_ai_provider, build_local_ai_provider};

use super::shared::{create_rag_builder, load_agent_specs_dir, DEFAULT_ENV_PATH};

async fn open_database() -> anyhow::Result<Arc<db::Database>> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    Ok(Arc::new(database))
}

fn parse_entry_target(target: &str) -> Option<(&str, &str)> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (agent_name, key) = trimmed.split_once(':')?;
    let agent_name = agent_name.trim();
    let key = key.trim();
    if agent_name.is_empty() || key.is_empty() {
        return None;
    }
    Some((agent_name, key))
}

fn filter_entries_for_target<'a>(
    entries: &'a [ScratchpadEntry],
    target: &str,
) -> Vec<&'a ScratchpadEntry> {
    if let Some((agent_name, key)) = parse_entry_target(target) {
        entries
            .iter()
            .filter(|entry| entry.agent_name == agent_name && entry.key == key)
            .collect()
    } else {
        entries
            .iter()
            .filter(|entry| entry.key == target.trim())
            .collect()
    }
}

fn build_hygiene_input(
    account_id: &str,
    entries: &[ScratchpadEntry],
    stats: &[ScratchpadStats],
) -> Value {
    let entry_items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "agent_name": entry.agent_name,
                "key": entry.key,
                "value": entry.value,
                "updated_at": entry.updated_at.to_rfc3339(),
                "expires_at": entry.expires_at.map(|value| value.to_rfc3339()),
            })
        })
        .collect();
    let stat_items: Vec<Value> = stats
        .iter()
        .map(|stat| {
            json!({
                "agent_name": stat.agent_name,
                "key_count": stat.key_count,
                "total_size_bytes": stat.total_size_bytes,
                "oldest_entry": stat.oldest_entry.map(|value| value.to_rfc3339()),
                "newest_entry": stat.newest_entry.map(|value| value.to_rfc3339()),
            })
        })
        .collect();
    json!({
        "task_type": "scratchpad_hygiene",
        "account_id": account_id,
        "scratchpad_entries": entry_items,
        "scratchpad_stats": stat_items,
    })
}

fn extract_hygiene_result(output: &Value) -> Option<Value> {
    let task_type = output.get("task_type").and_then(|value| value.as_str())?;
    if task_type != "scratchpad_hygiene" {
        return None;
    }
    let result = output.get("result")?;
    if !result.is_object() {
        return None;
    }
    Some(result.clone())
}

fn parse_delete_keys(result: &Value) -> Vec<String> {
    result
        .get("delete_keys")
        .and_then(|value| value.as_array())
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(|item| item.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn parse_summaries(result: &Value) -> Vec<(String, String)> {
    result
        .get("summarize")
        .and_then(|value| value.as_object())
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(|(target, summary)| {
            let target = target.trim();
            let summary = summary.as_str()?.trim();
            if target.is_empty() || summary.is_empty() {
                return None;
            }
            Some((target.to_string(), summary.to_string()))
        })
        .collect()
}

fn parse_poison_candidates(result: &Value) -> Vec<Value> {
    result
        .get("poison_candidates")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
}

pub async fn run_scratchpad_hygiene(account_id: Option<&str>) -> anyhow::Result<()> {
    let account_id = account_id.unwrap_or(DEFAULT_ACCOUNT_ID);
    let db = open_database().await?;
    let ai_config = ai::AIConfig::load().context("Load AI config")?;

    let local = build_local_ai_provider(&ai_config);
    let frontier = match build_frontier_ai_provider(&ai_config) {
        Ok(provider) => provider,
        Err(error) => {
            println!(
                "Orchestrator provider unavailable, skipping scratchpad hygiene for account {} ({})",
                account_id, error
            );
            return Ok(());
        }
    };

    let agents_dir = load_agent_specs_dir();
    let spec_path = Path::new(&agents_dir).join("orchestrator.md");
    let spec = match AgentSpec::parse_file(&spec_path) {
        Ok(spec) => spec,
        Err(error) => {
            println!(
                "Orchestrator spec unavailable at {}, skipping scratchpad hygiene ({})",
                spec_path.display(),
                error
            );
            return Ok(());
        }
    };
    let provider = match resolve_provider(&spec, local, frontier) {
        Ok(provider) => provider,
        Err(error) => {
            println!(
                "No orchestrator provider available, skipping scratchpad hygiene ({})",
                error
            );
            return Ok(());
        }
    };

    let entries = db
        .list_scratchpad_entries(account_id, None, None)
        .await
        .context("list scratchpad entries")?;
    let stats = db
        .get_scratchpad_stats_for_account(account_id)
        .await
        .context("get scratchpad stats")?;
    if entries.is_empty() {
        println!("No scratchpad entries found for account {}.", account_id);
        return Ok(());
    }

    let rag = create_rag_builder(db.clone(), Some(&ai_config)).await?;
    let tools = build_orchestrator_tools(db.clone(), rag, account_id.to_string());
    let mut harness = AgentHarness::new(spec, account_id, db.clone(), provider, tools);
    let task_id = format!("scratchpad-hygiene-{}", chrono::Utc::now().timestamp());
    let input = build_hygiene_input(account_id, &entries, &stats);
    let run_result = harness
        .run(&task_id, input)
        .await
        .context("run scratchpad hygiene orchestrator task")?;

    let Some(result) = extract_hygiene_result(&run_result.output) else {
        println!("Scratchpad hygiene produced malformed output; no changes applied.");
        return Ok(());
    };

    let delete_keys = parse_delete_keys(&result);
    let summarize = parse_summaries(&result);
    let poison_candidates = parse_poison_candidates(&result);

    let mut deleted = 0u64;
    for target in &delete_keys {
        for entry in filter_entries_for_target(&entries, target) {
            let removed = db
                .delete_scratchpad_entry(account_id, &entry.agent_name, &entry.key)
                .await
                .with_context(|| {
                    format!("delete scratchpad entry {}:{}", entry.agent_name, entry.key)
                })?;
            if removed {
                deleted += 1;
            }
        }
    }

    let entries_after_delete = db
        .list_scratchpad_entries(account_id, None, None)
        .await
        .context("list scratchpad entries after delete")?;

    let mut summarized = 0u64;
    for (target, summary) in summarize {
        for entry in filter_entries_for_target(&entries_after_delete, &target) {
            let updated = db
                .update_scratchpad_entry_for_account(
                    account_id,
                    &entry.agent_name,
                    &entry.key,
                    &Value::String(summary.clone()),
                )
                .await
                .with_context(|| {
                    format!(
                        "update scratchpad summary {}:{}",
                        entry.agent_name, entry.key
                    )
                })?;
            summarized += updated;
        }
    }

    for candidate in &poison_candidates {
        let rendered =
            serde_json::to_string(candidate).unwrap_or_else(|_| "<invalid candidate>".to_string());
        log::warn!(
            "[account={}] scratchpad hygiene poison candidate: {}",
            account_id,
            rendered
        );
    }

    println!(
        "Scratchpad hygiene completed for account {}: deleted {} key(s), summarized {} entry(ies), poison candidates {}.",
        account_id,
        deleted,
        summarized,
        poison_candidates.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_entry_target_reads_agent_key_pairs() {
        assert_eq!(
            parse_entry_target("email-analyzer:sender_patterns"),
            Some(("email-analyzer", "sender_patterns"))
        );
        assert_eq!(parse_entry_target(" sender_patterns "), None);
    }

    #[test]
    fn parse_delete_keys_ignores_invalid_values() {
        let result = json!({
            "delete_keys": ["key-a", "", 10, "key-b"]
        });
        assert_eq!(
            parse_delete_keys(&result),
            vec!["key-a".to_string(), "key-b".to_string()]
        );
    }

    #[test]
    fn parse_summaries_ignores_empty_values() {
        let result = json!({
            "summarize": {
                "key-a": "summary",
                "key-b": "",
                "": "skip"
            }
        });
        assert_eq!(
            parse_summaries(&result),
            vec![("key-a".to_string(), "summary".to_string())]
        );
    }
}
