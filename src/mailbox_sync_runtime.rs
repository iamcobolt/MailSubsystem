//! Sync runtime orchestration extracted from main.

use anyhow::Context;
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use sqlx::Row;
use std::time::Instant;

use crate::config::{AccountConfig, DEFAULT_ACCOUNT_ID};
use crate::db;
use crate::imap;
use crate::metrics;
use crate::sync_service::{
    drain_body_sync_queue, drain_body_sync_queue_for_account, enqueue_existing_missing_body_rows,
    BodySyncWorkerConfig, SLOW_SYNC_BATCH_SIZE,
};

fn empty_uid_range_end(start_uid: u32, max_uids: usize, uid_end: u32) -> u32 {
    start_uid
        .saturating_add(max_uids as u32)
        .saturating_sub(1)
        .min(uid_end)
}

pub async fn run_sync() -> anyhow::Result<()> {
    let account =
        AccountConfig::load(DEFAULT_ACCOUNT_ID).context("Failed to load account config")?;
    run_sync_for_account(&account).await
}

pub async fn run_sync_for_account(account: &AccountConfig) -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;

    let account_id = account.id.as_str();
    let client = imap::ImapClient::new(
        account.imap_server(),
        account.username.clone(),
        account.password.clone(),
    );

    let folders = client
        .list_mailboxes_with_attributes()
        .await
        .context("Failed to list mailboxes from IMAP")?;

    let result: db::FolderSyncResult = database
        .sync_folders_from_imap_for_account(account_id, &folders)
        .await
        .context("Failed to sync folders to database")?;

    println!("Synced {} folders to imap_folders.", result.new_count);

    let _ = client.disconnect().await;

    let db_folders = database
        .list_imap_folders_for_account(account_id)
        .await
        .context("Failed to list imap folders")?;
    let mut sync_folders: Vec<_> = db_folders.into_iter().filter(|f| !f.is_noselect).collect();
    sync_folders.sort_by(|a, b| {
        let inbox_a = if a.folder_name == "INBOX" { 0 } else { 1 };
        let inbox_b = if b.folder_name == "INBOX" { 0 } else { 1 };
        inbox_a
            .cmp(&inbox_b)
            .then_with(|| b.priority.unwrap_or(0).cmp(&a.priority.unwrap_or(0)))
            .then_with(|| a.folder_name.cmp(&b.folder_name))
    });

    const BATCH_SIZE: usize = 250;
    let mut total_synced = 0u32;
    metrics::gauge(
        "sync_folder_batch_size",
        sync_folders.len() as f64,
        &[("mode", "full")],
    );
    for folder in &sync_folders {
        let folder_started = Instant::now();
        let mut folder_synced = 0u32;
        let folder_name = &folder.folder_name;
        let mut start_uid: u32 = folder
            .last_synced_uid
            .map(|u| (u as u32).saturating_add(1))
            .unwrap_or(1);

        eprintln!(
            "  {}: syncing from UID {} (low-to-high)...",
            folder_name, start_uid
        );

        loop {
            let (mailbox, envelopes) = client
                .raw_uid_fetch_envelopes(folder_name, start_uid, BATCH_SIZE)
                .await
                .context("Failed to raw fetch envelopes")?;

            database
                .update_imap_folder_message_counts_for_account(
                    account_id,
                    &[(folder_name.clone(), mailbox.exists as i32)],
                )
                .await
                .context("Failed to update folder message count")?;

            let uid_validity = mailbox.uid_validity.unwrap_or(0);
            if uid_validity > 0 {
                let previous = database
                    .update_folder_uid_validity_for_account(
                        account_id,
                        folder_name,
                        uid_validity as i32,
                    )
                    .await
                    .context("Failed to update folder uid_validity")?;
                if let Some(prev) = previous {
                    if prev != 0 && prev != uid_validity as i32 {
                        eprintln!(
                            "  {}: UIDVALIDITY changed {} -> {}, resetting sync state",
                            folder_name, prev, uid_validity
                        );
                        database
                            .reset_folder_sync_state_for_account(
                                account_id,
                                folder_name,
                                uid_validity as i32,
                            )
                            .await
                            .context("Failed to reset folder sync state")?;
                        let cleared = database
                            .clear_folder_uids_for_account(account_id, folder_name)
                            .await
                            .context("Failed to clear folder UIDs")?;
                        eprintln!("  {}: cleared {} stale UID mappings", folder_name, cleared);
                        start_uid = 1;
                        let _ = client.disconnect().await;
                        continue;
                    }
                }
            }

            let uid_end = mailbox.uid_next.map(|n| n.saturating_sub(1)).unwrap_or(0);
            if envelopes.is_empty() {
                if uid_end == 0 {
                    database
                        .update_folder_last_synced_uid_for_account(account_id, folder_name, 0)
                        .await
                        .context("Failed to update last_synced_uid (empty mailbox)")?;
                    break;
                }
                if start_uid > uid_end {
                    break;
                }
                let end_of_range = empty_uid_range_end(start_uid, BATCH_SIZE, uid_end);
                eprintln!(
                    "  {}: no envelopes in range (start_uid={}), skipping to {}",
                    folder_name, start_uid, end_of_range
                );
                database
                    .update_folder_last_synced_uid_for_account(
                        account_id,
                        folder_name,
                        end_of_range as i32,
                    )
                    .await
                    .context("Failed to update last_synced_uid (empty range)")?;
                start_uid = end_of_range.saturating_add(1);
                let _ = client.disconnect().await;
                if start_uid > uid_end {
                    break;
                }
                continue;
            }

            let mut envelopes: Vec<(u32, imap::FetchEnvelopeResult)> = envelopes;
            let highest_fetched_uid = envelopes.iter().map(|(uid, _)| *uid).max().unwrap_or(0);
            const RETRIES: u32 = 2;
            for attempt in 0..=RETRIES {
                let (mut with_mid, without_mid): (Vec<_>, Vec<_>) =
                    envelopes.into_iter().partition(|(_, e)| {
                        e.message_id
                            .as_ref()
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                    });
                if without_mid.is_empty() {
                    envelopes = with_mid;
                    break;
                }
                if attempt == RETRIES {
                    for (uid, _) in &without_mid {
                        database
                            .record_missing_message_id_for_account(
                                account_id,
                                folder_name,
                                *uid as i32,
                                uid_validity as i32,
                            )
                            .await
                            .context("Failed to record missing message_id")?;
                    }
                    envelopes = with_mid;
                    break;
                }
                let mut still_without = Vec::new();
                for (uid, _) in without_mid {
                    let (_, retry_env) = client
                        .raw_uid_fetch_envelopes_by_uids(folder_name, &[uid])
                        .await
                        .context("Failed to retry fetch for missing Message-ID")?;
                    let env = retry_env
                        .into_iter()
                        .find(|(u, _)| *u == uid)
                        .map(|(_, e)| e)
                        .unwrap_or_default();
                    if env
                        .message_id
                        .as_ref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false)
                    {
                        with_mid.push((uid, env));
                    } else {
                        still_without.push((uid, env));
                    }
                }
                envelopes = with_mid
                    .into_iter()
                    .chain(still_without.into_iter())
                    .collect();
            }

            let payload_array: Vec<serde_json::Value> = envelopes
                .iter()
                .filter_map(|(uid, envelope)| {
                    let message_id = envelope
                        .message_id
                        .as_ref()
                        .filter(|s| !s.trim().is_empty())?;
                    Some(serde_json::json!({
                        "message_id": message_id.clone(),
                        "location": folder_name,
                        "uid": *uid as i32,
                        "uid_validity": uid_validity as i32,
                        "subject": envelope.subject,
                        "sender": envelope.from,
                        "recipients_to": envelope.to,
                        "recipients_cc": envelope.cc,
                        "recipients_bcc": envelope.bcc,
                        "received_date": envelope.date.map(|d| d.to_rfc3339()),
                    }))
                })
                .collect();

            if payload_array.is_empty() {
                if highest_fetched_uid > 0 {
                    database
                        .update_folder_last_synced_uid_for_account(
                            account_id,
                            folder_name,
                            highest_fetched_uid as i32,
                        )
                        .await
                        .context("Failed to update last_synced_uid for unimportable range")?;
                    eprintln!(
                        "  {}: no importable envelopes in fetched range; last_synced_uid={}",
                        folder_name, highest_fetched_uid
                    );
                    start_uid = highest_fetched_uid.saturating_add(1);
                    if highest_fetched_uid >= uid_end {
                        break;
                    }
                    continue;
                }
                break;
            }

            let payload = serde_json::Value::Array(payload_array.clone());
            let body_queue_items: Vec<db::BodySyncQueueItem> = envelopes
                .iter()
                .filter_map(|(uid, envelope)| {
                    envelope
                        .message_id
                        .as_ref()
                        .filter(|s| !s.trim().is_empty())
                        .map(|mid| db::BodySyncQueueItem {
                            folder_name: folder_name.clone(),
                            uid: *uid as i32,
                            uid_validity: uid_validity as i32,
                            message_id: mid.clone(),
                        })
                })
                .collect();
            database
                .upsert_envelopes_and_enqueue_body_sync_for_account(
                    account_id,
                    &payload,
                    &body_queue_items,
                )
                .await
                .context("Failed to upsert envelopes + enqueue body sync")?;

            let highest_uid = envelopes.iter().map(|(uid, _)| *uid).max().unwrap_or(0);
            database
                .update_folder_last_synced_uid_for_account(
                    account_id,
                    folder_name,
                    highest_uid as i32,
                )
                .await
                .context("Failed to update last_synced_uid")?;

            let count = envelopes.len() as u32;
            total_synced += count;
            folder_synced += count;
            metrics::counter(
                "sync_envelope_upserts_total",
                count as u64,
                &[("mode", "full"), ("folder", folder_name)],
            );
            eprintln!(
                "  {}: last_synced_uid={} (+{})",
                folder_name, highest_uid, count
            );
            start_uid = highest_uid.saturating_add(1);

            let _ = client.disconnect().await;

            if highest_uid >= uid_end {
                break;
            }
        }
        metrics::counter(
            "sync_folder_success_total",
            1,
            &[("mode", "full"), ("folder", folder_name)],
        );
        metrics::histogram(
            "sync_folder_latency_seconds",
            folder_started.elapsed().as_secs_f64(),
            &[("mode", "full"), ("folder", folder_name)],
        );
        metrics::gauge(
            "sync_folder_envelope_count",
            folder_synced as f64,
            &[("mode", "full"), ("folder", folder_name)],
        );
    }

    let worker_config = BodySyncWorkerConfig::from_env();
    let _ = drain_body_sync_queue_for_account(
        database.clone(),
        client.clone(),
        worker_config,
        account_id,
    )
    .await
    .context("Failed to process durable body sync queue")?;

    database
        .recompute_imap_folder_priority_for_account(account_id)
        .await
        .context("Failed to recompute folder priority")?;

    if total_synced > 0 {
        let email_count = database
            .count_emails_for_account(account_id)
            .await
            .context("Failed to verify email count")?;
        println!(
            "Synced {} message envelopes to emails. Total rows in emails table: {}",
            total_synced, email_count
        );
    }
    Ok(())
}

pub async fn run_sync_slow() -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;

    let (server, username, password) =
        imap::load_imap_from_env().context("Failed to load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let enqueued = enqueue_existing_missing_body_rows(&database, SLOW_SYNC_BATCH_SIZE)
        .await
        .context("Failed to enqueue missing body rows into durable queue")?;
    if enqueued > 0 {
        println!(
            "Enqueued {} body-sync row(s) for durable processing.",
            enqueued
        );
    }
    let worker_config = BodySyncWorkerConfig::from_env();
    let _ = drain_body_sync_queue(database.clone(), client.clone(), worker_config)
        .await
        .context("Failed to drain durable body sync queue")?;
    Ok(())
}

pub async fn run_sync_incremental_once() -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;

    let (server, username, password) =
        imap::load_imap_from_env().context("Failed to load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let folders = database
        .list_imap_folders()
        .await
        .context("Failed to list imap folders")?
        .into_iter()
        .filter(|f| !f.is_noselect && f.last_synced_uid.is_some())
        .collect::<Vec<_>>();

    if folders.is_empty() {
        println!("No folders to sync incrementally. Run 'sync' first to do initial sync.");
        return Ok(());
    }

    let mut total_changed = 0u32;
    let mut total_new = 0u32;
    let mut total_expunged = 0u64;
    metrics::gauge(
        "sync_folder_batch_size",
        folders.len() as f64,
        &[("mode", "incremental")],
    );

    for folder in &folders {
        let folder_started = Instant::now();
        let mut folder_new = 0u32;
        let mut folder_changed = 0u32;
        let mut folder_expunged = 0u64;
        let folder_name = &folder.folder_name;
        let highest_modseq = database
            .get_folder_highest_modseq(folder_name)
            .await
            .context("Failed to get highest_modseq")?
            .unwrap_or(0) as u64;

        if highest_modseq == 0 {
            eprintln!(
                "  {}: skipping (no MODSEQ tracked yet, run full sync first)",
                folder_name
            );
            continue;
        }

        eprintln!(
            "  {}: checking for changes since MODSEQ {}",
            folder_name, highest_modseq
        );
        eprintln!(
            "  {}: Note: MODSEQ = flag/metadata changes only. New messages detected by UID range check.",
            folder_name
        );

        let last_synced_uid = folder.last_synced_uid.unwrap_or(0) as u32;

        let new_uids_from_range = match client.get_new_uids(folder_name, last_synced_uid).await {
            Ok(uids) => {
                if !uids.is_empty() {
                    eprintln!(
                        "  {}: UID range check found {} new messages",
                        folder_name,
                        uids.len()
                    );
                }
                uids
            }
            Err(e) => {
                eprintln!("  {}: failed to get new UIDs: {}", folder_name, e);
                Vec::new()
            }
        };

        let server_highest_modseq = match client.get_highest_modseq(folder_name).await {
            Ok(Some(seq)) => {
                if seq > highest_modseq {
                    eprintln!(
                        "  {}: server MODSEQ increased {} -> {} (flag/metadata changes detected)",
                        folder_name, highest_modseq, seq
                    );
                } else {
                    eprintln!(
                        "  {}: server MODSEQ unchanged {} (no flag/metadata changes)",
                        folder_name, seq
                    );
                }
                seq
            }
            Ok(None) => {
                eprintln!(
                    "  {}: server doesn't support MODSEQ, checking UIDs only",
                    folder_name
                );
                if !new_uids_from_range.is_empty() {
                    eprintln!(
                        "  {}: found {} new UIDs (no MODSEQ support)",
                        folder_name,
                        new_uids_from_range.len()
                    );
                } else {
                    eprintln!("  {}: no new UIDs found", folder_name);
                    continue;
                }
                0
            }
            Err(e) => {
                eprintln!("  {}: failed to get highest MODSEQ: {}", folder_name, e);
                0
            }
        };

        let mut changed_uids_from_modseq = Vec::new();
        if server_highest_modseq > highest_modseq {
            eprintln!(
                "  {}: server MODSEQ increased {} -> {} (checking for flag/metadata changes)",
                folder_name, highest_modseq, server_highest_modseq
            );
            match client
                .search_uids_by_modseq(folder_name, highest_modseq + 1)
                .await
            {
                Ok(uids) => {
                    let count = uids.len();
                    changed_uids_from_modseq = uids;
                    if count > 0 {
                        eprintln!(
                            "  {}: found {} UIDs with MODSEQ changes (flags/metadata)",
                            folder_name, count
                        );
                    } else {
                        eprintln!(
                            "  {}: MODSEQ increased but no specific UIDs found (may be expunged messages)",
                            folder_name
                        );
                    }
                }
                Err(e) => {
                    eprintln!("  {}: failed to search by MODSEQ: {}", folder_name, e);
                }
            }
        } else if server_highest_modseq > 0 {
            eprintln!(
                "  {}: no MODSEQ changes (server {} <= tracked {})",
                folder_name, server_highest_modseq, highest_modseq
            );
        }

        let mut all_new_uids = new_uids_from_range;
        for uid in &changed_uids_from_modseq {
            if *uid > last_synced_uid && !all_new_uids.contains(uid) {
                all_new_uids.push(*uid);
            }
        }
        all_new_uids.sort_unstable();
        all_new_uids.dedup();

        let changed_existing_uids: Vec<_> = changed_uids_from_modseq
            .into_iter()
            .filter(|&uid| uid <= last_synced_uid)
            .collect();

        let new_uids = all_new_uids;

        let known_uids: Vec<u32> = database
            .get_folder_uids(folder_name)
            .await
            .context("Failed to get known UIDs")?
            .into_iter()
            .map(|u| u as u32)
            .collect();

        let expunged_uids = if !known_uids.is_empty() {
            let uid_validity = database
                .get_folder_uid_validity(folder_name)
                .await
                .context("Failed to get uid_validity")?
                .unwrap_or(0) as u32;
            match client
                .get_expunged_uids_qresync(folder_name, uid_validity, &known_uids)
                .await
            {
                Ok(uids) if !uids.is_empty() => {
                    eprintln!(
                        "  {}: QRESYNC detected {} expunged UIDs",
                        folder_name,
                        uids.len()
                    );
                    uids
                }
                _ => match client
                    .detect_expunged_uids_by_comparison(folder_name, &known_uids)
                    .await
                {
                    Ok(uids) if !uids.is_empty() => {
                        eprintln!(
                            "  {}: comparison detected {} expunged UIDs",
                            folder_name,
                            uids.len()
                        );
                        uids
                    }
                    _ => Vec::new(),
                },
            }
        } else {
            Vec::new()
        };

        if !expunged_uids.is_empty() {
            eprintln!(
                "  {}: processing {} expunged messages",
                folder_name,
                expunged_uids.len()
            );
            let expunged_i32: Vec<i32> = expunged_uids.iter().map(|&u| u as i32).collect();

            let expunged_rows: Vec<(String, i32)> = sqlx::query(
                "SELECT message_id, uid FROM emails WHERE account_id = $1 AND location = $2 AND uid = ANY($3)",
            )
            .bind(DEFAULT_ACCOUNT_ID)
            .bind(folder_name)
            .bind(&expunged_i32)
            .fetch_all(&database.pool)
            .await
            .context("Failed to get expunged rows")?
            .into_iter()
            .map(|r| (r.get::<String, _>("message_id"), r.get::<i32, _>("uid")))
            .collect();

            let other_folders: Vec<String> = database
                .list_imap_folders()
                .await
                .context("Failed to list folders")?
                .into_iter()
                .map(|f| f.folder_name)
                .filter(|n| n != folder_name)
                .collect();

            let max_search = 200usize;
            if expunged_rows.len() > max_search {
                eprintln!(
                    "  {}: {} expunged messages, limiting cross-folder searches to {}",
                    folder_name,
                    expunged_rows.len(),
                    max_search
                );
            }

            let mut resolved_uids: std::collections::HashSet<i32> =
                std::collections::HashSet::new();
            for (message_id, uid) in expunged_rows.iter().take(max_search) {
                for other in &other_folders {
                    if let Ok(uids) = client.search_uids_by_message_id(other, message_id).await {
                        if let Some(&found_uid) = uids.first() {
                            let uid_validity = database
                                .get_folder_uid_validity(other)
                                .await
                                .ok()
                                .flatten()
                                .unwrap_or(0);
                            if database
                                .update_email_location(
                                    message_id,
                                    other,
                                    found_uid as i32,
                                    uid_validity,
                                )
                                .await
                                .is_ok()
                            {
                                resolved_uids.insert(*uid);
                                eprintln!(
                                    "  {}: message {} found in {} (uid {}), updated location",
                                    folder_name, message_id, other, found_uid
                                );
                                break;
                            }
                        }
                    }
                }
            }

            let to_expunge: Vec<i32> = expunged_i32
                .into_iter()
                .filter(|uid| !resolved_uids.contains(uid))
                .collect();
            if !resolved_uids.is_empty() {
                eprintln!(
                    "  {}: {} messages resolved as moved to other folders",
                    folder_name,
                    resolved_uids.len()
                );
            }
            if !to_expunge.is_empty() {
                let expunged_count = database
                    .mark_emails_expunged(folder_name, &to_expunge)
                    .await
                    .context("Failed to mark emails as expunged")?;
                eprintln!(
                    "  {}: marked {} messages as expunged (deleted from server)",
                    folder_name, expunged_count
                );
                total_expunged += expunged_count;
                folder_expunged += expunged_count;
            }
        }

        if !new_uids.is_empty() {
            eprintln!(
                "  {}: processing {} new messages",
                folder_name,
                new_uids.len()
            );
            let mut all_envelopes = Vec::new();
            let mut last_mailbox_state = None;
            for chunk in new_uids.chunks(250) {
                let (mailbox_state, envelopes) = client
                    .raw_uid_fetch_envelopes_by_uids(folder_name, chunk)
                    .await
                    .context("Failed to fetch envelopes for new messages")?;
                last_mailbox_state = Some(mailbox_state);
                all_envelopes.extend(envelopes);
            }

            if let Some(ms) = last_mailbox_state.as_ref() {
                if let Some(uv) = ms.uid_validity {
                    let previous = database
                        .update_folder_uid_validity(folder_name, uv as i32)
                        .await
                        .context("Failed to update folder uid_validity")?;
                    if let Some(prev) = previous {
                        if prev != 0 && prev != uv as i32 {
                            eprintln!(
                                "  {}: UIDVALIDITY changed {} -> {}, resetting sync state",
                                folder_name, prev, uv
                            );
                            database
                                .reset_folder_sync_state(folder_name, uv as i32)
                                .await
                                .context("Failed to reset folder sync state")?;
                            let cleared = database
                                .clear_folder_uids(folder_name)
                                .await
                                .context("Failed to clear folder UIDs")?;
                            eprintln!("  {}: cleared {} stale UID mappings", folder_name, cleared);
                            continue;
                        }
                    }
                }
            }
            let envelopes = all_envelopes;

            let uid_validity = last_mailbox_state
                .as_ref()
                .and_then(|ms| ms.uid_validity)
                .unwrap_or(0);
            let payload_array: Vec<serde_json::Value> = envelopes
                .iter()
                .filter_map(|(uid, envelope)| {
                    let message_id = envelope
                        .message_id
                        .as_ref()
                        .filter(|s| !s.trim().is_empty())?;
                    Some(serde_json::json!({
                        "message_id": message_id.clone(),
                        "location": folder_name,
                        "uid": *uid as i32,
                        "uid_validity": uid_validity as i32,
                        "subject": envelope.subject,
                        "sender": envelope.from,
                        "recipients_to": envelope.to,
                        "recipients_cc": envelope.cc,
                        "recipients_bcc": envelope.bcc,
                        "received_date": envelope.date.map(|d| d.to_rfc3339()),
                    }))
                })
                .collect();

            if !payload_array.is_empty() {
                let payload = serde_json::Value::Array(payload_array);
                let body_queue_items: Vec<db::BodySyncQueueItem> = envelopes
                    .iter()
                    .filter_map(|(uid, envelope)| {
                        envelope
                            .message_id
                            .as_ref()
                            .filter(|s| !s.trim().is_empty())
                            .map(|mid| db::BodySyncQueueItem {
                                folder_name: folder_name.clone(),
                                uid: *uid as i32,
                                uid_validity: uid_validity as i32,
                                message_id: mid.clone(),
                            })
                    })
                    .collect();
                database
                    .upsert_envelopes_and_enqueue_body_sync(&payload, &body_queue_items)
                    .await
                    .context("Failed to upsert+enqueue new envelopes")?;

                let highest_new_uid = new_uids.iter().max().copied().unwrap_or(0);
                database
                    .update_folder_last_synced_uid(folder_name, highest_new_uid as i32)
                    .await
                    .context("Failed to update last_synced_uid")?;

                total_new += new_uids.len() as u32;
                folder_new += new_uids.len() as u32;
                metrics::counter(
                    "sync_envelope_upserts_total",
                    new_uids.len() as u64,
                    &[("mode", "incremental"), ("folder", folder_name)],
                );
                eprintln!(
                    "  {}: synced {} new envelopes and queued durable body sync",
                    folder_name,
                    new_uids.len()
                );
            }
        }

        if !changed_existing_uids.is_empty() {
            eprintln!(
                "  {}: updating flags for {} changed messages",
                folder_name,
                changed_existing_uids.len()
            );
            let flag_updates = client
                .fetch_flags_and_modseq(folder_name, &changed_existing_uids)
                .await
                .context("Failed to fetch flags for changed messages")?;

            let updates: Vec<(i32, bool)> = flag_updates
                .into_iter()
                .map(|(uid, is_read, _modseq)| (uid as i32, is_read))
                .collect();

            let updated_count = database
                .update_email_flags(folder_name, &updates)
                .await
                .context("Failed to update email flags")?;

            total_changed += updated_count as u32;
            folder_changed += updated_count as u32;
            eprintln!(
                "  {}: updated flags for {} messages",
                folder_name, updated_count
            );
        }

        if server_highest_modseq > 0 && server_highest_modseq > highest_modseq {
            database
                .update_folder_highest_modseq(folder_name, server_highest_modseq as i64)
                .await
                .context("Failed to update highest_modseq")?;
        }

        let _ = client.disconnect().await;
        metrics::counter(
            "sync_folder_success_total",
            1,
            &[("mode", "incremental"), ("folder", folder_name)],
        );
        metrics::histogram(
            "sync_folder_latency_seconds",
            folder_started.elapsed().as_secs_f64(),
            &[("mode", "incremental"), ("folder", folder_name)],
        );
        metrics::gauge(
            "sync_folder_new_envelopes",
            folder_new as f64,
            &[("mode", "incremental"), ("folder", folder_name)],
        );
        metrics::gauge(
            "sync_folder_changed_flags",
            folder_changed as f64,
            &[("mode", "incremental"), ("folder", folder_name)],
        );
        metrics::gauge(
            "sync_folder_expunged",
            folder_expunged as f64,
            &[("mode", "incremental"), ("folder", folder_name)],
        );
    }

    if total_new > 0 {
        let worker_config = BodySyncWorkerConfig::from_env();
        let _ = drain_body_sync_queue(database.clone(), client.clone(), worker_config)
            .await
            .context("Failed to process durable body sync queue after incremental sync")?;
    }

    if total_new > 0 || total_changed > 0 || total_expunged > 0 {
        println!(
            "Incremental sync complete: {} new, {} changed, {} expunged",
            total_new, total_changed, total_expunged
        );
    } else {
        println!("No changes detected.");
    }

    Ok(())
}

pub async fn run_sync_incremental_once_for_account(account: &AccountConfig) -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;
    let client = imap::ImapClient::new(
        account.imap_server(),
        account.username.clone(),
        account.password.clone(),
    );
    let account_id = account.id.as_str();

    let folders = database
        .list_imap_folders_for_account(account_id)
        .await
        .context("Failed to list imap folders")?
        .into_iter()
        .filter(|f| !f.is_noselect && f.last_synced_uid.is_some())
        .collect::<Vec<_>>();

    if folders.is_empty() {
        log::info!(
            "[account={}] incremental sync skipped: no seeded folders (run initial sync first)",
            account_id
        );
        return Ok(());
    }

    let mut total_new = 0u32;
    metrics::gauge(
        "sync_folder_batch_size",
        folders.len() as f64,
        &[("mode", "incremental"), ("account_id", account_id)],
    );

    for folder in &folders {
        let folder_started = Instant::now();
        let folder_name = &folder.folder_name;
        let last_synced_uid = folder.last_synced_uid.unwrap_or(0) as u32;

        let new_uids = match client.get_new_uids(folder_name, last_synced_uid).await {
            Ok(uids) => uids,
            Err(error) => {
                log::warn!(
                    "[account={}] incremental get_new_uids failed for folder={}: {}",
                    account_id,
                    folder_name,
                    error
                );
                continue;
            }
        };
        if new_uids.is_empty() {
            continue;
        }

        let mut all_envelopes = Vec::new();
        let mut last_mailbox_state = None;
        for chunk in new_uids.chunks(250) {
            let (mailbox_state, envelopes) = client
                .raw_uid_fetch_envelopes_by_uids(folder_name, chunk)
                .await
                .with_context(|| format!("fetch envelopes for {}", folder_name))?;
            last_mailbox_state = Some(mailbox_state);
            all_envelopes.extend(envelopes);
        }

        if let Some(ms) = last_mailbox_state.as_ref() {
            if let Some(uv) = ms.uid_validity {
                let previous = database
                    .update_folder_uid_validity_for_account(account_id, folder_name, uv as i32)
                    .await
                    .context("Failed to update folder uid_validity")?;
                if let Some(prev) = previous {
                    if prev != 0 && prev != uv as i32 {
                        log::warn!(
                            "[account={}] folder={} UIDVALIDITY changed {} -> {}, resetting sync state",
                            account_id,
                            folder_name,
                            prev,
                            uv
                        );
                        database
                            .reset_folder_sync_state_for_account(account_id, folder_name, uv as i32)
                            .await
                            .context("Failed to reset folder sync state")?;
                        let _ = database
                            .clear_folder_uids_for_account(account_id, folder_name)
                            .await
                            .context("Failed to clear folder UIDs")?;
                        continue;
                    }
                }
            }
        }

        let uid_validity = last_mailbox_state
            .as_ref()
            .and_then(|ms| ms.uid_validity)
            .unwrap_or(0);
        let payload_array: Vec<serde_json::Value> = all_envelopes
            .iter()
            .filter_map(|(uid, envelope)| {
                let message_id = envelope
                    .message_id
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())?;
                Some(serde_json::json!({
                    "message_id": message_id.clone(),
                    "location": folder_name,
                    "uid": *uid as i32,
                    "uid_validity": uid_validity as i32,
                    "subject": envelope.subject,
                    "sender": envelope.from,
                    "recipients_to": envelope.to,
                    "recipients_cc": envelope.cc,
                    "recipients_bcc": envelope.bcc,
                    "received_date": envelope.date.map(|d| d.to_rfc3339()),
                }))
            })
            .collect();

        if payload_array.is_empty() {
            continue;
        }
        let body_queue_items: Vec<db::BodySyncQueueItem> = all_envelopes
            .iter()
            .filter_map(|(uid, envelope)| {
                envelope
                    .message_id
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|mid| db::BodySyncQueueItem {
                        folder_name: folder_name.clone(),
                        uid: *uid as i32,
                        uid_validity: uid_validity as i32,
                        message_id: mid.clone(),
                    })
            })
            .collect();
        database
            .upsert_envelopes_and_enqueue_body_sync_for_account(
                account_id,
                &serde_json::Value::Array(payload_array),
                &body_queue_items,
            )
            .await
            .context("Failed to upsert+enqueue new envelopes")?;

        let highest_new_uid = new_uids.iter().max().copied().unwrap_or(0);
        database
            .update_folder_last_synced_uid_for_account(
                account_id,
                folder_name,
                highest_new_uid as i32,
            )
            .await
            .context("Failed to update last_synced_uid")?;

        let added = new_uids.len() as u32;
        total_new += added;
        metrics::counter(
            "sync_envelope_upserts_total",
            added as u64,
            &[
                ("mode", "incremental"),
                ("folder", folder_name.as_str()),
                ("account_id", account_id),
            ],
        );
        metrics::counter(
            "sync_folder_success_total",
            1,
            &[
                ("mode", "incremental"),
                ("folder", folder_name),
                ("account_id", account_id),
            ],
        );
        metrics::histogram(
            "sync_folder_latency_seconds",
            folder_started.elapsed().as_secs_f64(),
            &[
                ("mode", "incremental"),
                ("folder", folder_name),
                ("account_id", account_id),
            ],
        );
    }

    if total_new > 0 {
        let worker_config = BodySyncWorkerConfig::from_env();
        let _ = drain_body_sync_queue_for_account(
            database.clone(),
            client.clone(),
            worker_config,
            account_id,
        )
        .await
        .context("Failed to process durable body sync queue after incremental sync")?;
    }

    if total_new > 0 {
        log::info!(
            "[account={}] incremental sync complete: {} new",
            account_id,
            total_new
        );
    } else {
        log::debug!("[account={}] incremental sync: no changes", account_id);
    }
    Ok(())
}

fn format_imap_date(date: NaiveDate) -> String {
    date.format("%d-%b-%Y").to_string()
}

pub async fn run_sync_window(days: Option<i64>, full: bool) -> anyhow::Result<()> {
    let db_config = db::DatabaseConfig::load().context("Failed to load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Failed to connect to database")?;

    let (server, username, password) =
        imap::load_imap_from_env().context("Failed to load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let window_days = if full { None } else { Some(days.unwrap_or(30)) };
    let since_date = window_days.map(|d| Utc::now().date_naive() - ChronoDuration::days(d));
    let query = if let Some(date) = since_date {
        format!("SINCE {}", format_imap_date(date))
    } else {
        "ALL".to_string()
    };

    let folders = database
        .list_imap_folders()
        .await
        .context("Failed to list imap folders")?
        .into_iter()
        .filter(|f| !f.is_noselect)
        .collect::<Vec<_>>();

    if folders.is_empty() {
        println!("No selectable folders found.");
        return Ok(());
    }

    let mut total_synced = 0u32;
    metrics::gauge(
        "sync_folder_batch_size",
        folders.len() as f64,
        &[("mode", "window")],
    );
    for folder in &folders {
        let folder_started = Instant::now();
        let folder_name = &folder.folder_name;
        eprintln!("  {}: window sync query {}", folder_name, query);

        let uids = match client.search_uids(folder_name, &query).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("  {}: window search failed: {}", folder_name, e);
                continue;
            }
        };
        if uids.is_empty() {
            continue;
        }

        let mut all_envelopes: Vec<(u32, imap::FetchEnvelopeResult)> = Vec::new();
        let mut last_mailbox_state: Option<imap::MailboxState> = None;
        for chunk in uids.chunks(250) {
            let (mailbox_state, envelopes) = client
                .raw_uid_fetch_envelopes_by_uids(folder_name, chunk)
                .await
                .context("Failed to fetch envelopes for window")?;
            last_mailbox_state = Some(mailbox_state);
            all_envelopes.extend(envelopes);
        }

        if let Some(mailbox) = last_mailbox_state.as_ref() {
            database
                .update_imap_folder_message_counts(&[(folder_name.clone(), mailbox.exists as i32)])
                .await
                .context("Failed to update folder message count")?;

            let uid_validity = mailbox.uid_validity.unwrap_or(0);
            if uid_validity > 0 {
                let previous = database
                    .update_folder_uid_validity(folder_name, uid_validity as i32)
                    .await
                    .context("Failed to update folder uid_validity")?;
                if let Some(prev) = previous {
                    if prev != 0 && prev != uid_validity as i32 {
                        eprintln!(
                            "  {}: UIDVALIDITY changed {} -> {}, resetting sync state",
                            folder_name, prev, uid_validity
                        );
                        database
                            .reset_folder_sync_state(folder_name, uid_validity as i32)
                            .await
                            .context("Failed to reset folder sync state")?;
                        let cleared = database
                            .clear_folder_uids(folder_name)
                            .await
                            .context("Failed to clear folder UIDs")?;
                        eprintln!("  {}: cleared {} stale UID mappings", folder_name, cleared);
                        continue;
                    }
                }
            }
        }

        let uid_validity = last_mailbox_state
            .as_ref()
            .and_then(|ms| ms.uid_validity)
            .unwrap_or(0);
        let mut envelopes = all_envelopes;
        const RETRIES: u32 = 2;
        for attempt in 0..=RETRIES {
            let (mut with_mid, without_mid): (Vec<_>, Vec<_>) =
                envelopes.into_iter().partition(|(_, e)| {
                    e.message_id
                        .as_ref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false)
                });
            if without_mid.is_empty() {
                envelopes = with_mid;
                break;
            }
            if attempt == RETRIES {
                for (uid, _) in &without_mid {
                    database
                        .record_missing_message_id(folder_name, *uid as i32, uid_validity as i32)
                        .await
                        .context("Failed to record missing message_id")?;
                }
                envelopes = with_mid;
                break;
            }
            let mut still_without = Vec::new();
            for (uid, _) in without_mid {
                let (_, retry_env) = client
                    .raw_uid_fetch_envelopes_by_uids(folder_name, &[uid])
                    .await
                    .context("Failed to retry fetch for missing Message-ID")?;
                let env = retry_env
                    .into_iter()
                    .find(|(u, _)| *u == uid)
                    .map(|(_, e)| e)
                    .unwrap_or_default();
                if env
                    .message_id
                    .as_ref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    with_mid.push((uid, env));
                } else {
                    still_without.push((uid, env));
                }
            }
            envelopes = with_mid
                .into_iter()
                .chain(still_without.into_iter())
                .collect();
        }

        let payload_array: Vec<serde_json::Value> = envelopes
            .iter()
            .filter_map(|(uid, envelope)| {
                let message_id = envelope
                    .message_id
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())?;
                Some(serde_json::json!({
                    "message_id": message_id.clone(),
                    "location": folder_name,
                    "uid": *uid as i32,
                    "uid_validity": uid_validity as i32,
                    "subject": envelope.subject,
                    "sender": envelope.from,
                    "recipients_to": envelope.to,
                    "recipients_cc": envelope.cc,
                    "recipients_bcc": envelope.bcc,
                    "received_date": envelope.date.map(|d| d.to_rfc3339()),
                }))
            })
            .collect();

        if !payload_array.is_empty() {
            let payload = serde_json::Value::Array(payload_array);
            let body_queue_items: Vec<db::BodySyncQueueItem> = envelopes
                .iter()
                .filter_map(|(uid, envelope)| {
                    envelope
                        .message_id
                        .as_ref()
                        .filter(|s| !s.trim().is_empty())
                        .map(|mid| db::BodySyncQueueItem {
                            folder_name: folder_name.clone(),
                            uid: *uid as i32,
                            uid_validity: uid_validity as i32,
                            message_id: mid.clone(),
                        })
                })
                .collect();
            database
                .upsert_envelopes_and_enqueue_body_sync(&payload, &body_queue_items)
                .await
                .context("Failed to upsert+enqueue window envelopes")?;
            total_synced += envelopes.len() as u32;
            metrics::counter(
                "sync_envelope_upserts_total",
                envelopes.len() as u64,
                &[("mode", "window"), ("folder", folder_name)],
            );
            eprintln!(
                "  {}: window synced {} envelopes and queued body sync",
                folder_name,
                envelopes.len()
            );
        }
        metrics::counter(
            "sync_folder_success_total",
            1,
            &[("mode", "window"), ("folder", folder_name)],
        );
        metrics::histogram(
            "sync_folder_latency_seconds",
            folder_started.elapsed().as_secs_f64(),
            &[("mode", "window"), ("folder", folder_name)],
        );
    }

    let worker_config = BodySyncWorkerConfig::from_env();
    let _ = drain_body_sync_queue(database.clone(), client.clone(), worker_config)
        .await
        .context("Failed to process durable body sync queue after window sync")?;

    let window_key = window_days.map(|d| d as i32).unwrap_or(0);
    database
        .record_sync_window_run(window_key)
        .await
        .context("Failed to record window run")?;

    println!(
        "Window sync complete: {} messages (window={}).",
        total_synced,
        window_days
            .map(|d| d.to_string())
            .unwrap_or_else(|| "full".to_string())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_uid_range_advances_only_the_requested_batch() {
        assert_eq!(empty_uid_range_end(1, 250, 2356), 250);
        assert_eq!(empty_uid_range_end(2260, 250, 2356), 2356);
    }
}
