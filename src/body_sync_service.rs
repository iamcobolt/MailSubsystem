//! Sync internals extracted from main runtime orchestration.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::{config::DEFAULT_ACCOUNT_ID, db, imap, metrics};

/// Slow sync (standalone): fetch full messages for emails needing body sync.
pub const SLOW_SYNC_BATCH_SIZE: i32 = 25;
pub type BackfillBatchEntry = (i32, i32, String, bool, bool, bool, bool, bool, bool);

#[derive(Debug, Clone)]
pub struct BodySyncWorkerConfig {
    pub workers: usize,
    pub claim_batch_size: usize,
    pub max_attempts: i32,
    pub retry_base_secs: i64,
    pub max_retry_delay_secs: i64,
    pub stale_processing_secs: i64,
}

impl BodySyncWorkerConfig {
    pub fn from_env() -> Self {
        let workers = std::env::var("BODY_SYNC_WORKERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2usize)
            .max(1);
        let claim_batch_size = std::env::var("BODY_SYNC_CLAIM_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(SLOW_SYNC_BATCH_SIZE as usize)
            .max(1);
        let max_attempts = std::env::var("BODY_SYNC_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_i32)
            .max(1);
        let retry_base_secs = std::env::var("BODY_SYNC_RETRY_BASE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_i64)
            .max(1);
        let max_retry_delay_secs = std::env::var("BODY_SYNC_MAX_RETRY_DELAY_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600_i64)
            .max(1);
        let stale_processing_secs = std::env::var("BODY_SYNC_STALE_PROCESSING_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900_i64)
            .max(30);

        Self {
            workers,
            claim_batch_size,
            max_attempts,
            retry_base_secs,
            max_retry_delay_secs,
            stale_processing_secs,
        }
    }
}

fn retry_delay_secs(base_secs: i64, attempt_count: i32, max_delay_secs: i64) -> i64 {
    let exponent = (attempt_count - 1).clamp(0, 16) as u32;
    let factor = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
    base_secs
        .saturating_mul(factor)
        .clamp(1, max_delay_secs.max(1))
}

/// Flush a backfill batch: fetch from IMAP but only include in payload fields that are null in DB (fill_* = true).
pub async fn flush_backfill_batch(
    database: &db::Database,
    client: &imap::ImapClient,
    folder_name: &str,
    batch: &[BackfillBatchEntry],
) -> Result<u32> {
    if batch.is_empty() {
        return Ok(0);
    }
    let uids: Vec<u32> = batch
        .iter()
        .map(|(u, _, _, _, _, _, _, _, _)| *u as u32)
        .collect();
    let uid_validity = batch
        .first()
        .map(|(_, uv, _, _, _, _, _, _, _)| *uv as u32)
        .unwrap_or(0);
    let results = client
        .raw_uid_fetch_full_messages(folder_name, &uids, uid_validity)
        .await
        .context("Failed to fetch full messages")?;

    let uid_to_full: HashMap<u32, imap::FullMessageResult> = results.into_iter().collect();

    let payload_array: Vec<serde_json::Value> = batch
        .iter()
        .filter_map(
            |(
                uid,
                _uv,
                message_id,
                fill_subject,
                fill_sender,
                fill_rd,
                fill_raw,
                fill_body,
                fill_size,
            )| {
                let uid_u = *uid as u32;
                let full = uid_to_full.get(&uid_u)?;
                let raw = full.raw_email_content.replace('\0', " ");
                let body = full.body_text.as_ref().map(|s| s.replace('\0', " "));

                let mut related_ids = Vec::new();
                if let Some(ref in_reply_to) = full.in_reply_to {
                    if !in_reply_to.trim().is_empty() {
                        related_ids.push(in_reply_to.clone());
                    }
                }
                for ref_id in &full.references {
                    if !ref_id.trim().is_empty() && !related_ids.contains(ref_id) {
                        related_ids.push(ref_id.clone());
                    }
                }

                let subject = if *fill_subject {
                    full.subject.clone()
                } else {
                    None
                };
                let sender = if *fill_sender {
                    full.sender.clone()
                } else {
                    None
                };
                let received_date = if *fill_rd {
                    full.received_date.map(|d| d.to_rfc3339())
                } else {
                    None
                };
                let raw_email_content = if *fill_raw && !raw.is_empty() {
                    Some(raw)
                } else {
                    None
                };
                let body_text = if *fill_body {
                    body.filter(|s| !s.is_empty())
                } else {
                    None
                };
                let message_size = if *fill_size {
                    full.message_size.map(|s| s as i32)
                } else {
                    None
                };

                Some(serde_json::json!({
                    "message_id": message_id,
                    "location": folder_name,
                    "uid": *uid,
                    "uid_validity": uid_validity as i32,
                    "subject": subject,
                    "sender": sender,
                    "received_date": received_date,
                    "recipients_to": full.recipients_to,
                    "recipients_cc": full.recipients_cc,
                    "recipients_bcc": full.recipients_bcc,
                    "raw_email_content": raw_email_content,
                    "body_text": body_text,
                    "is_read": full.is_read,
                    "message_size": message_size,
                    "modseq": full.modseq.map(|m| m as i64),
                    "list_unsubscribe": full.list_unsubscribe,
                    "list_id": full.list_id,
                    "x_priority": full.x_priority,
                    "return_path": full.return_path,
                    "reply_to": full.reply_to,
                    "custom_headers": full.custom_headers,
                    "related_message_ids": related_ids,
                }))
            },
        )
        .collect();

    if payload_array.is_empty() {
        return Ok(0);
    }

    let message_ids_to_mark: Vec<String> = payload_array
        .iter()
        .filter_map(|obj| {
            let mid = obj
                .get("message_id")
                .and_then(|v| v.as_str())
                .map(String::from)?;
            let has_raw = obj
                .get("raw_email_content")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
            let has_body = obj
                .get("body_text")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
            if has_raw || has_body {
                Some(mid)
            } else {
                None
            }
        })
        .collect();
    let payload = serde_json::Value::Array(payload_array);
    database
        .upsert_and_mark_body_synced(&payload, None, None, Some(&message_ids_to_mark))
        .await
        .context("Failed to upsert backfill")?;

    let highest = uid_to_full.keys().max().copied().unwrap_or(0);
    if highest > 0 {
        database
            .update_folder_last_full_sync_uid(folder_name, highest as i32)
            .await
            .context("Failed to update last_full_sync_uid")?;
    }
    let highest_modseq = uid_to_full
        .values()
        .filter_map(|full| full.modseq.map(|m| m as i64))
        .max();
    if let Some(modseq) = highest_modseq {
        database
            .update_folder_highest_modseq(folder_name, modseq)
            .await
            .context("Failed to update highest_modseq")?;
    }
    Ok(message_ids_to_mark.len() as u32)
}

/// Queue missing full-body rows into durable body_sync_queue for later workers.
pub async fn enqueue_existing_missing_body_rows(
    database: &db::Database,
    per_folder_limit: i32,
) -> Result<u64> {
    enqueue_existing_missing_body_rows_for_account(database, DEFAULT_ACCOUNT_ID, per_folder_limit)
        .await
}

pub async fn enqueue_existing_missing_body_rows_for_account(
    database: &db::Database,
    account_id: &str,
    per_folder_limit: i32,
) -> Result<u64> {
    let folders = database
        .list_imap_folders_for_account(account_id)
        .await
        .context("list folders for body queue seeding")?;
    let mut total = 0u64;
    for folder in folders.into_iter().filter(|f| !f.is_noselect) {
        let needing = database
            .get_emails_needing_body_sync_for_account(
                account_id,
                &folder.folder_name,
                per_folder_limit,
            )
            .await
            .with_context(|| format!("get emails needing body sync for {}", folder.folder_name))?;
        if needing.is_empty() {
            continue;
        }
        let items: Vec<db::BodySyncQueueItem> = needing
            .into_iter()
            .map(|(uid, message_id, uid_validity)| db::BodySyncQueueItem {
                folder_name: folder.folder_name.clone(),
                uid,
                uid_validity,
                message_id,
            })
            .collect();
        total += database
            .enqueue_body_sync_items_for_account(account_id, &items)
            .await
            .with_context(|| format!("enqueue body sync items for {}", folder.folder_name))?;
    }
    Ok(total)
}

/// Fetch full bodies for one folder batch and return message_ids that were successfully synced.
async fn flush_body_sync_batch(
    database: &db::Database,
    account_id: &str,
    client: &imap::ImapClient,
    folder_name: &str,
    batch: &[(i32, i32, String)],
) -> Result<Vec<String>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let uids: Vec<u32> = batch.iter().map(|(u, _, _)| *u as u32).collect();
    let uid_validity = batch.first().map(|(_, uv, _)| *uv as u32).unwrap_or(0);
    let results = client
        .raw_uid_fetch_full_messages(folder_name, &uids, uid_validity)
        .await
        .context("Failed to fetch full messages")?;

    let uid_to_full: HashMap<u32, imap::FullMessageResult> = results.into_iter().collect();

    let payload_array: Vec<serde_json::Value> = batch
        .iter()
        .filter_map(|(uid, _uv, message_id)| {
            let uid_u = *uid as u32;
            let full = uid_to_full.get(&uid_u)?;
            let raw = full.raw_email_content.replace('\0', " ");
            let body = full.body_text.as_ref().map(|s| s.replace('\0', " "));

            let mut related_ids = Vec::new();
            if let Some(ref in_reply_to) = full.in_reply_to {
                if !in_reply_to.trim().is_empty() {
                    related_ids.push(in_reply_to.clone());
                }
            }
            for ref_id in &full.references {
                if !ref_id.trim().is_empty() && !related_ids.contains(ref_id) {
                    related_ids.push(ref_id.clone());
                }
            }

            Some(serde_json::json!({
                "message_id": message_id,
                "location": folder_name,
                "uid": *uid,
                "uid_validity": uid_validity as i32,
                "subject": full.subject,
                "sender": full.sender,
                "received_date": full.received_date.map(|d| d.to_rfc3339()),
                "recipients_to": full.recipients_to,
                "recipients_cc": full.recipients_cc,
                "recipients_bcc": full.recipients_bcc,
                "raw_email_content": raw,
                "body_text": body,
                "is_read": full.is_read,
                "message_size": full.message_size.map(|s| s as i32),
                "modseq": full.modseq.map(|m| m as i64),
                "list_unsubscribe": full.list_unsubscribe,
                "list_id": full.list_id,
                "x_priority": full.x_priority,
                "return_path": full.return_path,
                "reply_to": full.reply_to,
                "custom_headers": full.custom_headers,
                "related_message_ids": related_ids,
            }))
        })
        .collect();

    if payload_array.is_empty() {
        return Ok(Vec::new());
    }

    let uids_to_mark: Vec<i32> = payload_array
        .iter()
        .filter_map(|obj| obj.get("uid").and_then(|u| u.as_i64()).map(|u| u as i32))
        .collect();
    let synced_message_ids: Vec<String> = payload_array
        .iter()
        .filter_map(|obj| {
            obj.get("message_id")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .collect();

    let payload = serde_json::Value::Array(payload_array);
    database
        .upsert_and_mark_body_synced_for_account(
            account_id,
            &payload,
            Some(folder_name),
            Some(&uids_to_mark),
            None,
        )
        .await
        .context("Failed to upsert and mark body synced")?;

    let highest = uid_to_full.keys().max().copied().unwrap_or(0);
    if highest > 0 {
        database
            .update_folder_last_full_sync_uid_for_account(account_id, folder_name, highest as i32)
            .await
            .context("Failed to update last_full_sync_uid")?;
    }

    let highest_modseq = uid_to_full
        .values()
        .filter_map(|full| full.modseq.map(|m| m as i64))
        .max();
    if let Some(modseq) = highest_modseq {
        database
            .update_folder_highest_modseq_for_account(account_id, folder_name, modseq)
            .await
            .context("Failed to update highest_modseq")?;
    }

    Ok(synced_message_ids)
}

async fn schedule_retry(
    database: &db::Database,
    account_id: &str,
    entry: &db::BodySyncQueueEntry,
    config: &BodySyncWorkerConfig,
    error: &str,
) -> Result<()> {
    let delay = retry_delay_secs(
        config.retry_base_secs,
        entry.attempt_count,
        config.max_retry_delay_secs,
    );
    let status = database
        .mark_body_sync_retry_or_dead_for_account(
            account_id,
            entry.id,
            entry.attempt_count,
            config.max_attempts,
            delay,
            error,
        )
        .await?;
    if status == "dead" {
        metrics::counter("body_sync_queue_dead_total", 1, &[]);
        eprintln!(
            "  body-sync: dead-letter id={} folder={} uid={} attempts={} error={}",
            entry.id, entry.folder_name, entry.uid, entry.attempt_count, error
        );
    } else {
        metrics::counter("body_sync_queue_retry_total", 1, &[]);
    }
    Ok(())
}

async fn process_claimed_folder_batch(
    database: &db::Database,
    account_id: &str,
    client: &imap::ImapClient,
    config: &BodySyncWorkerConfig,
    folder_name: &str,
    entries: Vec<db::BodySyncQueueEntry>,
) -> Result<u32> {
    if entries.is_empty() {
        return Ok(0);
    }
    let fetch_started = Instant::now();
    let fetch_batch: Vec<(i32, i32, String)> = entries
        .iter()
        .map(|e| (e.uid, e.uid_validity, e.message_id.clone()))
        .collect();

    match flush_body_sync_batch(database, account_id, client, folder_name, &fetch_batch).await {
        Ok(synced_message_ids) => {
            metrics::histogram(
                "body_sync_fetch_latency_seconds",
                fetch_started.elapsed().as_secs_f64(),
                &[("folder", folder_name)],
            );
            let synced_set: HashSet<String> = synced_message_ids.into_iter().collect();
            let done_ids: Vec<i64> = entries
                .iter()
                .filter(|e| synced_set.contains(&e.message_id))
                .map(|e| e.id)
                .collect();
            let done_count = if done_ids.is_empty() {
                0
            } else {
                database
                    .mark_body_sync_done_for_account(account_id, &done_ids)
                    .await? as u32
            };
            if done_count > 0 {
                metrics::counter("body_sync_queue_done_total", done_count as u64, &[]);
            }

            for entry in entries
                .iter()
                .filter(|e| !synced_set.contains(&e.message_id))
            {
                schedule_retry(
                    database,
                    account_id,
                    entry,
                    config,
                    "IMAP full fetch returned no content for queued UID",
                )
                .await?;
            }

            Ok(done_count)
        }
        Err(err) => {
            metrics::histogram(
                "body_sync_fetch_latency_seconds",
                fetch_started.elapsed().as_secs_f64(),
                &[("folder", folder_name)],
            );
            let error = err.to_string();
            for entry in &entries {
                schedule_retry(database, account_id, entry, config, &error).await?;
            }
            Ok(0)
        }
    }
}

async fn run_body_sync_worker(
    account_id: &str,
    worker_index: usize,
    database: db::Database,
    client: imap::ImapClient,
    config: BodySyncWorkerConfig,
) -> Result<u32> {
    let worker_id = format!("body-sync-{}-{}", std::process::id(), worker_index);
    let mut total_synced = 0u32;

    loop {
        let claimed = database
            .claim_body_sync_batch_for_account(account_id, &worker_id, config.claim_batch_size)
            .await
            .with_context(|| format!("claim body sync batch ({})", worker_id))?;
        if claimed.is_empty() {
            break;
        }
        metrics::counter("body_sync_queue_claim_total", claimed.len() as u64, &[]);

        let mut by_folder: HashMap<String, Vec<db::BodySyncQueueEntry>> = HashMap::new();
        for entry in claimed {
            by_folder
                .entry(entry.folder_name.clone())
                .or_default()
                .push(entry);
        }

        for (folder_name, entries) in by_folder {
            let n = process_claimed_folder_batch(
                &database,
                account_id,
                &client,
                &config,
                &folder_name,
                entries,
            )
            .await?;
            total_synced += n;
            if n > 0 {
                eprintln!("  {}: full sync +{} (durable queue)", folder_name, n);
            }
        }
    }

    Ok(total_synced)
}

/// Drain durable body sync queue with one or more workers.
pub async fn drain_body_sync_queue(
    database: db::Database,
    client: imap::ImapClient,
    config: BodySyncWorkerConfig,
) -> Result<u32> {
    drain_body_sync_queue_for_account(database, client, config, DEFAULT_ACCOUNT_ID).await
}

pub async fn drain_body_sync_queue_for_account(
    database: db::Database,
    client: imap::ImapClient,
    config: BodySyncWorkerConfig,
    account_id: &str,
) -> Result<u32> {
    let reset = database
        .reset_stale_body_sync_processing_for_account(account_id, config.stale_processing_secs)
        .await
        .context("reset stale body sync processing")?;
    if reset > 0 {
        eprintln!("Reset {} stale body-sync processing rows.", reset);
        metrics::counter("body_sync_queue_stale_reset_total", reset, &[]);
    }

    let mut handles = Vec::new();
    for worker_index in 1..=config.workers {
        let db = database.clone();
        let imap = client.clone();
        let cfg = config.clone();
        let account = account_id.to_string();
        handles.push(tokio::spawn(async move {
            run_body_sync_worker(&account, worker_index, db, imap, cfg).await
        }));
    }

    let mut total_synced = 0u32;
    for handle in handles {
        let synced = handle
            .await
            .context("body-sync worker join failed")?
            .context("body-sync worker failed")?;
        total_synced += synced;
    }

    let depth = database
        .body_sync_queue_depth_for_account(account_id)
        .await
        .context("query body sync queue depth")?;
    metrics::gauge("body_sync_queue_pending", depth.pending as f64, &[]);
    metrics::gauge("body_sync_queue_failed", depth.failed as f64, &[]);
    metrics::gauge("body_sync_queue_processing", depth.processing as f64, &[]);
    metrics::gauge("body_sync_queue_dead", depth.dead as f64, &[]);
    eprintln!(
        "Body sync queue depth: pending={} failed={} processing={} dead={}",
        depth.pending, depth.failed, depth.processing, depth.dead
    );

    if total_synced > 0 {
        println!("Slow sync: {} full messages fetched.", total_synced);
    }

    Ok(total_synced)
}
