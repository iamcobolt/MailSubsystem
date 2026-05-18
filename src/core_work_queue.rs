//! Durable core work coordinator.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use tokio::sync::watch;
use tokio::time::sleep;

use crate::commands::{analysis_commands, location_commands};
use crate::config::{AccountConfig, DEFAULT_ACCOUNT_ID};
use crate::db::{self, CoreWorkQueueEntry, CoreWorkType, SubagentTaskRecord};
use crate::runtime_services;
use crate::subagent_runtime;
use crate::sync_runtime;

const DEFAULT_CORE_LOCATE_LIMIT: usize = 50;
const DEFAULT_CORE_ANALYZE_LIMIT: usize = 50;

#[derive(Debug, Clone)]
pub struct CoreCoordinatorConfig {
    pub account_id: String,
    pub worker_id: String,
    pub poll_interval_secs: u64,
    pub background_sync_enabled: bool,
    pub background_sync_interval_secs: u64,
    pub stale_after_secs: i64,
    pub retry_after_secs: i64,
    pub analyze_limit: usize,
    pub embed_limit: usize,
    pub locate_limit: usize,
    pub file_apply_enabled: bool,
    pub assistant_heartbeat_enabled: bool,
    pub assistant_heartbeat_interval_secs: u64,
    pub assistant_heartbeat_task_limit: usize,
    pub subagent_concurrency: usize,
}

impl CoreCoordinatorConfig {
    pub fn from_env() -> Self {
        let bool_env = |name: &str| {
            std::env::var(name)
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        };
        let usize_env = |name: &str, default_value: usize| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default_value)
        };
        let u64_env = |name: &str, default_value: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default_value)
        };
        let i64_env = |name: &str, default_value: i64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default_value)
        };

        Self {
            account_id: std::env::var("CORE_ACCOUNT_ID")
                .unwrap_or_else(|_| DEFAULT_ACCOUNT_ID.to_string()),
            worker_id: format!("core-{}", uuid::Uuid::new_v4()),
            poll_interval_secs: u64_env("CORE_WORK_POLL_INTERVAL_SECS", 15).max(1),
            background_sync_enabled: std::env::var("CORE_BACKGROUND_SYNC")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            background_sync_interval_secs: u64_env("CORE_BACKGROUND_SYNC_INTERVAL_SECS", 60).max(5),
            stale_after_secs: i64_env("CORE_WORK_STALE_AFTER_SECS", 600).max(1),
            retry_after_secs: i64_env("CORE_WORK_RETRY_AFTER_SECS", 60).max(0),
            analyze_limit: usize_env("CORE_ANALYZE_LIMIT", DEFAULT_CORE_ANALYZE_LIMIT),
            embed_limit: usize_env("CORE_EMBED_LIMIT", 50),
            locate_limit: usize_env("CORE_LOCATE_LIMIT", DEFAULT_CORE_LOCATE_LIMIT),
            file_apply_enabled: bool_env("CORE_FILE_APPLY"),
            assistant_heartbeat_enabled: std::env::var("MAIL_ASSISTANT_HEARTBEAT")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            assistant_heartbeat_interval_secs: u64_env(
                "MAIL_ASSISTANT_HEARTBEAT_INTERVAL_SECS",
                60,
            )
            .max(5),
            assistant_heartbeat_task_limit: usize_env("MAIL_ASSISTANT_HEARTBEAT_BATCH_SIZE", 50)
                .max(1),
            subagent_concurrency: usize_env("MAIL_ASSISTANT_SUBAGENT_CONCURRENCY", 4).max(1),
        }
    }
}

pub async fn run_core_coordinator_with_config(
    config: CoreCoordinatorConfig,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let db = runtime_services::load_database("core coordinator").await?;
    let Some(_runtime_lease) = try_acquire_core_runtime_lease(&config.account_id).await? else {
        log::warn!(
            "[core] another core runtime already owns account {}; exiting",
            config.account_id
        );
        eprintln!(
            "Another MailSubsystem core runtime is already running for account {}; exiting.",
            config.account_id
        );
        return Ok(());
    };

    let recovered = db
        .recover_orphaned_core_work_for_account(
            &config.account_id,
            "core startup recovered processing work without a live owner",
        )
        .await
        .context("recover orphaned core work at startup")?;
    if recovered > 0 {
        log::warn!(
            "[core] recovered {} orphaned processing work item(s) before starting",
            recovered
        );
    }

    let result = run_core_coordinator_loop(db.clone(), config.clone(), shutdown).await;
    let cleanup = release_worker_claims(&db, &config, "core coordinator shutdown").await;

    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => {
            Err(error.context(format!("shutdown cleanup also failed: {cleanup_error:#}")))
        }
    }
}

struct CoreRuntimeLease {
    _connection: PgConnection,
}

async fn try_acquire_core_runtime_lease(
    account_id: &str,
) -> anyhow::Result<Option<CoreRuntimeLease>> {
    let db_config = db::DatabaseConfig::load().context("load database config for runtime lease")?;
    let mut connection = PgConnection::connect(&db_config.connection_string())
        .await
        .context("connect for core runtime lease")?;
    let lock_key = core_runtime_lock_key(account_id);
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(lock_key)
        .fetch_one(&mut connection)
        .await
        .context("acquire core runtime advisory lock")?;

    if acquired {
        log::info!(
            "[core] acquired runtime lease account={} lock_key={}",
            account_id,
            lock_key
        );
        Ok(Some(CoreRuntimeLease {
            _connection: connection,
        }))
    } else {
        Ok(None)
    }
}

fn core_runtime_lock_key(account_id: &str) -> i64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in b"mailsubsystem-core-runtime" {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for byte in account_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash & 0x7fff_ffff_ffff_ffff) as i64
}

async fn run_core_coordinator_loop(
    db: Arc<db::Database>,
    config: CoreCoordinatorConfig,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    log::info!(
        "[core] coordinator started account={} worker={} poll={}s background_sync={} background_sync_interval={}s file_apply={}",
        config.account_id,
        config.worker_id,
        config.poll_interval_secs,
        config.background_sync_enabled,
        config.background_sync_interval_secs,
        config.file_apply_enabled
    );

    seed_bootstrap_work(&db, &config).await?;
    let background_sync_task = config.background_sync_enabled.then(|| {
        tokio::spawn(run_background_sync_loop(
            db.clone(),
            config.clone(),
            shutdown.clone(),
        ))
    });

    loop {
        if *shutdown.borrow() {
            break;
        }

        let did_work = run_once(&db, &config).await?;
        if did_work {
            tokio::task::yield_now().await;
            continue;
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(config.poll_interval_secs)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }

        enqueue_idle_work(&db, &config).await?;
    }

    if let Some(task) = background_sync_task {
        task.abort();
        let _ = task.await;
    }

    log::info!("[core] coordinator stopped");
    Ok(())
}

async fn release_worker_claims(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
    reason: &str,
) -> anyhow::Result<()> {
    let released = db
        .release_core_work_for_worker_for_account(&config.account_id, &config.worker_id, reason)
        .await
        .with_context(|| {
            format!(
                "release core work claimed by worker {} for account {}",
                config.worker_id, config.account_id
            )
        })?;
    if released > 0 {
        log::warn!(
            "[core] released {} processing work item(s) owned by worker {}",
            released,
            config.worker_id
        );
    }
    Ok(())
}

pub async fn cleanup_worker_claims_from_runtime(
    account_id: &str,
    worker_id: &str,
    reason: &str,
) -> anyhow::Result<u64> {
    let db = runtime_services::load_database("core shutdown cleanup").await?;
    db.release_core_work_for_worker_for_account(account_id, worker_id, reason)
        .await
        .with_context(|| {
            format!(
                "release core work claimed by worker {} for account {}",
                worker_id, account_id
            )
        })
}

async fn run_background_sync_loop(
    db: Arc<db::Database>,
    config: CoreCoordinatorConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(config.background_sync_interval_secs)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        }

        if *shutdown.borrow() {
            break;
        }

        match db
            .has_active_sync_work_for_account(&config.account_id)
            .await
        {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                log::warn!("[core] background sync status check failed: {:#}", error);
                continue;
            }
        }

        let account = match AccountConfig::load(&config.account_id) {
            Ok(account) => account,
            Err(error) => {
                log::warn!("[core] background sync skipped: {:#}", error);
                continue;
            }
        };

        log::debug!("[core] background incremental sync started");
        match sync_runtime::run_sync_incremental_once_for_account(&account).await {
            Ok(()) => {
                if let Err(error) =
                    enqueue_follow_up_work(&db, &config, CoreWorkType::SyncIncremental).await
                {
                    log::warn!(
                        "[core] background sync follow-up enqueue failed: {:#}",
                        error
                    );
                }
            }
            Err(error) => {
                log::warn!("[core] background incremental sync failed: {:#}", error);
            }
        }
    }
}

async fn seed_bootstrap_work(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
) -> anyhow::Result<()> {
    let snapshot = db
        .db_completeness_snapshot_for_account(&config.account_id)
        .await
        .context("load core bootstrap completeness snapshot")?;

    if snapshot.needs_full_sync_backfill() {
        let reason = if snapshot.email_count == 0 {
            "blank_database"
        } else {
            "partial_database_backfill"
        };
        enqueue_core_work(
            db,
            &config.account_id,
            CoreWorkType::SyncFull,
            "bootstrap",
            json!({"reason": reason}),
        )
        .await?;
    } else {
        enqueue_core_work(
            db,
            &config.account_id,
            CoreWorkType::SyncIncremental,
            "periodic",
            json!({"reason": "core_start"}),
        )
        .await?;
    }

    if snapshot.body_missing > 0
        || snapshot.body_sync.pending > 0
        || snapshot.body_sync.failed > 0
        || snapshot.body_sync.processing > 0
    {
        enqueue_core_work(
            db,
            &config.account_id,
            CoreWorkType::SyncBody,
            "body-backlog",
            json!({"reason": "active_body_backlog"}),
        )
        .await?;
    }

    if config.assistant_heartbeat_enabled {
        enqueue_core_work(
            db,
            &config.account_id,
            CoreWorkType::AssistantHeartbeat,
            "assistant-heartbeat",
            json!({"reason": "core_start", "requested_by": "mail-assistant"}),
        )
        .await?;
    }

    Ok(())
}

async fn run_once(db: &Arc<db::Database>, config: &CoreCoordinatorConfig) -> anyhow::Result<bool> {
    let reset = db
        .reset_stale_core_work_for_account(&config.account_id, config.stale_after_secs)
        .await
        .context("reset stale core work")?;
    if reset > 0 {
        log::info!("[core] reset {} stale work item(s)", reset);
    }

    let Some(work) = db
        .claim_core_work_for_account(&config.account_id, &config.worker_id)
        .await
        .context("claim core work")?
    else {
        return Ok(false);
    };

    let work_id = work.id;
    let work_type = work.work_type;
    log::info!("[core] claimed work id={} type={:?}", work_id, work_type);

    if work.work_type == CoreWorkType::SubagentTask && config.subagent_concurrency > 1 {
        let mut batch = vec![work];
        let remaining = config.subagent_concurrency.saturating_sub(1);
        if remaining > 0 {
            let more = db
                .claim_core_work_batch_for_account(
                    &config.account_id,
                    &config.worker_id,
                    CoreWorkType::SubagentTask,
                    remaining,
                )
                .await
                .context("claim subagent work batch")?;
            batch.extend(more);
        }
        let concurrency = config.subagent_concurrency.min(batch.len().max(1));
        stream::iter(batch.into_iter().map(|work| {
            let db = db.clone();
            let config = config.clone();
            async move { process_claimed_work(&db, &config, work).await }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
        return Ok(true);
    }

    process_claimed_work(db, config, work).await?;

    Ok(true)
}

async fn process_claimed_work(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
    work: CoreWorkQueueEntry,
) -> anyhow::Result<()> {
    let work_id = work.id;
    let work_type = work.work_type;

    match execute_work(db, &work, config).await {
        Ok(()) => {
            db.mark_core_work_done_for_account(&config.account_id, work_id)
                .await
                .context("mark core work done")?;
            enqueue_follow_up_work(db, config, work_type).await?;
        }
        Err(error) => {
            let status = db
                .mark_core_work_retry_or_dead_for_account(
                    &config.account_id,
                    work_id,
                    work.attempt_count,
                    work.max_attempts,
                    config.retry_after_secs,
                    &format!("{:#}", error),
                )
                .await
                .context("mark core work failed")?;
            log::warn!(
                "[core] work id={} type={:?} marked {}: {:#}",
                work_id,
                work_type,
                status,
                error
            );
        }
    }
    Ok(())
}

async fn enqueue_follow_up_work(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
    completed: CoreWorkType,
) -> anyhow::Result<()> {
    let snapshot = db
        .db_completeness_snapshot_for_account(&config.account_id)
        .await
        .context("load follow-up completeness snapshot")?;
    for (work_type, reason) in follow_up_work_plan(completed, config.file_apply_enabled, &snapshot)
    {
        enqueue_core_work(
            db,
            &config.account_id,
            work_type,
            "pipeline",
            json!({"reason": reason}),
        )
        .await?;
    }
    Ok(())
}

async fn enqueue_idle_work(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
) -> anyhow::Result<()> {
    let snapshot = db
        .db_completeness_snapshot_for_account(&config.account_id)
        .await
        .context("load idle completeness snapshot")?;

    for (work_type, reason) in idle_work_plan(&snapshot, config.file_apply_enabled) {
        enqueue_core_work(
            db,
            &config.account_id,
            work_type,
            "periodic",
            json!({"reason": reason}),
        )
        .await?;
    }

    if config.assistant_heartbeat_enabled
        && db
            .core_work_due_for_account(
                &config.account_id,
                CoreWorkType::AssistantHeartbeat,
                "assistant-heartbeat",
                config.assistant_heartbeat_interval_secs as i64,
            )
            .await
            .context("check assistant heartbeat due")?
    {
        enqueue_core_work(
            db,
            &config.account_id,
            CoreWorkType::AssistantHeartbeat,
            "assistant-heartbeat",
            json!({"reason": "heartbeat_due", "requested_by": "mail-assistant"}),
        )
        .await?;
    }

    Ok(())
}

fn follow_up_work_plan(
    completed: CoreWorkType,
    file_apply_enabled: bool,
    snapshot: &db::DbCompletenessSnapshot,
) -> Vec<(CoreWorkType, &'static str)> {
    let mut plan = Vec::new();

    match completed {
        CoreWorkType::SyncFull | CoreWorkType::SyncIncremental | CoreWorkType::SyncBody => {
            if snapshot.analysis_missing > 0 {
                plan.push((CoreWorkType::Analyze, "sync_completed"));
            }
        }
        CoreWorkType::Analyze => {
            plan.push((CoreWorkType::Embed, "analysis_completed"));
            if snapshot.location_missing > 0 {
                plan.push((CoreWorkType::Locate, "analysis_completed"));
            }
        }
        CoreWorkType::Embed => {
            if snapshot.location_missing > 0 {
                plan.push((CoreWorkType::Locate, "embed_completed"));
            }
        }
        CoreWorkType::Locate => {
            if snapshot.filing_pending > 0 {
                plan.push((CoreWorkType::FilePreview, "locate_completed"));
            }
        }
        CoreWorkType::FilePreview if file_apply_enabled && snapshot.filing_pending > 0 => {
            plan.push((CoreWorkType::FileApply, "file_preview_completed"));
        }
        CoreWorkType::FilePreview
        | CoreWorkType::FileApply
        | CoreWorkType::AssistantHeartbeat
        | CoreWorkType::SubagentTask => {}
    }

    if snapshot.analysis_missing > 0
        && !plan
            .iter()
            .any(|(work_type, _)| *work_type == CoreWorkType::Analyze)
    {
        plan.push((CoreWorkType::Analyze, "analysis_backlog"));
    }
    if snapshot.embedding_missing > 0
        && !plan
            .iter()
            .any(|(work_type, _)| *work_type == CoreWorkType::Embed)
    {
        plan.push((CoreWorkType::Embed, "embedding_backlog"));
    }
    if snapshot.location_missing > 0
        && !plan
            .iter()
            .any(|(work_type, _)| *work_type == CoreWorkType::Locate)
    {
        plan.push((CoreWorkType::Locate, "location_backlog"));
    }

    plan
}

fn idle_work_plan(
    snapshot: &db::DbCompletenessSnapshot,
    file_apply_enabled: bool,
) -> Vec<(CoreWorkType, &'static str)> {
    let mut plan = if snapshot.needs_full_sync_backfill() {
        let reason = if snapshot.email_count == 0 {
            "blank_database"
        } else {
            "partial_database_backfill"
        };
        vec![(CoreWorkType::SyncFull, reason)]
    } else {
        vec![(CoreWorkType::SyncIncremental, "core_idle_poll")]
    };

    if snapshot.body_missing > 0
        || snapshot.body_sync.pending > 0
        || snapshot.body_sync.failed > 0
        || snapshot.body_sync.processing > 0
    {
        plan.push((CoreWorkType::SyncBody, "body_backlog"));
    }
    if snapshot.analysis_missing > 0 {
        plan.push((CoreWorkType::Analyze, "analysis_backlog"));
    }
    if snapshot.embedding_missing > 0 {
        plan.push((CoreWorkType::Embed, "embedding_backlog"));
    }
    if snapshot.location_missing > 0 {
        plan.push((CoreWorkType::Locate, "location_backlog"));
    }
    if file_apply_enabled && snapshot.filing_pending > 0 {
        plan.push((CoreWorkType::FileApply, "filing_backlog"));
    }

    plan
}

async fn enqueue_core_work(
    db: &Arc<db::Database>,
    account_id: &str,
    work_type: CoreWorkType,
    idempotency_key: &str,
    payload: Value,
) -> anyhow::Result<()> {
    if should_coalesce_core_work(work_type)
        && db
            .has_active_core_work_type_for_account(account_id, work_type)
            .await
            .with_context(|| format!("check active core work {}", work_type.as_str()))?
    {
        let source = payload
            .get("requested_by")
            .and_then(|value| value.as_str())
            .or_else(|| payload.get("source").and_then(|value| value.as_str()))
            .unwrap_or("system");
        let reason = payload
            .get("reason")
            .and_then(|value| value.as_str())
            .unwrap_or("unspecified");
        log::debug!(
            target: "core_work",
            "{}",
            serde_json::json!({
                "event": "core_work_enqueue_skip",
                "account_id": account_id,
                "work_type": work_type.as_str(),
                "idempotency_key": idempotency_key,
                "source": source,
                "reason": reason,
                "skip_reason": "active_same_type",
            })
        );
        return Ok(());
    }

    db.enqueue_core_work_for_account(account_id, work_type, idempotency_key, payload)
        .await
        .with_context(|| format!("enqueue core work {}", work_type.as_str()))?;
    Ok(())
}

fn should_coalesce_core_work(work_type: CoreWorkType) -> bool {
    !matches!(
        work_type,
        CoreWorkType::AssistantHeartbeat | CoreWorkType::SubagentTask
    )
}

async fn execute_work(
    db: &Arc<db::Database>,
    work: &CoreWorkQueueEntry,
    config: &CoreCoordinatorConfig,
) -> anyhow::Result<()> {
    match work.work_type {
        CoreWorkType::SyncFull => {
            let account = AccountConfig::load(&config.account_id).context("load account config")?;
            sync_runtime::run_sync_for_account(&account).await
        }
        CoreWorkType::SyncIncremental => {
            let account = AccountConfig::load(&config.account_id).context("load account config")?;
            sync_runtime::run_sync_incremental_once_for_account(&account).await
        }
        CoreWorkType::SyncBody => sync_runtime::run_sync_slow().await,
        CoreWorkType::Analyze => {
            analysis_commands::run_analyze_with_limit_for_account(
                None,
                false,
                Some(limit_from_payload(
                    &work.payload,
                    "limit",
                    config.analyze_limit,
                )),
                &config.account_id,
            )
            .await
        }
        CoreWorkType::Embed => {
            analysis_commands::run_embed_backfill_for_account(
                limit_from_payload(&work.payload, "limit", config.embed_limit),
                &config.account_id,
            )
            .await
        }
        CoreWorkType::Locate => {
            location_commands::run_locate_with_limit_for_account(
                None,
                false,
                Some(limit_from_payload(
                    &work.payload,
                    "limit",
                    config.locate_limit,
                )),
                &config.account_id,
            )
            .await
        }
        CoreWorkType::FilePreview => {
            location_commands::run_file_for_account(true, &config.account_id).await
        }
        CoreWorkType::FileApply => {
            if !config.file_apply_enabled {
                log::info!("[core] skipping file_apply because CORE_FILE_APPLY is not enabled");
                return Ok(());
            }
            location_commands::run_file_for_account(false, &config.account_id).await
        }
        CoreWorkType::AssistantHeartbeat => execute_assistant_heartbeat(db, config).await,
        CoreWorkType::SubagentTask => {
            subagent_runtime::execute_subagent_task(
                db.clone(),
                &config.account_id,
                work.id,
                work.payload.clone(),
            )
            .await
        }
    }
}

fn limit_from_payload(payload: &Value, key: &str, default_value: usize) -> usize {
    payload
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default_value)
        .max(1)
}

struct SubagentEmailTask<'a> {
    account_id: &'a str,
    task_id: &'a str,
    task_kind: &'a str,
    worker_name: &'a str,
    skill_bundle: &'a str,
    emails: &'a [db::EmailRecord],
    input_context: Value,
}

async fn execute_assistant_heartbeat(
    db: &Arc<db::Database>,
    config: &CoreCoordinatorConfig,
) -> anyhow::Result<()> {
    let snapshot = db
        .db_completeness_snapshot_for_account(&config.account_id)
        .await
        .context("load heartbeat completeness snapshot")?;

    if snapshot.analysis_missing > 0 {
        let emails = db
            .get_unanalyzed_emails_for_account(
                &config.account_id,
                config.assistant_heartbeat_task_limit.min(u32::MAX as usize) as u32,
                false,
            )
            .await
            .context("load heartbeat classification candidates")?;
        enqueue_subagent_task_from_emails(
            db,
            SubagentEmailTask {
                account_id: &config.account_id,
                task_id: "heartbeat-email-classification",
                task_kind: "email_classification",
                worker_name: "classification-worker",
                skill_bundle: "email_classification",
                emails: &emails,
                input_context: json!({
                "snapshot": {
                    "analysis_missing": snapshot.analysis_missing,
                    "body_missing": snapshot.body_missing,
                },
                "instruction": "Classify these messages as structured artifacts only. Do not mutate mailbox state."
                }),
            },
        )
        .await?;
    }

    if snapshot.location_missing > 0 {
        let emails = db
            .get_emails_needing_location_for_account(
                &config.account_id,
                config.assistant_heartbeat_task_limit.min(u32::MAX as usize) as u32,
                false,
            )
            .await
            .context("load heartbeat folder recommendation candidates")?;
        enqueue_subagent_task_from_emails(
            db,
            SubagentEmailTask {
                account_id: &config.account_id,
                task_id: "heartbeat-folder-recommendation",
                task_kind: "folder_recommendation",
                worker_name: "folder-recommendation-worker",
                skill_bundle: "folder_recommendation",
                emails: &emails,
                input_context: json!({
                "snapshot": {
                    "location_missing": snapshot.location_missing,
                },
                "instruction": "Recommend folders as structured artifacts only. Core filing policy decides whether a move is allowed."
                }),
            },
        )
        .await?;
    }

    if snapshot.analysis_missing > 0 || snapshot.location_missing > 0 || snapshot.body_missing > 0 {
        db.insert_assistant_insight_for_account(db::AssistantInsightInsert {
            account_id: &config.account_id,
            insight_type: "mailbox_backlog",
            severity: "info",
            message:
                "Mail Assistant heartbeat found mailbox work that still needs worker processing.",
            related_message_id: None,
            related_folder: None,
            metadata: json!({
                "analysis_missing": snapshot.analysis_missing,
                "location_missing": snapshot.location_missing,
                "body_missing": snapshot.body_missing,
                "body_sync": {
                    "pending": snapshot.body_sync.pending,
                    "failed": snapshot.body_sync.failed,
                    "processing": snapshot.body_sync.processing,
                    "dead": snapshot.body_sync.dead,
                }
            }),
        })
        .await
        .context("record heartbeat assistant insight")?;
    }

    Ok(())
}

async fn enqueue_subagent_task_from_emails(
    db: &Arc<db::Database>,
    email_task: SubagentEmailTask<'_>,
) -> anyhow::Result<()> {
    let SubagentEmailTask {
        account_id,
        task_id,
        task_kind,
        worker_name,
        skill_bundle,
        emails,
        input_context,
    } = email_task;

    if emails.is_empty() {
        return Ok(());
    }

    let message_ids = emails
        .iter()
        .map(|email| email.message_id.clone())
        .collect::<Vec<_>>();
    let email_context = emails
        .iter()
        .map(|email| {
            json!({
                "message_id": email.message_id,
                "subject": email.subject.as_deref(),
                "sender": email.sender.as_deref(),
                "received_date": email.received_date.as_ref().map(|value| value.to_rfc3339()),
                "category": email.category.as_deref(),
                "organization": email.organization.as_deref(),
                "email_type": email.email_type.as_deref(),
                "location": email.location.as_deref(),
                "location_recommendation": email.location_recommendation.as_deref(),
                "human_summary": email.human_summary.as_deref(),
            })
        })
        .collect::<Vec<_>>();
    let task = SubagentTaskRecord {
        task_id: task_id.to_string(),
        task_kind: task_kind.to_string(),
        worker_name: worker_name.to_string(),
        skill_bundle: skill_bundle.to_string(),
        message_ids,
        input_context: json!({
            "heartbeat_context": input_context,
            "emails": email_context,
        }),
        priority: 0,
        correlation_id: "assistant-heartbeat".to_string(),
        created_by: "mail-assistant".to_string(),
    };

    db.upsert_subagent_task_for_account(account_id, &task, None, "pending")
        .await
        .context("record subagent heartbeat task")?;
    enqueue_core_work(
        db,
        account_id,
        CoreWorkType::SubagentTask,
        task_id,
        subagent_runtime::subagent_payload_from_record(&task, "assistant_heartbeat"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_up_preview_stays_dry_run_when_file_apply_disabled() {
        assert!(follow_up_work_plan(
            CoreWorkType::FilePreview,
            false,
            &db::DbCompletenessSnapshot::default()
        )
        .is_empty());
    }

    #[test]
    fn follow_up_preview_enqueues_apply_when_file_apply_enabled() {
        let snapshot = db::DbCompletenessSnapshot {
            filing_pending: 1,
            ..Default::default()
        };
        assert_eq!(
            follow_up_work_plan(CoreWorkType::FilePreview, true, &snapshot),
            vec![(CoreWorkType::FileApply, "file_preview_completed")]
        );
    }

    #[test]
    fn follow_up_locate_still_previews_before_apply() {
        let snapshot = db::DbCompletenessSnapshot {
            filing_pending: 1,
            ..Default::default()
        };
        assert_eq!(
            follow_up_work_plan(CoreWorkType::Locate, true, &snapshot),
            vec![(CoreWorkType::FilePreview, "locate_completed")]
        );
    }

    #[test]
    fn follow_up_file_apply_requeues_analysis_when_backlog_remains() {
        let snapshot = db::DbCompletenessSnapshot {
            analysis_missing: 919,
            ..Default::default()
        };
        assert_eq!(
            follow_up_work_plan(CoreWorkType::FileApply, true, &snapshot),
            vec![(CoreWorkType::Analyze, "analysis_backlog")]
        );
    }

    #[test]
    fn follow_up_analyze_keeps_locating_and_requeues_analysis_backlog() {
        let snapshot = db::DbCompletenessSnapshot {
            analysis_missing: 12,
            location_missing: 7,
            ..Default::default()
        };
        assert_eq!(
            follow_up_work_plan(CoreWorkType::Analyze, true, &snapshot),
            vec![
                (CoreWorkType::Embed, "analysis_completed"),
                (CoreWorkType::Locate, "analysis_completed"),
                (CoreWorkType::Analyze, "analysis_backlog")
            ]
        );
    }

    #[test]
    fn follow_up_requeues_embedding_backlog() {
        let snapshot = db::DbCompletenessSnapshot {
            embedding_missing: 4,
            ..Default::default()
        };
        assert_eq!(
            follow_up_work_plan(CoreWorkType::Locate, true, &snapshot),
            vec![(CoreWorkType::Embed, "embedding_backlog")]
        );
    }

    #[test]
    fn idle_work_plan_keeps_core_syncing_when_backlog_is_clear() {
        let snapshot = db::DbCompletenessSnapshot {
            folder_count: 6,
            email_count: 10,
            ..Default::default()
        };

        assert_eq!(
            idle_work_plan(&snapshot, false),
            vec![(CoreWorkType::SyncIncremental, "core_idle_poll")]
        );
    }

    #[test]
    fn idle_work_plan_recovers_partial_database() {
        let snapshot = db::DbCompletenessSnapshot {
            folder_count: 6,
            largest_folder_message_count: 1000,
            email_count: 29,
            ..Default::default()
        };

        assert_eq!(
            idle_work_plan(&snapshot, false),
            vec![(CoreWorkType::SyncFull, "partial_database_backfill")]
        );
    }

    #[test]
    fn idle_work_plan_requeues_missing_backlog() {
        let snapshot = db::DbCompletenessSnapshot {
            folder_count: 6,
            email_count: 10,
            analysis_missing: 12,
            embedding_missing: 5,
            location_missing: 7,
            ..Default::default()
        };

        assert_eq!(
            idle_work_plan(&snapshot, false),
            vec![
                (CoreWorkType::SyncIncremental, "core_idle_poll"),
                (CoreWorkType::Analyze, "analysis_backlog"),
                (CoreWorkType::Embed, "embedding_backlog"),
                (CoreWorkType::Locate, "location_backlog")
            ]
        );
    }

    #[test]
    fn idle_work_plan_bootstraps_blank_database() {
        assert_eq!(
            idle_work_plan(&db::DbCompletenessSnapshot::default(), false),
            vec![(CoreWorkType::SyncFull, "blank_database")]
        );
    }

    #[test]
    fn idle_work_plan_treats_observed_empty_mailbox_as_current() {
        let snapshot = db::DbCompletenessSnapshot {
            folder_count: 6,
            largest_folder_message_count: 0,
            email_count: 0,
            ..Default::default()
        };

        assert_eq!(
            idle_work_plan(&snapshot, false),
            vec![(CoreWorkType::SyncIncremental, "core_idle_poll")]
        );
    }

    #[test]
    fn idle_work_plan_applies_filing_backlog_only_when_enabled() {
        let snapshot = db::DbCompletenessSnapshot {
            folder_count: 6,
            email_count: 10,
            filing_pending: 3,
            ..Default::default()
        };

        assert_eq!(
            idle_work_plan(&snapshot, false),
            vec![(CoreWorkType::SyncIncremental, "core_idle_poll")]
        );
        assert_eq!(
            idle_work_plan(&snapshot, true),
            vec![
                (CoreWorkType::SyncIncremental, "core_idle_poll"),
                (CoreWorkType::FileApply, "filing_backlog")
            ]
        );
    }

    #[test]
    fn runtime_lock_key_is_stable_and_account_scoped() {
        assert_eq!(
            core_runtime_lock_key("default"),
            core_runtime_lock_key("default")
        );
        assert_ne!(
            core_runtime_lock_key("default"),
            core_runtime_lock_key("other")
        );
    }

    #[test]
    fn planner_coalesces_durable_pipeline_work_not_ephemeral_subagents() {
        assert!(should_coalesce_core_work(CoreWorkType::Analyze));
        assert!(should_coalesce_core_work(CoreWorkType::FileApply));
        assert!(!should_coalesce_core_work(CoreWorkType::AssistantHeartbeat));
        assert!(!should_coalesce_core_work(CoreWorkType::SubagentTask));
    }
}
