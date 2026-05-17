use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::ai;
use crate::commands::shared::{create_rag_builder, load_agent_specs_dir};
use crate::db;
use crate::harness::{
    build_analysis_tools, build_digest_tools, build_location_tools, build_mail_assistant_tools,
    build_orchestrator_tools, resolve_provider, AgentHarness, AgentSpec, HarnessEventCallback,
    RunResult, ToolRegistry,
};
use crate::rate_limit::{build_frontier_ai_provider, build_local_ai_provider};

fn validate_named_agent(agent_name: &str) -> Result<String> {
    let trimmed = agent_name.trim();
    if trimmed.is_empty() {
        bail!("agent name must not be empty");
    }
    let normalized = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    if normalized
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Ok(normalized.to_string());
    }
    bail!("agent name may only contain letters, numbers, '-' or '_'");
}

pub fn named_agent_spec_path(agent_name: &str) -> Result<PathBuf> {
    let normalized = validate_named_agent(agent_name)?;
    let searched = agent_spec_search_dirs();
    for dir in &searched {
        let candidate = dir.join(format!("{}.md", normalized));
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let searched = searched
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "agent or worker spec '{}' not found in any spec directory: {}",
        normalized,
        searched
    );
}

pub fn agent_spec_search_dirs() -> Vec<PathBuf> {
    let agents_dir = load_agent_specs_dir();
    let agents_dir = PathBuf::from(agents_dir);
    let workers_dir = std::env::var("WORKERS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            agents_dir
                .parent()
                .map(|parent| parent.join("workers"))
                .unwrap_or_else(|| PathBuf::from("./specs/workers"))
        });

    vec![agents_dir, workers_dir]
}

pub fn load_named_agent_spec(agent_name: &str) -> Result<AgentSpec> {
    let spec_path = named_agent_spec_path(agent_name)?;
    AgentSpec::parse_file(&spec_path)
        .with_context(|| format!("load agent spec: {}", spec_path.display()))
}

pub async fn build_tools_for_agent(
    spec: &AgentSpec,
    db: Arc<db::Database>,
    account_id: &str,
    ai_config: &ai::AIConfig,
) -> Result<ToolRegistry> {
    let agent_name = spec.name.to_lowercase();

    if agent_name == "mail-assistant" {
        let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
        Ok(build_mail_assistant_tools(db, rag, account_id.to_string()))
    } else if agent_name.contains("email")
        || agent_name.contains("analyzer")
        || agent_name.contains("classification")
    {
        let rag = create_rag_builder(db, Some(ai_config)).await?;
        Ok(build_analysis_tools(rag, account_id.to_string()))
    } else if agent_name.contains("digest") {
        let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
        Ok(build_digest_tools(rag, db, account_id.to_string()))
    } else if agent_name.contains("location") || agent_name.contains("folder-recommendation") {
        let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
        Ok(build_location_tools(db, Some(rag), account_id.to_string()))
    } else if agent_name.contains("orchestrator") || agent_name.contains("conflict-review") {
        let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
        Ok(build_orchestrator_tools(db, rag, account_id.to_string()))
    } else if agent_name.contains("consolidator") || agent_name.contains("folder-learning") {
        Ok(build_location_tools(db, None, account_id.to_string()))
    } else {
        Ok(ToolRegistry::new())
    }
}

pub async fn build_tools_for_skill_bundle(
    skill_bundle: &str,
    db: Arc<db::Database>,
    account_id: &str,
    ai_config: &ai::AIConfig,
) -> Result<ToolRegistry> {
    match skill_bundle {
        "email_classification" => {
            let rag = create_rag_builder(db, Some(ai_config)).await?;
            Ok(build_analysis_tools(rag, account_id.to_string()))
        }
        "folder_recommendation" => {
            let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
            Ok(build_location_tools(db, Some(rag), account_id.to_string()))
        }
        "digest_generation" => {
            let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
            Ok(build_digest_tools(rag, db, account_id.to_string()))
        }
        "folder_learning" => {
            let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
            Ok(build_location_tools(db, Some(rag), account_id.to_string()))
        }
        "conflict_review" | "safety_policy" => {
            let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
            Ok(build_orchestrator_tools(db, rag, account_id.to_string()))
        }
        "mailbox_context" => {
            let rag = create_rag_builder(db.clone(), Some(ai_config)).await?;
            Ok(build_mail_assistant_tools(db, rag, account_id.to_string()))
        }
        other => anyhow::bail!("unknown sub-agent skill bundle '{}'", other),
    }
}

pub async fn run_agent_spec(
    spec: AgentSpec,
    db: Arc<db::Database>,
    account_id: &str,
    task_id: &str,
    input: Value,
) -> Result<RunResult> {
    run_agent_spec_with_callback(spec, db, account_id, task_id, input, None).await
}

pub async fn run_agent_spec_with_callback(
    spec: AgentSpec,
    db: Arc<db::Database>,
    account_id: &str,
    task_id: &str,
    input: Value,
    callback: Option<HarnessEventCallback>,
) -> Result<RunResult> {
    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let local = build_local_ai_provider(&ai_config);
    let frontier = build_frontier_ai_provider(&ai_config)?;
    let provider =
        resolve_provider(&spec, local, frontier).map_err(|error| anyhow::anyhow!(error))?;
    let tools = build_tools_for_agent(&spec, db.clone(), account_id, &ai_config).await?;

    let mut harness = AgentHarness::new(spec, account_id, db, provider, tools);
    harness.run_with_callback(task_id, input, callback).await
}

pub async fn run_agent_spec_with_tools(
    spec: AgentSpec,
    db: Arc<db::Database>,
    account_id: &str,
    task_id: &str,
    input: Value,
    tools: ToolRegistry,
    callback: Option<HarnessEventCallback>,
) -> Result<RunResult> {
    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let local = build_local_ai_provider(&ai_config);
    let frontier = build_frontier_ai_provider(&ai_config)?;
    let provider =
        resolve_provider(&spec, local, frontier).map_err(|error| anyhow::anyhow!(error))?;

    let mut harness = AgentHarness::new(spec, account_id, db, provider, tools);
    harness.run_with_callback(task_id, input, callback).await
}

pub async fn run_named_agent(
    db: Arc<db::Database>,
    account_id: &str,
    agent_name: &str,
    task_id: &str,
    input: Value,
) -> Result<RunResult> {
    run_named_agent_with_callback(db, account_id, agent_name, task_id, input, None).await
}

pub async fn run_named_agent_with_callback(
    db: Arc<db::Database>,
    account_id: &str,
    agent_name: &str,
    task_id: &str,
    input: Value,
    callback: Option<HarnessEventCallback>,
) -> Result<RunResult> {
    let spec = load_named_agent_spec(agent_name)?;
    run_agent_spec_with_callback(spec, db, account_id, task_id, input, callback).await
}
