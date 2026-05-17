use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use chrono::{Duration as ChronoDuration, Utc};

use crate::ai;
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db;
use crate::harness::{build_digest_tools, resolve_provider, AgentHarness, AgentSpec};
use crate::rate_limit::{build_frontier_ai_provider, build_local_ai_provider};

use super::shared::{create_rag_builder, load_agent_specs_dir, DEFAULT_ENV_PATH};

pub async fn run_digest(
    window: &str,
    account_id: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    let account_id = account_id.unwrap_or(DEFAULT_ACCOUNT_ID);
    let window = if window.eq_ignore_ascii_case("weekly") {
        "weekly"
    } else {
        "daily"
    };
    let since = if window == "weekly" {
        Utc::now() - ChronoDuration::days(7)
    } else {
        Utc::now() - ChronoDuration::days(1)
    };

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
    let spec_path = Path::new(&agents_dir).join("digest-agent.md");
    let spec = AgentSpec::parse_file(&spec_path)
        .with_context(|| format!("load digest-agent spec: {}", spec_path.display()))?;
    let provider =
        resolve_provider(&spec, local, frontier).map_err(|error| anyhow::anyhow!(error))?;

    let rag = create_rag_builder(db.clone(), Some(&ai_config)).await?;
    let tools = build_digest_tools(rag, db.clone(), account_id);

    let task_id = format!("digest-{}-{}", window, Utc::now().timestamp());
    let task_input = serde_json::json!({
        "account_id": account_id,
        "window": window,
        "since": since.to_rfc3339(),
    });

    let mut harness = AgentHarness::new(spec, account_id, db, provider, tools);
    let result = harness
        .run(&task_id, task_input)
        .await
        .context("run digest-agent")?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&result.output).context("serialize digest output")?
        );
        return Ok(());
    }

    if let Some(markdown) = result
        .output
        .get("digest_markdown")
        .and_then(|v| v.as_str())
    {
        println!("{}", markdown);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&result.output)
                .context("serialize digest output fallback")?
        );
    }

    Ok(())
}
