use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{Duration as ChronoDuration, Utc};
use std::str::FromStr;

use crate::agent_runtime;
use crate::ai;
use crate::ai_analysis;
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{
    self, AgentRunDetail, AgentRunStats, AgentRunStatus, AgentRunSummary, ScratchpadEntry,
};
use crate::harness::AgentSpec;
use crate::rate_limit::{
    build_local_ai_provider, effective_frontier_provider_name, wrap_configured_ai_provider,
};

use super::shared::{create_rag_builder, DEFAULT_ENV_PATH};

fn load_input_json(input: Option<String>) -> anyhow::Result<serde_json::Value> {
    let raw = if let Some(input) = input {
        input
    } else {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .context("read stdin for --input JSON")?;
        buffer
    };

    if raw.trim().is_empty() {
        bail!("no input JSON provided; pass --input '<json>' or pipe JSON on stdin");
    }

    serde_json::from_str(&raw).context("parse agent input JSON")
}

async fn open_database() -> anyhow::Result<Arc<db::Database>> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    Ok(Arc::new(database))
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let prefix: String = value.chars().take(max_chars - 3).collect();
    format!("{}...", prefix)
}

fn format_timestamp(dt: chrono::DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

fn format_optional_timestamp(dt: Option<chrono::DateTime<Utc>>) -> String {
    dt.map(format_timestamp).unwrap_or_else(|| "-".to_string())
}

fn format_duration_ms(duration_ms: Option<i32>) -> String {
    match duration_ms {
        Some(ms) if ms >= 1000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!("{}ms", ms),
        None => "-".to_string(),
    }
}

fn format_avg_duration_ms(duration_ms: f64) -> String {
    if duration_ms >= 1000.0 {
        format!("{:.1}s", duration_ms / 1000.0)
    } else {
        format!("{:.0}ms", duration_ms)
    }
}

fn format_token_total(input_tokens: Option<i32>, output_tokens: Option<i32>) -> String {
    let total = input_tokens.unwrap_or(0) + output_tokens.unwrap_or(0);
    if total == 0 {
        "-".to_string()
    } else {
        total.to_string()
    }
}

fn preview_json(value: &serde_json::Value, max_chars: usize) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| "<invalid json>".to_string());
    truncate_display(&serialized, max_chars)
}

fn prompt_confirmation(agent: &str, key: &str) -> anyhow::Result<bool> {
    print!(
        "Delete scratchpad key '{}' for agent '{}' ? [y/N] ",
        key, agent
    );
    std::io::stdout()
        .flush()
        .context("flush scratchpad delete prompt")?;

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read scratchpad delete confirmation")?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn print_run_table(runs: &[AgentRunSummary]) {
    println!(
        "{:<36}  {:<18}  {:<24}  {:<10}  {:>5}  {:>4}  {:>8}  {:>10}",
        "RUN_ID", "AGENT", "TASK_ID", "STATUS", "STEPS", "LLM", "TOKENS", "DURATION"
    );
    for run in runs {
        println!(
            "{:<36}  {:<18}  {:<24}  {:<10}  {:>5}  {:>4}  {:>8}  {:>10}",
            run.run_id,
            truncate_display(&run.agent_name, 18),
            truncate_display(&run.task_id, 24),
            run.status,
            run.steps,
            run.llm_calls,
            format_token_total(run.input_tokens, run.output_tokens),
            format_duration_ms(run.duration_ms),
        );
    }
}

fn print_run_detail(detail: &AgentRunDetail) -> anyhow::Result<()> {
    let summary = &detail.summary;
    println!("run_id: {}", summary.run_id);
    println!("account_id: {}", detail.account_id);
    println!("agent_name: {}", summary.agent_name);
    println!(
        "agent_version: {}",
        detail.agent_version.as_deref().unwrap_or("-")
    );
    println!("task_id: {}", summary.task_id);
    println!("status: {}", summary.status);
    println!("escalated: {}", summary.escalated);
    println!("steps: {}", summary.steps);
    println!("llm_calls: {}", summary.llm_calls);
    println!("tool_calls: {}", summary.tool_calls);
    println!(
        "tokens: in={} out={} total={}",
        summary
            .input_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
        summary
            .output_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
        format_token_total(summary.input_tokens, summary.output_tokens),
    );
    println!("duration: {}", format_duration_ms(summary.duration_ms));
    println!("started_at: {}", format_timestamp(summary.started_at));
    println!(
        "finished_at: {}",
        format_optional_timestamp(detail.finished_at)
    );
    println!("error: {}", summary.error.as_deref().unwrap_or("-"));

    println!("\nresult:");
    match &detail.result {
        Some(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(result).context("serialize agent run result")?
            );
        }
        None => println!("(none)"),
    }

    println!("\ntool_log:");
    if detail.tool_log.is_empty() {
        println!("(none)");
    } else {
        for entry in &detail.tool_log {
            println!(
                "- step={} tool={} latency={} called_at={}",
                entry.step,
                entry.tool_name,
                format_avg_duration_ms(entry.latency_ms as f64),
                format_timestamp(entry.called_at)
            );
            println!(
                "  arguments: {}",
                serde_json::to_string_pretty(&entry.arguments)
                    .context("serialize tool log arguments")?
            );
            println!("  result: {}", entry.result);
        }
    }

    Ok(())
}

fn print_scratchpad_table(entries: &[ScratchpadEntry]) {
    println!(
        "{:<18}  {:<24}  {:<56}  {:<20}  {:<20}",
        "AGENT", "KEY", "VALUE_PREVIEW", "UPDATED", "EXPIRES"
    );
    for entry in entries {
        println!(
            "{:<18}  {:<24}  {:<56}  {:<20}  {:<20}",
            truncate_display(&entry.agent_name, 18),
            truncate_display(&entry.key, 24),
            truncate_display(&preview_json(&entry.value, 56), 56),
            truncate_display(&format_timestamp(entry.updated_at), 20),
            truncate_display(&format_optional_timestamp(entry.expires_at), 20),
        );
    }
}

fn print_agent_stats(stats: &[AgentRunStats]) {
    for stat in stats {
        println!("Agent: {}", stat.agent_name);
        println!(
            "  Runs (24h):      {} completed, {} failed, {} timed_out",
            stat.completed, stat.failed, stat.timed_out
        );
        println!("  Avg steps:       {:.1}", stat.avg_steps);
        println!("  Avg tokens:      {:.0}", stat.avg_tokens);
        println!(
            "  Avg duration:    {}",
            format_avg_duration_ms(stat.avg_duration_ms)
        );
        println!("  Escalation rate: {:.0}%", stat.escalation_rate * 100.0);
        println!();
    }
}

async fn analyzer_from_config(
    db: Arc<db::Database>,
    ai_config: &ai::AIConfig,
) -> anyhow::Result<ai_analysis::EmailAnalyzer> {
    let frontier_name = effective_frontier_provider_name(ai_config);
    let frontier_box = ai::create_provider(ai_config).context("Create AI provider")?;
    let frontier = wrap_configured_ai_provider(ai_config, &frontier_name, Arc::from(frontier_box));

    let local = build_local_ai_provider(ai_config);

    let router = if ai_config.provider.eq_ignore_ascii_case("hybrid") && local.is_some() {
        Some(ai::HybridRouter::new(local, frontier.clone(), ai_config))
    } else {
        None
    };
    let rag_builder = create_rag_builder(db.clone(), Some(ai_config)).await?;
    let agents_dir = super::shared::load_agent_specs_dir();

    Ok(
        ai_analysis::EmailAnalyzer::new(router, frontier, rag_builder)
            .with_agent_specs(agents_dir, Some(db))
            .with_account_id(DEFAULT_ACCOUNT_ID)
            .with_analysis_mode(ai_config.analysis_mode.clone(), ai_config.max_iterations),
    )
}

fn extract_harness_run_id(analyzed_by: Option<&str>) -> Option<String> {
    analyzed_by
        .and_then(|value| value.strip_prefix("harness:"))
        .map(str::to_string)
}

fn escalation_reason_from_output(
    output: Option<&serde_json::Value>,
    escalation: &crate::harness::spec::EscalationConfig,
) -> Option<String> {
    let output = output?;
    if let Some(confidence) = output.get("confidence").and_then(|v| v.as_f64()) {
        if confidence < f64::from(escalation.confidence_threshold) {
            return Some(format!(
                "confidence {:.3} below threshold {:.3}",
                confidence, escalation.confidence_threshold
            ));
        }
    }
    if escalation.always_escalate_on_phishing
        && output
            .get("phishing_status")
            .and_then(|v| v.as_str())
            .is_some_and(|status| status == "phishing")
    {
        return Some("phishing result requires escalation".to_string());
    }
    if let Some(threat_level) = output.get("threat_level").and_then(|v| v.as_str()) {
        if escalation
            .always_escalate_on_threat
            .iter()
            .any(|candidate| candidate == threat_level)
        {
            return Some(format!(
                "threat_level '{}' requires escalation",
                threat_level
            ));
        }
    }
    None
}

pub async fn run_agent_benchmark(limit: usize) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db = open_database().await?;
    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let effective_limit = if limit == 0 { 20 } else { limit };
    let rows = db
        .get_emails_for_benchmark_for_account(DEFAULT_ACCOUNT_ID, effective_limit)
        .await?;
    if rows.is_empty() {
        println!("No benchmark rows found.");
        return Ok(());
    }

    let analyzer = analyzer_from_config(db.clone(), &ai_config).await?;
    let spec = AgentSpec::parse_file(Path::new("./specs/agents/email-analyzer.md"))
        .context("load agent spec for benchmark escalation policy")?;

    let mut spam_matches = 0usize;
    let mut phishing_matches = 0usize;
    let mut category_matches = 0usize;
    let mut email_type_matches = 0usize;
    let mut completed = 0usize;

    let mut llm_calls_total: i64 = 0;
    let mut tool_calls_total: i64 = 0;
    let mut input_tokens_total: i64 = 0;
    let mut output_tokens_total: i64 = 0;
    let mut duration_ms_total: i64 = 0;

    let mut escalations: Vec<(String, String)> = Vec::new();

    for email in rows {
        let started = std::time::Instant::now();
        let result = match analyzer.analyze(&email).await {
            Ok(result) => result,
            Err(error) => {
                log::warn!("[benchmark] skip {}: {}", email.message_id, error);
                continue;
            }
        };
        completed += 1;

        if result.spam_status.as_deref() == Some(email.spam_status.as_str()) {
            spam_matches += 1;
        }
        if result.phishing_status.as_deref() == Some(email.phishing_status.as_str()) {
            phishing_matches += 1;
        }
        if result.category == email.category {
            category_matches += 1;
        }
        if result.email_type == email.email_type {
            email_type_matches += 1;
        }

        let detail = if let Some(run_id) = extract_harness_run_id(result.analyzed_by.as_deref()) {
            db.get_agent_run(&run_id).await?
        } else {
            None
        };

        if let Some(detail) = detail.as_ref() {
            llm_calls_total += i64::from(detail.summary.llm_calls);
            tool_calls_total += i64::from(detail.summary.tool_calls);
            input_tokens_total += i64::from(detail.summary.input_tokens.unwrap_or(0));
            output_tokens_total += i64::from(detail.summary.output_tokens.unwrap_or(0));
            duration_ms_total += i64::from(detail.summary.duration_ms.unwrap_or(0));
        } else {
            input_tokens_total += result
                .token_usage
                .as_ref()
                .and_then(|u| u.input_tokens)
                .map(i64::from)
                .unwrap_or(0);
            output_tokens_total += result
                .token_usage
                .as_ref()
                .and_then(|u| u.output_tokens)
                .map(i64::from)
                .unwrap_or(0);
            duration_ms_total += started.elapsed().as_millis() as i64;
        }

        if let Some(local_run) = db
            .get_latest_agent_run_for_task(DEFAULT_ACCOUNT_ID, &email.message_id)
            .await?
        {
            if local_run.summary.escalated {
                let reason =
                    escalation_reason_from_output(local_run.result.as_ref(), &spec.escalation)
                        .unwrap_or_else(|| "escalation policy triggered".to_string());
                escalations.push((email.message_id.clone(), reason));
            }
        }
    }

    if completed == 0 {
        println!("Harness benchmark: 0 emails completed successfully.");
        return Ok(());
    }

    let pct = |n: usize| (n as f64 * 100.0) / completed as f64;
    let avg = |total: i64| total as f64 / completed as f64;

    println!("Harness benchmark: {} emails\n", completed);
    println!("Accuracy vs stored results:");
    println!(
        "  spam_status match:      {}/{}  ({:.0}%)",
        spam_matches,
        completed,
        pct(spam_matches)
    );
    println!(
        "  phishing_status match:  {}/{}  ({:.0}%)",
        phishing_matches,
        completed,
        pct(phishing_matches)
    );
    println!(
        "  category match:         {}/{}  ({:.0}%)",
        category_matches,
        completed,
        pct(category_matches)
    );
    println!(
        "  email_type match:       {}/{}  ({:.0}%)",
        email_type_matches,
        completed,
        pct(email_type_matches)
    );
    println!("\nPerformance:");
    println!("  Avg llm_calls:          {:.1}", avg(llm_calls_total));
    println!("  Avg tool_calls:         {:.1}", avg(tool_calls_total));
    println!("  Avg input_tokens:       {:.0}", avg(input_tokens_total));
    println!("  Avg output_tokens:      {:.0}", avg(output_tokens_total));
    println!(
        "  Avg duration:           {:.1}s",
        avg(duration_ms_total) / 1000.0
    );
    println!(
        "  Escalation rate:        {:.0}%   ({}/{})",
        (escalations.len() as f64 * 100.0) / completed as f64,
        escalations.len(),
        completed
    );

    println!("\nEscalations:");
    if escalations.is_empty() {
        println!("  (none)");
    } else {
        for (message_id, reason) in escalations {
            println!("  {}  reason: {}", message_id, reason);
        }
    }
    Ok(())
}

pub async fn run_agent(
    agent_spec: String,
    task_id: String,
    input: Option<String>,
) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);

    let spec = AgentSpec::parse_file(Path::new(&agent_spec))
        .with_context(|| format!("load agent spec: {}", agent_spec))?;
    let task_input = load_input_json(input)?;

    let db = open_database().await?;

    let result =
        agent_runtime::run_agent_spec(spec, db, DEFAULT_ACCOUNT_ID, &task_id, task_input).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&result.output).context("serialize harness output")?
    );

    eprintln!("run_id: {}", result.run_id);
    eprintln!("llm_calls: {}", result.llm_calls);
    eprintln!("tool_calls: {}", result.tool_calls);
    eprintln!(
        "tokens: in={:?} out={:?}",
        result.input_tokens, result.output_tokens
    );
    eprintln!("should_escalate: {}", result.should_escalate);
    if let Some(reason) = result.escalate_reason.as_deref() {
        eprintln!("escalate_reason: {}", reason);
    }

    Ok(())
}

pub async fn run_agent_runs(
    limit: usize,
    status: Option<String>,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let status = status
        .as_deref()
        .map(AgentRunStatus::from_str)
        .transpose()
        .context("invalid --status value")?;
    let db = open_database().await?;
    let runs = db
        .list_agent_runs_for_account(DEFAULT_ACCOUNT_ID, limit, status, agent.as_deref())
        .await?;

    if runs.is_empty() {
        println!("No agent runs found.");
        return Ok(());
    }

    print_run_table(&runs);
    Ok(())
}

pub async fn run_agent_show(run_id: String) -> anyhow::Result<()> {
    let db = open_database().await?;
    let detail = db
        .get_agent_run(&run_id)
        .await?
        .with_context(|| format!("agent run not found: {}", run_id))?;
    print_run_detail(&detail)
}

pub async fn run_agent_scratchpad(
    agent: Option<String>,
    key: Option<String>,
) -> anyhow::Result<()> {
    let db = open_database().await?;
    let entries = db
        .list_scratchpad_entries(DEFAULT_ACCOUNT_ID, agent.as_deref(), key.as_deref())
        .await?;

    if entries.is_empty() {
        println!("No scratchpad entries found.");
        return Ok(());
    }

    print_scratchpad_table(&entries);
    Ok(())
}

pub async fn run_agent_scratchpad_delete(agent: String, key: String) -> anyhow::Result<()> {
    if !prompt_confirmation(&agent, &key)? {
        bail!("scratchpad deletion cancelled");
    }

    let db = open_database().await?;
    let deleted = db
        .delete_scratchpad_entry(DEFAULT_ACCOUNT_ID, &agent, &key)
        .await?;
    if deleted {
        println!("Deleted scratchpad key '{}' for agent '{}'.", key, agent);
    } else {
        println!(
            "Scratchpad key '{}' for agent '{}' was not found.",
            key, agent
        );
    }
    Ok(())
}

pub async fn run_agent_stats() -> anyhow::Result<()> {
    let db = open_database().await?;
    let since = Utc::now() - ChronoDuration::hours(24);
    let stats = db
        .get_agent_run_stats_for_account(DEFAULT_ACCOUNT_ID, since)
        .await?;

    if stats.is_empty() {
        println!("No agent runs found in the last 24 hours.");
        return Ok(());
    }

    print_agent_stats(&stats);
    Ok(())
}

pub async fn run_agent_calibrate(agent: Option<String>) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db = open_database().await?;

    let specs: Vec<crate::harness::AgentSpec> = if let Some(ref name) = agent {
        let path = crate::agent_runtime::named_agent_spec_path(name)?;
        let spec = crate::harness::AgentSpec::parse_file(&path)
            .with_context(|| format!("load agent spec: {}", path.display()))?;
        vec![spec]
    } else {
        let mut specs = Vec::new();
        for spec_dir in crate::agent_runtime::agent_spec_search_dirs() {
            if !spec_dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&spec_dir)
                .with_context(|| format!("read spec directory {}", spec_dir.display()))?
                .flatten()
            {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.eq_ignore_ascii_case("readme") || n.starts_with('_'))
                {
                    continue;
                }
                if let Ok(spec) = crate::harness::AgentSpec::parse_file(&path) {
                    specs.push(spec);
                }
            }
        }
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    };

    if specs.is_empty() {
        println!("No agent specs found.");
        return Ok(());
    }

    println!(
        "{:<25} {:>7} {:>7} {:>7} {:>7} {:>10} {:>10} {:>10}",
        "AGENT", "SAMPLES", "P50", "P75", "P95", "ESC_RATE", "OLD_THR", "NEW_THR"
    );

    for spec in &specs {
        let result = crate::harness::calibration::calibrate_agent(
            &db,
            DEFAULT_ACCOUNT_ID,
            &spec.name,
            spec.escalation.confidence_threshold,
            None,
        )
        .await?;

        if result.skipped {
            println!(
                "{:<25} {:>7} {:>7} {:>7} {:>7} {:>10} {:>10} {:>10}",
                result.agent_name,
                result.sample_count,
                "-",
                "-",
                "-",
                "-",
                format!("{:.3}", spec.escalation.confidence_threshold),
                format!("(skip: {})", result.skip_reason.as_deref().unwrap_or("?")),
            );
        } else {
            println!(
                "{:<25} {:>7} {:>7.3} {:>7.3} {:>7.3} {:>9.1}% {:>10.3} {:>10.3}",
                result.agent_name,
                result.sample_count,
                result.p50,
                result.p75,
                result.p95,
                result.escalation_rate * 100.0,
                result
                    .previous_threshold
                    .unwrap_or(spec.escalation.confidence_threshold),
                result.new_threshold,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::AgentState;
    use serde_json::json;
    use uuid::Uuid;

    async fn load_test_database() -> Option<Arc<db::Database>> {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())?;
        let db = db::Database::new(&url).await.ok()?;
        let _ = sqlx::raw_sql(include_str!("../../schema.sql"))
            .execute(&db.pool)
            .await;
        Some(Arc::new(db))
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_agent_runs_empty() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping agent command test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let agent_name = format!("empty-agent-{}", Uuid::new_v4());
        let runs = db
            .list_agent_runs(10, None, Some(&agent_name))
            .await
            .expect("list agent runs");
        assert!(runs.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn test_scratchpad_roundtrip() {
        let Some(db) = load_test_database().await else {
            eprintln!("Skipping agent scratchpad test (no TEST_DATABASE_URL or DATABASE_URL)");
            return;
        };

        let agent_name = format!("scratchpad-agent-{}", Uuid::new_v4());
        let key = format!("key-{}", Uuid::new_v4());
        let state = AgentState::new(db.clone(), DEFAULT_ACCOUNT_ID, agent_name.clone());
        let expected = json!({"status":"ok","count":1});

        state
            .write_scratchpad(&key, expected.clone(), Some(1))
            .await
            .expect("write scratchpad");

        let entries = db
            .list_scratchpad_entries(DEFAULT_ACCOUNT_ID, Some(&agent_name), Some(&key))
            .await
            .expect("list scratchpad entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].agent_name, agent_name);
        assert_eq!(entries[0].key, key);
        assert_eq!(entries[0].value, expected);

        let deleted = db
            .delete_scratchpad_entry(DEFAULT_ACCOUNT_ID, &entries[0].agent_name, &entries[0].key)
            .await
            .expect("delete scratchpad entry");
        assert!(deleted);
    }
}
