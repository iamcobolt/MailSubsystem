use anyhow::Context;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
use serde_json::Value;

use crate::rate_limit::{
    build_local_ai_provider, effective_frontier_provider_name, wrap_configured_ai_provider,
    wrap_embedding_provider_with_pressure,
};
use crate::{ai, ai_analysis, config::DEFAULT_ACCOUNT_ID, db, embeddings, metrics};

use super::shared::{create_rag_builder, load_agent_specs_dir, DEFAULT_ENV_PATH};

fn format_error_chain(err: &anyhow::Error) -> String {
    let mut out = err.to_string();
    for (idx, cause) in err.chain().skip(1).enumerate() {
        out.push_str(&format!("\n    {}: {}", idx + 1, cause));
    }
    out
}

#[derive(Debug, Clone)]
struct FrontierQueueWorkerConfig {
    max_attempts: i32,
    retry_base_secs: i64,
    max_retry_delay_secs: i64,
    stale_processing_secs: i64,
}

impl FrontierQueueWorkerConfig {
    fn from_env() -> Self {
        let max_attempts = std::env::var("FRONTIER_QUEUE_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_i32)
            .max(1);
        let retry_base_secs = std::env::var("FRONTIER_QUEUE_RETRY_BASE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_i64)
            .max(1);
        let max_retry_delay_secs = std::env::var("FRONTIER_QUEUE_MAX_RETRY_DELAY_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600_i64)
            .max(1);
        let stale_processing_secs = std::env::var("FRONTIER_QUEUE_STALE_PROCESSING_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900_i64)
            .max(30);

        Self {
            max_attempts,
            retry_base_secs,
            max_retry_delay_secs,
            stale_processing_secs,
        }
    }
}

fn frontier_retry_delay_secs(base_secs: i64, attempt_count: i32, max_delay_secs: i64) -> i64 {
    let exponent = (attempt_count - 1).clamp(0, 16) as u32;
    let factor = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
    base_secs
        .saturating_mul(factor)
        .clamp(1, max_delay_secs.max(1))
}

fn is_transient_frontier_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("503")
        || lower.contains("service unavailable")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("high demand")
}

async fn schedule_frontier_retry(
    db: &db::Database,
    account_id: &str,
    entry: &db::FrontierQueueEntry,
    config: &FrontierQueueWorkerConfig,
    error: &str,
) -> anyhow::Result<String> {
    let delay = frontier_retry_delay_secs(
        config.retry_base_secs,
        entry.attempt_count,
        config.max_retry_delay_secs,
    );
    db.mark_frontier_retry_or_dead_for_account(
        account_id,
        &entry.message_id,
        entry.attempt_count,
        config.max_attempts,
        delay,
        error,
    )
    .await
    .context("mark frontier retry/dead")
}

pub async fn run_test_llm(local: bool, frontier: bool) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let ai_config = ai::AIConfig::load().context("Load AI config")?;

    if local == frontier {
        anyhow::bail!(
            "Specify exactly one: --local or --frontier. \
             test-llm --local = local LLM (omlx/LM Studio/Ollama), test-llm --frontier = frontier (Gemini/OpenAI/Anthropic)."
        );
    }

    let (label, config) = if local {
        ("local", {
            let mut c = ai_config.clone();
            c.provider = "lmstudio".to_string();
            c
        })
    } else {
        ("frontier", {
            let mut c = ai_config.clone();
            c.provider = c
                .frontier_provider
                .clone()
                .unwrap_or_else(|| "gemini".to_string());
            c
        })
    };

    let provider_name = if local {
        "lmstudio".to_string()
    } else {
        effective_frontier_provider_name(&config)
    };
    let provider = ai::create_provider(&config).context("Create AI provider")?;
    let provider: Arc<dyn ai::AIProvider> = Arc::from(provider);
    let provider = wrap_configured_ai_provider(&config, &provider_name, provider);
    println!(
        "test-llm ({}): provider created, sending one completion...",
        label
    );

    let messages = vec![ai::Message::user("Reply with exactly: OK")];
    let response = provider.complete(messages).await.context("LLM complete")?;

    let content = response.content.trim();
    println!(
        "response: {}",
        if content.is_empty() {
            "(empty)"
        } else {
            content
        }
    );
    if let Some(c) = response.confidence {
        println!("confidence: {}", c);
    }
    println!("finish_reason: {}", response.finish_reason);
    Ok(())
}

pub async fn run_show(message_id: Option<String>) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let message_id =
        message_id.ok_or_else(|| anyhow::anyhow!("Usage: mailsubsystem show <message_id>"))?;

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;

    let Some(fields) = database
        .get_email_ai_fields(&message_id)
        .await
        .context("Fetch AI fields")?
    else {
        anyhow::bail!("No email found with message_id: {}", message_id);
    };

    println!("message_id: {}", message_id);
    println!("--- AI fields in DB ---");
    println!("  analyzed_by:     {:?}", fields.analyzed_by);
    println!("  spam_status:     {:?}", fields.spam_status);
    println!("  phishing_status: {:?}", fields.phishing_status);
    println!("  marketing_status:{:?}", fields.marketing_status);
    println!("  otp_status:      {:?}", fields.otp_status);
    println!("  otp_code:        {:?}", fields.otp_code);
    println!("  threat_level:    {:?}", fields.threat_level);
    println!("  threat_indicators:{:?}", fields.threat_indicators);
    println!("  category:        {:?}", fields.category);
    println!("  subcategory:     {:?}", fields.subcategory);
    println!("  organization:    {:?}", fields.organization);
    println!("  topic:           {:?}", fields.topic);
    println!("  email_type:      {:?}", fields.email_type);
    println!(
        "  location_recommendation: {:?}",
        fields.location_recommendation
    );
    if let Some(s) = fields.human_summary.as_deref() {
        let preview = if s.len() > 80 {
            format!("{}...", &s[..77])
        } else {
            s.to_string()
        };
        println!("  human_summary:   {}", preview);
    } else {
        println!("  human_summary:   (none)");
    }
    if let Some(ref v) = fields.ai_summary {
        println!(
            "  ai_summary:      (JSON, {} chars)",
            serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
        );
    } else {
        println!("  ai_summary:      (none)");
    }
    Ok(())
}

pub async fn run_classification_eval(corpus: Option<String>) -> anyhow::Result<()> {
    ai_analysis::run_classification_eval(corpus.as_deref().map(Path::new))
}

pub async fn run_process_frontier_queue(limit: usize) -> anyhow::Result<()> {
    run_process_frontier_queue_for_account(limit, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_process_frontier_queue_for_account(
    limit: usize,
    account_id: &str,
) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let worker_config = FrontierQueueWorkerConfig::from_env();

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);
    if let Err(err) = db.ensure_hybrid_retrieval_indexes().await {
        log::warn!("Failed to ensure hybrid retrieval indexes: {}", err);
    }

    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let frontier_name = effective_frontier_provider_name(&ai_config);
    let frontier_box = ai::create_provider(&ai_config).context("Create frontier provider")?;
    let frontier: Arc<dyn ai::AIProvider> = Arc::from(frontier_box);
    let frontier = wrap_configured_ai_provider(&ai_config, &frontier_name, frontier);
    let rag_builder = create_rag_builder(db.clone(), Some(&ai_config)).await?;
    let agents_dir = load_agent_specs_dir();
    let analyzer = ai_analysis::EmailAnalyzer::new(None, frontier, rag_builder)
        .with_agent_specs(agents_dir, Some(db.clone()))
        .with_account_id(account_id)
        .with_analysis_mode(ai_config.analysis_mode.clone(), ai_config.max_iterations);

    if let Ok(reset) = db
        .reset_stale_frontier_processing_for_account(
            account_id,
            worker_config.stale_processing_secs,
        )
        .await
    {
        if reset > 0 {
            metrics::counter("analysis_frontier_queue_stale_reset_total", reset, &[]);
            log::warn!("[frontier] reset {} stale processing rows", reset);
        }
    }

    let worker_id = format!("frontier-worker-{}-{}", std::process::id(), account_id);
    let claimed = db
        .claim_frontier_queue_batch_for_account(account_id, &worker_id, limit)
        .await
        .context("Claim frontier queue")?;
    metrics::gauge("frontier_queue_batch_size", claimed.len() as f64, &[]);
    if claimed.is_empty() {
        println!("Frontier queue is empty.");
        return Ok(());
    }
    println!("Processing {} item(s) from frontier queue", claimed.len());

    for entry in claimed {
        let message_id = entry.message_id.as_str();
        let started = std::time::Instant::now();
        let email = match db
            .get_email_by_message_id_for_account(account_id, message_id)
            .await?
        {
            Some(e) => e,
            None => {
                log::warn!("[frontier] skip {}: email not found", message_id);
                let _ = db
                    .mark_frontier_done_for_account(account_id, message_id)
                    .await;
                continue;
            }
        };

        match analyzer.analyze_frontier_only(&email).await {
            Ok(mut result) => {
                if let Err(e) = ai_analysis::apply_analysis_result_for_account(
                    db.as_ref(),
                    account_id,
                    message_id,
                    &mut result,
                )
                .await
                {
                    log::error!("[frontier] failed to save {}: {}", message_id, e);
                    let error_text = format_error_chain(&e);
                    match schedule_frontier_retry(
                        db.as_ref(),
                        account_id,
                        &entry,
                        &worker_config,
                        &error_text,
                    )
                    .await
                    {
                        Ok(status) if status == "dead" => {
                            metrics::counter("analysis_frontier_queue_dead_total", 1, &[]);
                        }
                        Ok(_) => {
                            metrics::counter("analysis_frontier_queue_retry_total", 1, &[]);
                        }
                        Err(retry_err) => {
                            log::error!(
                                "[frontier] failed to schedule retry for {}: {}",
                                message_id,
                                retry_err
                            );
                        }
                    }
                    continue;
                }
                if let Err(e) = db
                    .mark_frontier_done_for_account(account_id, message_id)
                    .await
                {
                    log::warn!(
                        "[frontier] failed to complete {} in queue: {}",
                        message_id,
                        e
                    );
                    let error_text = format_error_chain(&e);
                    match schedule_frontier_retry(
                        db.as_ref(),
                        account_id,
                        &entry,
                        &worker_config,
                        &error_text,
                    )
                    .await
                    {
                        Ok(status) if status == "dead" => {
                            metrics::counter("analysis_frontier_queue_dead_total", 1, &[]);
                        }
                        Ok(_) => {
                            metrics::counter("analysis_frontier_queue_retry_total", 1, &[]);
                        }
                        Err(retry_err) => {
                            log::error!(
                                "[frontier] failed to schedule retry for {}: {}",
                                message_id,
                                retry_err
                            );
                        }
                    }
                } else {
                    println!("Frontier analyzed: {}", message_id);
                    metrics::counter("analysis_frontier_queue_done_total", 1, &[]);
                    metrics::histogram(
                        "analysis_frontier_queue_latency_seconds",
                        started.elapsed().as_secs_f64(),
                        &[],
                    );
                }
            }
            Err(e) => {
                metrics::counter("analysis_frontier_queue_failed_total", 1, &[]);
                metrics::histogram(
                    "analysis_frontier_queue_latency_seconds",
                    started.elapsed().as_secs_f64(),
                    &[],
                );
                let error_text = format_error_chain(&e);
                let transient = is_transient_frontier_error(&error_text);
                match schedule_frontier_retry(
                    db.as_ref(),
                    account_id,
                    &entry,
                    &worker_config,
                    &error_text,
                )
                .await
                {
                    Ok(status) if status == "dead" => {
                        metrics::counter("analysis_frontier_queue_dead_total", 1, &[]);
                    }
                    Ok(_) => {
                        metrics::counter("analysis_frontier_queue_retry_total", 1, &[]);
                    }
                    Err(retry_err) => {
                        log::error!(
                            "[frontier] failed to schedule retry for {}: {}",
                            message_id,
                            retry_err
                        );
                    }
                }
                if transient {
                    log::error!(
                        "[frontier] stopping frontier processing due to transient provider error: {}",
                        error_text
                    );
                    break;
                }
                log::warn!("[frontier] failed {}: {}", message_id, error_text);
            }
        }
    }

    if let Ok(depth) = db.frontier_queue_depth_for_account(account_id).await {
        metrics::gauge("frontier_queue_pending", depth.pending as f64, &[]);
        metrics::gauge("frontier_queue_failed", depth.failed as f64, &[]);
        metrics::gauge("frontier_queue_processing", depth.processing as f64, &[]);
        metrics::gauge("frontier_queue_dead", depth.dead as f64, &[]);
    }

    Ok(())
}

struct AnalyzeRuntimeContext {
    db: Arc<db::Database>,
    analyzer: ai_analysis::EmailAnalyzer,
    max_analysis_attempts: i32,
    analysis_concurrency: usize,
    worker_id: String,
    analysis_lock_ttl_secs: i64,
}

async fn build_analyze_runtime_context(
    account_id: &str,
    concurrency_override: Option<usize>,
) -> anyhow::Result<AnalyzeRuntimeContext> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let max_analysis_attempts: i32 = std::env::var("MAX_ANALYSIS_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let worker_id = analysis_worker_id_from_env();
    let analysis_lock_ttl_secs = analysis_lock_ttl_secs_from_env();

    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);
    if let Err(err) = db.ensure_hybrid_retrieval_indexes().await {
        log::warn!("Failed to ensure hybrid retrieval indexes: {}", err);
    }

    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let frontier_name = effective_frontier_provider_name(&ai_config);
    let frontier_box = ai::create_provider(&ai_config).context("Create AI provider")?;
    let frontier: Arc<dyn ai::AIProvider> = Arc::from(frontier_box);
    let frontier = wrap_configured_ai_provider(&ai_config, &frontier_name, frontier);
    let analysis_concurrency = concurrency_override
        .map(|value| value.max(1))
        .unwrap_or_else(|| analysis_concurrency_from_env(&ai_config, frontier.is_local()));

    let local = build_local_ai_provider(&ai_config);

    let router = if ai_config.provider.eq_ignore_ascii_case("hybrid") && local.is_some() {
        Some(ai::HybridRouter::new(local, frontier.clone(), &ai_config))
    } else {
        None
    };

    let rag_builder = create_rag_builder(db.clone(), Some(&ai_config)).await?;
    let agents_dir = load_agent_specs_dir();

    let analyzer = ai_analysis::EmailAnalyzer::new(router, frontier, rag_builder)
        .with_agent_specs(agents_dir, Some(db.clone()))
        .with_account_id(account_id)
        .with_analysis_mode(ai_config.analysis_mode.clone(), ai_config.max_iterations);

    Ok(AnalyzeRuntimeContext {
        db,
        analyzer,
        max_analysis_attempts,
        analysis_concurrency,
        worker_id,
        analysis_lock_ttl_secs,
    })
}

fn analysis_worker_id_from_env() -> String {
    std::env::var("ANALYSIS_WORKER_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("analysis-{}-{}", std::process::id(), uuid::Uuid::new_v4()))
}

fn analysis_lock_ttl_secs_from_env() -> i64 {
    std::env::var("ANALYSIS_LOCK_TTL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(db::DEFAULT_ANALYSIS_LOCK_TTL_SECS)
        .max(1)
}

fn analysis_concurrency_from_env(config: &ai::AIConfig, provider_is_local: bool) -> usize {
    if let Some(value) = std::env::var("ANALYZE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return value.max(1);
    }
    if let Some(value) = std::env::var("LOCAL_LLM_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
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

struct AnalyzeRecordOutcome {
    message_id: String,
    error: Option<anyhow::Error>,
    transient: bool,
    durable_progress: bool,
}

#[derive(Clone, Copy)]
struct AnalyzeRecordOptions<'a> {
    max_analysis_attempts: i32,
    claim_worker_id: Option<&'a str>,
    worker_instruction: Option<&'a str>,
    single: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AnalyzeBatchSummary {
    candidates: usize,
    succeeded: usize,
    failed: usize,
    durable_progress: usize,
}

impl AnalyzeBatchSummary {
    fn made_durable_progress(self) -> bool {
        self.durable_progress > 0
    }
}

#[derive(Clone, Copy)]
struct AnalyzeBatchOptions<'a> {
    max_analysis_attempts: i32,
    claim_worker_id: Option<&'a str>,
    worker_instructions: Option<&'a HashMap<String, String>>,
    analysis_concurrency: usize,
}

fn is_transient_analysis_error(message: &str) -> bool {
    message.contains("503")
        || message.contains("Service Unavailable")
        || message.contains("429")
        || message.contains("Too Many Requests")
        || message.contains("RESOURCE_EXHAUSTED")
        || message.contains("quota")
        || message.contains("high demand")
}

async fn record_analysis_failure_for_account(
    db: &db::Database,
    account_id: &str,
    email: &db::EmailRecord,
    claim_worker_id: Option<&str>,
    message: &str,
    max_analysis_attempts: i32,
) -> bool {
    let attempts = email.analysis_attempts + 1;
    let mark_permanent = attempts >= max_analysis_attempts;

    let recorded = if let Some(worker_id) = claim_worker_id {
        match db
            .record_analysis_attempt_failed_for_claimed_account(
                account_id,
                &email.message_id,
                worker_id,
                message,
                mark_permanent,
            )
            .await
        {
            Ok(rows) => rows > 0,
            Err(db_err) => {
                log::warn!(
                    "[analyze] failed to record attempt for {}: {}",
                    email.message_id,
                    db_err
                );
                return false;
            }
        }
    } else if let Err(db_err) = db
        .record_analysis_attempt_failed_for_account(account_id, &email.message_id, message)
        .await
    {
        log::warn!(
            "[analyze] failed to record attempt for {}: {}",
            email.message_id,
            db_err
        );
        return false;
    } else {
        true
    };

    if !recorded {
        log::warn!(
            "[analyze] did not record attempt for {} because claim is no longer owned",
            email.message_id
        );
        return false;
    }

    if mark_permanent {
        log::error!(
            "[analyze] permanent failure for {} after {} attempts",
            email.message_id,
            attempts
        );
        if claim_worker_id.is_none() {
            if let Err(db_err) = db
                .mark_analysis_permanent_failure_for_account(account_id, &email.message_id)
                .await
            {
                log::error!(
                    "[analyze] failed to mark permanent failure for {}: {}",
                    email.message_id,
                    db_err
                );
            }
        }
    }

    true
}

fn ensure_analysis_backlog_page_made_progress(
    summary: AnalyzeBatchSummary,
    emails: &[db::EmailRecord],
) -> anyhow::Result<()> {
    if summary.candidates == 0 || summary.made_durable_progress() {
        return Ok(());
    }

    let message_ids = emails
        .iter()
        .take(10)
        .map(|email| email.message_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if emails.len() > 10 { ", ..." } else { "" };
    anyhow::bail!(
        "analysis backlog made no durable progress for {} candidate email(s): {}{}",
        summary.candidates,
        message_ids,
        suffix
    )
}

async fn analyze_one_record_for_account(
    db: Arc<db::Database>,
    analyzer: &ai_analysis::EmailAnalyzer,
    account_id: &str,
    email: db::EmailRecord,
    options: AnalyzeRecordOptions<'_>,
) -> AnalyzeRecordOutcome {
    let started = std::time::Instant::now();
    let message_id = email.message_id.clone();

    match analyzer
        .analyze_with_worker_instruction(&email, options.worker_instruction)
        .await
    {
        Ok(mut result) => {
            let apply_result = if let Some(worker_id) = options.claim_worker_id {
                ai_analysis::apply_analysis_result_for_claimed_account(
                    db.as_ref(),
                    account_id,
                    &message_id,
                    worker_id,
                    &mut result,
                )
                .await
            } else {
                ai_analysis::apply_analysis_result_for_account(
                    db.as_ref(),
                    account_id,
                    &message_id,
                    &mut result,
                )
                .await
            };

            if let Err(e) = apply_result {
                let msg = e.to_string();
                let detail = format_error_chain(&e);
                log::error!("[analyze] failed to save {}: {}", message_id, detail);
                metrics::counter("analysis_save_failed_total", 1, &[]);
                let transient = is_transient_analysis_error(&msg);
                if transient {
                    log::error!("[analyze] stopping analysis: {}", msg);
                }
                let durable_progress = record_analysis_failure_for_account(
                    db.as_ref(),
                    account_id,
                    &email,
                    options.claim_worker_id,
                    &msg,
                    options.max_analysis_attempts,
                )
                .await;
                return AnalyzeRecordOutcome {
                    message_id,
                    error: Some(e.context("failed to save analysis result")),
                    transient,
                    durable_progress,
                };
            }
            if result.queued_for_frontier {
                match db
                    .enqueue_frontier_analysis_for_account(account_id, &message_id)
                    .await
                {
                    Ok(_) => {
                        if options.single {
                            println!("Analyzed (local, queued for frontier): {}", message_id);
                        }
                        metrics::counter("analysis_queued_for_frontier_total", 1, &[]);
                    }
                    Err(e) => {
                        log::warn!(
                            "[analyze] failed to enqueue {} for frontier: {}",
                            message_id,
                            e
                        );
                        if options.single {
                            println!("Analyzed (local): {}", message_id);
                        }
                    }
                }
            } else if options.single {
                println!("Analyzed: {}", message_id);
            }
            metrics::counter("analysis_success_total", 1, &[]);
            metrics::histogram(
                "analysis_latency_seconds",
                started.elapsed().as_secs_f64(),
                &[],
            );
            if options.single {
                if let Some(ref by) = result.analyzed_by {
                    println!("  analyzed_by: {}", by);
                }
                if let Some(ref u) = result.token_usage {
                    let total = u
                        .total_tokens()
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "?".into());
                    println!(
                        "  tokens: in={:?} out={:?} total={}",
                        u.input_tokens, u.output_tokens, total,
                    );
                }
                println!(
                    "  spam={:?} phishing={:?} marketing={:?} otp={:?} category={:?} org={:?} type={:?}",
                    result.spam_status,
                    result.phishing_status,
                    result.marketing_status,
                    result.otp_status,
                    result.category,
                    result.organization,
                    result.email_type,
                );
                if let Some(s) = result.human_summary.as_deref() {
                    let preview = if s.len() > 80 {
                        format!("{}...", &s[..77])
                    } else {
                        s.to_string()
                    };
                    println!("  summary: {}", preview);
                }
            }
            AnalyzeRecordOutcome {
                message_id,
                error: None,
                transient: false,
                durable_progress: true,
            }
        }
        Err(e) => {
            metrics::counter("analysis_failed_total", 1, &[]);
            let msg = e.to_string();
            log::warn!("[analyze] failed for {}: {}", message_id, msg);
            let transient = is_transient_analysis_error(&msg);
            if transient {
                log::error!("[analyze] stopping analysis: {}", msg);
            }
            let durable_progress = record_analysis_failure_for_account(
                db.as_ref(),
                account_id,
                &email,
                options.claim_worker_id,
                &msg,
                options.max_analysis_attempts,
            )
            .await;
            let detail = format_error_chain(&e);
            log::warn!("[analyze] failed {}: {}", message_id, detail);
            AnalyzeRecordOutcome {
                message_id,
                error: Some(e),
                transient,
                durable_progress,
            }
        }
    }
}

async fn analyze_records_for_account(
    db: Arc<db::Database>,
    analyzer: &ai_analysis::EmailAnalyzer,
    account_id: &str,
    emails: &[db::EmailRecord],
    options: AnalyzeBatchOptions<'_>,
) -> anyhow::Result<AnalyzeBatchSummary> {
    let single = emails.len() == 1;
    let mut first_error: Option<anyhow::Error> = None;
    let mut summary = AnalyzeBatchSummary {
        candidates: emails.len(),
        ..Default::default()
    };

    let concurrency = if single {
        1
    } else {
        options.analysis_concurrency.max(1).min(emails.len().max(1))
    };
    if concurrency > 1 {
        println!("Analyzing with concurrency {}", concurrency);
    }

    if concurrency == 1 {
        for email in emails {
            let email = email.clone();
            let worker_instruction = options
                .worker_instructions
                .and_then(|instructions| instructions.get(&email.message_id))
                .map(String::as_str);
            let outcome = analyze_one_record_for_account(
                db.clone(),
                analyzer,
                account_id,
                email,
                AnalyzeRecordOptions {
                    max_analysis_attempts: options.max_analysis_attempts,
                    claim_worker_id: options.claim_worker_id,
                    worker_instruction,
                    single,
                },
            )
            .await;
            if outcome.durable_progress {
                summary.durable_progress += 1;
            }
            if let Some(error) = outcome.error {
                summary.failed += 1;
                if outcome.transient {
                    return Err(error);
                }
                if first_error.is_none() {
                    first_error = Some(error);
                }
            } else {
                summary.succeeded += 1;
            }
        }
    } else {
        let email_tasks = emails.to_vec();
        let outcomes = stream::iter(email_tasks.into_iter().map(|email| {
            let db = db.clone();
            let worker_instruction = options
                .worker_instructions
                .and_then(|instructions| instructions.get(&email.message_id))
                .cloned();
            async move {
                analyze_one_record_for_account(
                    db,
                    analyzer,
                    account_id,
                    email,
                    AnalyzeRecordOptions {
                        max_analysis_attempts: options.max_analysis_attempts,
                        claim_worker_id: options.claim_worker_id,
                        worker_instruction: worker_instruction.as_deref(),
                        single,
                    },
                )
                .await
            }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

        for outcome in outcomes {
            if outcome.durable_progress {
                summary.durable_progress += 1;
            }
            if let Some(error) = outcome.error {
                summary.failed += 1;
                if outcome.transient {
                    return Err(error);
                }
                log::warn!(
                    "[analyze] completed batch with failed item {}",
                    outcome.message_id
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            } else {
                summary.succeeded += 1;
            }
        }
    }
    if !single {
        println!(
            "Analyzed {} of {} email(s) ({} failed)",
            summary.succeeded,
            emails.len(),
            summary.failed
        );
    }
    if single {
        if let Some(err) = first_error {
            return Err(err).context("single-message analyze failed");
        }
    }
    Ok(summary)
}

pub async fn run_analyze(message_id: Option<String>, force: bool) -> anyhow::Result<()> {
    run_analyze_with_limit(message_id, force, None).await
}

pub async fn run_analyze_with_limit(
    message_id: Option<String>,
    force: bool,
    limit_override: Option<usize>,
) -> anyhow::Result<()> {
    run_analyze_with_limit_for_account(message_id, force, limit_override, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_analyze_with_limit_for_account(
    message_id: Option<String>,
    force: bool,
    limit_override: Option<usize>,
    account_id: &str,
) -> anyhow::Result<()> {
    let limit: u32 = limit_override
        .map(|v| v.min(u32::MAX as usize) as u32)
        .unwrap_or_else(|| {
            std::env::var("ANALYZE_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10)
        });

    let Some(id) = message_id else {
        return run_claimed_analysis_batch_for_account(limit, force, None, account_id)
            .await
            .map(|_| ());
    };

    let runtime = build_analyze_runtime_context(account_id, None).await?;
    let db = runtime.db.clone();

    if force {
        println!("--force is ignored for single-message analyze.");
    }
    let email = db
        .get_email_by_message_id_for_account(account_id, &id)
        .await
        .context("Fetch email by message_id")?
        .ok_or_else(|| anyhow::anyhow!("No email found with message_id: {}", id))?;
    println!("Analyzing one record: {}", id);
    let emails = vec![email];

    analyze_records_for_account(
        db,
        &runtime.analyzer,
        account_id,
        &emails,
        AnalyzeBatchOptions {
            max_analysis_attempts: runtime.max_analysis_attempts,
            claim_worker_id: None,
            worker_instructions: None,
            analysis_concurrency: runtime.analysis_concurrency,
        },
    )
    .await
    .map(|_| ())
}

pub async fn run_analyze_worker(limit: usize, concurrency: Option<usize>) -> anyhow::Result<()> {
    run_analyze_worker_for_account(limit, concurrency, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_analyze_worker_for_account(
    limit: usize,
    concurrency: Option<usize>,
    account_id: &str,
) -> anyhow::Result<()> {
    run_claimed_analysis_batch_for_account(
        limit.min(u32::MAX as usize) as u32,
        false,
        concurrency,
        account_id,
    )
    .await
    .map(|_| ())
}

async fn analyze_claimed_records_for_runtime(
    runtime: &AnalyzeRuntimeContext,
    account_id: &str,
    emails: Vec<db::EmailRecord>,
) -> anyhow::Result<AnalyzeBatchSummary> {
    let claimed_message_ids: Vec<String> = emails
        .iter()
        .map(|email| email.message_id.clone())
        .collect();

    let result = analyze_records_for_account(
        runtime.db.clone(),
        &runtime.analyzer,
        account_id,
        &emails,
        AnalyzeBatchOptions {
            max_analysis_attempts: runtime.max_analysis_attempts,
            claim_worker_id: Some(&runtime.worker_id),
            worker_instructions: None,
            analysis_concurrency: runtime.analysis_concurrency,
        },
    )
    .await;

    if let Err(error) = runtime
        .db
        .release_analysis_claims_for_account(
            account_id,
            &runtime.worker_id,
            &claimed_message_ids,
            "analysis batch cleanup",
        )
        .await
    {
        log::warn!(
            "[analyze] failed to release remaining claims for worker {}: {}",
            runtime.worker_id,
            error
        );
    }

    result
}

async fn run_claimed_analysis_batch_for_account(
    limit: u32,
    force: bool,
    concurrency_override: Option<usize>,
    account_id: &str,
) -> anyhow::Result<AnalyzeBatchSummary> {
    let runtime = build_analyze_runtime_context(account_id, concurrency_override).await?;
    let emails = runtime
        .db
        .claim_analysis_emails_for_account(
            account_id,
            &runtime.worker_id,
            limit,
            force,
            runtime.analysis_lock_ttl_secs,
        )
        .await
        .context("Claim analysis emails")?;

    if force {
        println!(
            "Claimed {} emails for analysis (limit {}, force mode: reanalyze eligible rows, worker {})",
            emails.len(),
            limit,
            runtime.worker_id
        );
    } else {
        println!(
            "Claimed {} emails for analysis (limit {}, worker {})",
            emails.len(),
            limit,
            runtime.worker_id
        );
    }

    analyze_claimed_records_for_runtime(&runtime, account_id, emails).await
}

pub async fn run_analyze_backlog_for_account(
    account_id: &str,
    page_size: usize,
) -> anyhow::Result<()> {
    let page_size = page_size.max(1).min(u32::MAX as usize);
    let limit = page_size as u32;
    let runtime = build_analyze_runtime_context(account_id, None).await?;
    let db = runtime.db.clone();
    let mut total_progressed = 0usize;

    loop {
        let emails = db
            .claim_analysis_emails_for_account(
                account_id,
                &runtime.worker_id,
                limit,
                false,
                runtime.analysis_lock_ttl_secs,
            )
            .await
            .context("Claim analysis emails")?;
        if emails.is_empty() {
            break;
        }

        println!(
            "Analyzing {} email(s) from backlog page size {}",
            emails.len(),
            page_size
        );
        let summary =
            analyze_claimed_records_for_runtime(&runtime, account_id, emails.clone()).await?;
        ensure_analysis_backlog_page_made_progress(summary, &emails)?;
        total_progressed += summary.durable_progress;
    }

    println!(
        "Analysis backlog pass complete after {} durable progress record(s)",
        total_progressed
    );
    Ok(())
}

#[cfg(test)]
fn parse_priority_rank_from_batch_plan(plan: Option<&Value>) -> HashMap<String, usize> {
    let mut rank = HashMap::new();
    let Some(priority_order) = plan
        .and_then(|value| value.get("priority_order"))
        .and_then(|value| value.as_array())
    else {
        return rank;
    };

    for (index, message_id) in priority_order
        .iter()
        .filter_map(|value| value.as_str())
        .enumerate()
    {
        rank.entry(message_id.trim().to_string()).or_insert(index);
    }
    rank
}

#[cfg(test)]
fn parse_worker_instructions_from_batch_plan(plan: Option<&Value>) -> HashMap<String, String> {
    let mut instructions = HashMap::new();
    let Some(worker_instructions) = plan
        .and_then(|value| value.get("worker_instructions"))
        .and_then(|value| value.as_object())
    else {
        return instructions;
    };

    for (message_id, instruction) in worker_instructions {
        let trimmed_message_id = message_id.trim();
        let Some(instruction_text) = instruction.as_str() else {
            continue;
        };
        let trimmed_instruction = instruction_text.trim();
        if trimmed_message_id.is_empty() || trimmed_instruction.is_empty() {
            continue;
        }
        instructions.insert(
            trimmed_message_id.to_string(),
            trimmed_instruction.to_string(),
        );
    }
    instructions
}

#[cfg(test)]
fn reorder_batch_emails_by_priority(
    emails: &[db::EmailRecord],
    priority_rank: &HashMap<String, usize>,
) -> Vec<db::EmailRecord> {
    let mut ranked: Vec<(usize, usize, db::EmailRecord)> = emails
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, email)| {
            let rank = priority_rank
                .get(email.message_id.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            (rank, index, email)
        })
        .collect();
    ranked.sort_by_key(|(rank, index, _)| (*rank, *index));
    ranked.into_iter().map(|(_, _, email)| email).collect()
}

pub async fn run_embed_backfill(limit: usize) -> anyhow::Result<()> {
    run_embed_backfill_for_account(limit, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_embed_backfill_for_account(limit: usize, account_id: &str) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);

    db.ensure_vector_extension()
        .await
        .context("Ensure vector extension")?;
    if let Err(err) = db.ensure_hybrid_retrieval_indexes().await {
        log::warn!("Failed to ensure hybrid retrieval indexes: {}", err);
    }

    let embedder = embeddings::create_embedding_provider()
        .await
        .context("Embedding provider required for embed-backfill")?;
    embeddings::validate_embedding_model(&db, embedder.as_ref()).await?;
    let embedding_provider_name = embedder.provider_name().to_string();
    let rpm = if embedder.is_local() {
        None
    } else {
        ai_config.rate_limit_for_provider(&embedding_provider_name)
    };
    let embedder =
        wrap_embedding_provider_with_pressure(Arc::from(embedder), &embedding_provider_name, rpm);

    embed_backfill_loop(&db, embedder, account_id, limit).await
}

/// Shared backfill loop: fetches emails needing embeddings, generates them, stores them.
async fn embed_backfill_loop(
    db: &Arc<db::Database>,
    embedder: Arc<dyn embeddings::EmbeddingProvider>,
    account_id: &str,
    limit: usize,
) -> anyhow::Result<()> {
    let coverage_before = db
        .get_embedding_coverage_stats_for_account(account_id)
        .await
        .context("Get embedding coverage before backfill")?;
    let ratio_before = if coverage_before.total_with_text > 0 {
        coverage_before.with_embedding as f64 / coverage_before.total_with_text as f64
    } else {
        1.0
    };
    metrics::gauge(
        "embedding_coverage_ratio",
        ratio_before,
        &[("phase", "before_backfill")],
    );
    metrics::gauge(
        "embedding_backlog_count",
        coverage_before.without_embedding as f64,
        &[("phase", "before_backfill")],
    );

    let need_embedding = db
        .get_emails_needing_embedding_for_account(account_id, limit)
        .await?;
    if need_embedding.is_empty() {
        println!(
            "No emails needing embeddings. Coverage: {}/{} ({:.2}%).",
            coverage_before.with_embedding,
            coverage_before.total_with_text,
            ratio_before * 100.0
        );
        return Ok(());
    }
    println!(
        "Embedding {} email(s)... backlog={} coverage={:.2}%",
        need_embedding.len(),
        coverage_before.without_embedding,
        ratio_before * 100.0
    );

    let texts: Vec<String> = need_embedding
        .iter()
        .map(|(_, body)| embeddings::truncate_for_embedding(body, 16_384))
        .collect();
    let started = std::time::Instant::now();
    let embeddings_vec: Vec<Vec<f32>> = embedder.embed_batch(&texts).await?;
    metrics::histogram(
        "embed_backfill_generation_latency_seconds",
        started.elapsed().as_secs_f64(),
        &[],
    );

    let _model_lock = db.acquire_embedding_model_shared_lock().await?;
    embeddings::assert_embedding_model_current(db, embedder.as_ref()).await?;

    let mut done = 0;
    let mut failed = 0;
    for ((message_id, _), embedding) in need_embedding.iter().zip(embeddings_vec.iter()) {
        if let Err(e) = db
            .update_embedding_for_account(account_id, message_id, embedding)
            .await
        {
            log::warn!(
                "[embed] failed to update embedding for {}: {}",
                message_id,
                e
            );
            failed += 1;
        } else {
            done += 1;
        }
    }
    if done > 0 {
        metrics::counter("embed_backfill_done_total", done as u64, &[]);
    }
    if failed > 0 {
        metrics::counter("embed_backfill_failed_total", failed as u64, &[]);
    }
    let coverage_after = db
        .get_embedding_coverage_stats_for_account(account_id)
        .await
        .context("Get embedding coverage after backfill")?;
    let ratio_after = if coverage_after.total_with_text > 0 {
        coverage_after.with_embedding as f64 / coverage_after.total_with_text as f64
    } else {
        1.0
    };
    metrics::gauge(
        "embedding_coverage_ratio",
        ratio_after,
        &[("phase", "after_backfill")],
    );
    metrics::gauge(
        "embedding_backlog_count",
        coverage_after.without_embedding as f64,
        &[("phase", "after_backfill")],
    );
    println!(
        "Embedded {} of {} email(s). Coverage now: {}/{} ({:.2}%), backlog={}.",
        done,
        need_embedding.len(),
        coverage_after.with_embedding,
        coverage_after.total_with_text,
        ratio_after * 100.0,
        coverage_after.without_embedding
    );
    Ok(())
}

// ── embed-rebuild ───────────────────────────────────────────────────────────

pub async fn run_embed_rebuild(limit: usize) -> anyhow::Result<()> {
    run_embed_rebuild_for_account(limit, DEFAULT_ACCOUNT_ID).await
}

pub async fn run_embed_rebuild_for_account(limit: usize, account_id: &str) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let ai_config = ai::AIConfig::load().context("Load AI config")?;
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let db = Arc::new(database);

    db.ensure_vector_extension()
        .await
        .context("Ensure vector extension")?;

    // Probe the new embedding model
    let embedder = embeddings::create_embedding_provider()
        .await
        .context("Embedding provider required for embed-rebuild")?;

    let model = embedder.model_name().to_string();
    let dims = embedder.dimensions();

    let stored_model = db
        .get_system_metadata("embedding_model")
        .await?
        .unwrap_or_default();
    let stored_dims = db
        .get_system_metadata("embedding_dimensions")
        .await?
        .unwrap_or_default();
    println!(
        "Rebuilding embeddings:\n  Old: {} ({}d)\n  New: {} ({}d)",
        if stored_model.is_empty() {
            "(none)"
        } else {
            &stored_model
        },
        if stored_dims.is_empty() {
            "?"
        } else {
            &stored_dims
        },
        model,
        dims
    );

    {
        let _lock = db.acquire_embedding_model_lock().await?;

        // Step 1: Null all existing embeddings
        let nulled = db.null_all_embeddings().await?;
        println!(
            "Nulled {} existing embedding(s) across all accounts.",
            nulled
        );

        // Step 2: Rebuild HNSW index
        println!("Rebuilding HNSW index...");
        db.rebuild_embedding_index(dims).await?;
        println!("Index rebuilt.");

        // Step 3: Store new model metadata
        db.set_system_metadata("embedding_model", &model).await?;
        db.set_system_metadata("embedding_dimensions", &dims.to_string())
            .await?;
        println!("Stored new embedding model metadata.");
    }

    // Step 4: Backfill with the new model
    println!("Starting backfill with limit {}...", limit);
    let embedding_provider_name = embedder.provider_name().to_string();
    let rpm = if embedder.is_local() {
        None
    } else {
        ai_config.rate_limit_for_provider(&embedding_provider_name)
    };
    let embedder =
        wrap_embedding_provider_with_pressure(Arc::from(embedder), &embedding_provider_name, rpm);
    embed_backfill_loop(&db, embedder, account_id, limit).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn test_email_record(message_id: &str) -> db::EmailRecord {
        let now = Utc::now();
        db::EmailRecord {
            message_id: message_id.to_string(),
            subject: None,
            sender: None,
            received_date: Some(now),
            spam_status: "unknown".to_string(),
            phishing_status: "unknown".to_string(),
            marketing_status: "unknown".to_string(),
            otp_status: None,
            otp_code: None,
            otp_expires: None,
            threat_level: None,
            threat_indicators: None,
            uid: None,
            uid_validity: None,
            modseq: None,
            ai_summary: None,
            human_summary: None,
            category: None,
            subcategory: None,
            organization: None,
            subject_area: None,
            topic: None,
            location: None,
            location_recommendation: None,
            offer_expires: None,
            flag_color: None,
            imap_flag_color: None,
            imap_flag_color_updated_at: None,
            llm_recommended_flag_color: None,
            llm_flag_color_updated_at: None,
            related_message_ids: Vec::new(),
            email_type: None,
            is_read: false,
            raw_email_content: None,
            body_text: None,
            body_synced_at: None,
            message_size: None,
            message_tokens: None,
            analyzed_at: None,
            action_status: None,
            action_applied_at: None,
            analysis_attempts: 0,
            analysis_failed_at: None,
            analysis_permanent_failure: false,
            last_analysis_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn parse_priority_rank_dedupes_and_trims() {
        let plan = json!({
            "priority_order": ["id-2", " id-1 ", "id-2"]
        });
        let rank = parse_priority_rank_from_batch_plan(Some(&plan));

        assert_eq!(rank.len(), 2);
        assert_eq!(rank.get("id-2"), Some(&0));
        assert_eq!(rank.get("id-1"), Some(&1));
    }

    #[test]
    fn parse_worker_instructions_ignores_invalid_entries() {
        let plan = json!({
            "worker_instructions": {
                "id-1": " review links carefully ",
                "id-2": "",
                "id-3": 42
            }
        });
        let instructions = parse_worker_instructions_from_batch_plan(Some(&plan));

        assert_eq!(instructions.len(), 1);
        assert_eq!(
            instructions.get("id-1"),
            Some(&"review links carefully".to_string())
        );
    }

    #[test]
    fn backlog_progress_guard_rejects_pages_without_durable_progress() {
        let emails = vec![test_email_record("id-1")];
        let summary = AnalyzeBatchSummary {
            candidates: 1,
            failed: 1,
            ..Default::default()
        };

        let err = ensure_analysis_backlog_page_made_progress(summary, &emails).unwrap_err();
        assert!(err.to_string().contains("no durable progress"));
        assert!(err.to_string().contains("id-1"));
    }

    #[test]
    fn backlog_progress_guard_accepts_durable_progress() {
        let emails = vec![test_email_record("id-1")];
        let summary = AnalyzeBatchSummary {
            candidates: 1,
            failed: 1,
            durable_progress: 1,
            ..Default::default()
        };

        ensure_analysis_backlog_page_made_progress(summary, &emails).unwrap();
    }

    #[test]
    fn reorder_batch_emails_applies_priority_and_preserves_unranked_order() {
        let emails = vec![
            test_email_record("id-1"),
            test_email_record("id-2"),
            test_email_record("id-3"),
            test_email_record("id-4"),
        ];
        let plan = json!({
            "priority_order": ["id-3", "id-1"]
        });
        let rank = parse_priority_rank_from_batch_plan(Some(&plan));

        let ordered = reorder_batch_emails_by_priority(&emails, &rank);
        let message_ids: Vec<String> = ordered.into_iter().map(|email| email.message_id).collect();
        assert_eq!(
            message_ids,
            vec![
                "id-3".to_string(),
                "id-1".to_string(),
                "id-2".to_string(),
                "id-4".to_string()
            ]
        );
    }
}
