use anyhow::Context;

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::sync_service::{flush_backfill_batch, BackfillBatchEntry};
use crate::{db, imap};

use super::shared::DEFAULT_ENV_PATH;

pub async fn run_backfill_message_tokens() -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let n = database
        .backfill_message_tokens_for_account(DEFAULT_ACCOUNT_ID)
        .await?;
    println!("Backfilled message_tokens for {} row(s).", n);
    Ok(())
}

pub async fn run_backfill_message(message_id: Option<String>) -> anyhow::Result<()> {
    let message_id =
        message_id.ok_or_else(|| anyhow::anyhow!("Usage: backfill-message <message_id>"))?;
    let message_id = message_id.trim();
    if message_id.is_empty() {
        anyhow::bail!("Usage: backfill-message <message_id>");
    }

    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let (server, username, password) = imap::load_imap_from_env().context("Load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let (folder_name, uid, uid_validity) = if let Some(email) =
        database.get_email_by_message_id(message_id).await?
    {
        if let (Some(loc), Some(u), Some(uv)) = (email.location, email.uid, email.uid_validity) {
            (loc, u as u32, uv as u32)
        } else {
            let mailboxes = client.list_mailboxes_with_attributes().await?;
            let mut found = None;
            for (mb_name, is_noselect, _) in mailboxes {
                if is_noselect {
                    continue;
                }
                if let Ok(uids) = client.search_uids_by_message_id(&mb_name, message_id).await {
                    if let Some(&u) = uids.first() {
                        let uv = database
                            .get_folder_uid_validity(&mb_name)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or(0) as u32;
                        found = Some((mb_name, u, uv));
                        break;
                    }
                }
            }
            found.ok_or_else(|| anyhow::anyhow!("Message-ID not found in any mailbox"))?
        }
    } else {
        let mailboxes = client.list_mailboxes_with_attributes().await?;
        let mut found = None;
        for (mb_name, is_noselect, _) in mailboxes {
            if is_noselect {
                continue;
            }
            if let Ok(uids) = client.search_uids_by_message_id(&mb_name, message_id).await {
                if let Some(&u) = uids.first() {
                    let uv = database
                        .get_folder_uid_validity(&mb_name)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(0) as u32;
                    found = Some((mb_name, u, uv));
                    break;
                }
            }
        }
        found.ok_or_else(|| anyhow::anyhow!("Message-ID not found in any mailbox"))?
    };

    let results = client
        .raw_uid_fetch_full_messages(&folder_name, &[uid], uid_validity)
        .await
        .context("Failed to fetch message from IMAP")?;
    let (_uid, full) = results
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No data returned from IMAP fetch"))?;

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
    let mid = full
        .message_id
        .as_ref()
        .unwrap_or(&message_id.to_string())
        .clone();
    let payload = serde_json::json!([{
        "message_id": mid,
        "location": folder_name,
        "uid": uid as i32,
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
    }]);
    database
        .upsert_and_mark_body_synced(&payload, Some(&folder_name), Some(&[uid as i32]), None)
        .await
        .context("Failed to upsert message")?;

    println!(
        "Backfilled message {} from {}/uid {}",
        mid, folder_name, uid
    );
    Ok(())
}

pub async fn run_backfill_from_imap(limit: usize) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .context("Connect to database")?;
    let (server, username, password) = imap::load_imap_from_env().context("Load IMAP config")?;
    let client = imap::ImapClient::new(server, username, password);

    let needing = database
        .get_emails_needing_imap_backfill(limit)
        .await
        .context("Get emails needing IMAP backfill")?;
    if needing.is_empty() {
        println!("No emails needing IMAP backfill.");
        return Ok(());
    }

    let total_needing = needing.len();
    let mut by_folder: std::collections::HashMap<(String, i32), Vec<BackfillBatchEntry>> =
        std::collections::HashMap::new();
    for (
        message_id,
        location,
        uid,
        uid_validity,
        fill_subject,
        fill_sender,
        fill_rd,
        fill_raw,
        fill_body,
        fill_size,
    ) in needing
    {
        by_folder
            .entry((location.clone(), uid_validity))
            .or_default()
            .push((
                uid,
                uid_validity,
                message_id,
                fill_subject,
                fill_sender,
                fill_rd,
                fill_raw,
                fill_body,
                fill_size,
            ));
    }

    let batch_size = 25;
    let mut total = 0u32;
    for ((folder_name, _uid_validity), items) in by_folder {
        for chunk in items.chunks(batch_size) {
            let batch: Vec<BackfillBatchEntry> = chunk.to_vec();
            match flush_backfill_batch(&database, &client, &folder_name, &batch).await {
                Ok(n) => {
                    total += n;
                    println!("  {}: backfilled {} message(s)", folder_name, n);
                }
                Err(e) => eprintln!("  {}: failed: {}", folder_name, e),
            }
        }
    }
    println!(
        "Backfilled {} message(s) from IMAP ({} candidates).",
        total, total_needing
    );
    Ok(())
}

pub async fn run_migrate_schema(apply: bool) -> anyhow::Result<()> {
    let _ = dotenvy::from_path(DEFAULT_ENV_PATH);
    let db_config = db::DatabaseConfig::load().context("Load database config")?;
    let connection_string = db_config.connection_string();
    let mode = if apply {
        db::SchemaMigrationMode::Auto
    } else {
        db::SchemaMigrationMode::Validate
    };

    db::Database::new_with_schema_mode(&connection_string, mode)
        .await
        .context(if apply {
            "Apply embedded schema.sql"
        } else {
            "Validate database schema"
        })?;

    if apply {
        println!("Database schema is current after applying embedded schema.sql.");
    } else {
        println!("Database schema is current. No migration was applied.");
        println!("Use `mailsubsystem migrate-schema --apply` to intentionally apply schema.sql.");
    }
    Ok(())
}
