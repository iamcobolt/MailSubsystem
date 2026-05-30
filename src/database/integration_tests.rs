use super::*;
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::core_work::CoreWorkQueuePressure;
use crate::database::schema_management::EMBEDDED_SCHEMA;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;
use std::collections::HashSet;
use uuid::Uuid;

#[test]
fn test_analysis_backoff_intervals_are_exponential() {
    let backoff_minutes = |attempts: i32| 2_i64.pow(attempts.clamp(0, 6) as u32);

    assert_eq!(backoff_minutes(0), 1);
    assert_eq!(backoff_minutes(1), 2);
    assert_eq!(backoff_minutes(2), 4);
    assert_eq!(backoff_minutes(3), 8);
    assert_eq!(backoff_minutes(4), 16);
    assert_eq!(backoff_minutes(5), 32);
    assert_eq!(backoff_minutes(6), 64);
    assert_eq!(backoff_minutes(7), 64);
}

#[test]
fn test_db_completeness_snapshot_backlog_detection() {
    let ready = DbCompletenessSnapshot {
        folder_count: 4,
        selectable_folders_missing_counts: 0,
        largest_folder_message_count: 42,
        email_count: 42,
        missing_message_id: 0,
        body_missing: 0,
        analysis_missing: 0,
        analysis_ready: 0,
        embedding_missing: 0,
        location_missing: 0,
        filing_pending: 0,
        body_sync: BodySyncQueueDepth {
            pending: 0,
            failed: 0,
            processing: 0,
            dead: 3,
        },
    };
    assert!(!ready.has_active_backlog());

    let non_blocking_missing = DbCompletenessSnapshot {
        missing_message_id: 1,
        ..ready.clone()
    };
    assert!(!non_blocking_missing.has_active_backlog());

    let ready_empty_mailbox = DbCompletenessSnapshot {
        folder_count: 4,
        largest_folder_message_count: 0,
        email_count: 0,
        ..ready.clone()
    };
    assert!(!ready_empty_mailbox.needs_full_sync_backfill());
    assert!(!ready_empty_mailbox.has_active_backlog());

    let blocked_unobserved_folder_counts = DbCompletenessSnapshot {
        selectable_folders_missing_counts: 2,
        largest_folder_message_count: 0,
        email_count: 0,
        ..ready.clone()
    };
    assert!(blocked_unobserved_folder_counts.needs_full_sync_backfill());
    assert!(blocked_unobserved_folder_counts.has_active_backlog());

    let blocked_empty = DbCompletenessSnapshot {
        folder_count: 0,
        largest_folder_message_count: 0,
        email_count: 0,
        ..ready.clone()
    };
    assert!(blocked_empty.has_active_backlog());

    let blocked_partial = DbCompletenessSnapshot {
        largest_folder_message_count: 1000,
        email_count: 29,
        ..ready.clone()
    };
    assert!(blocked_partial.needs_full_sync_backfill());
    assert!(blocked_partial.has_active_backlog());

    let blocked_body = DbCompletenessSnapshot {
        body_missing: 1,
        ..ready.clone()
    };
    assert!(blocked_body.has_active_backlog());

    let blocked_analysis = DbCompletenessSnapshot {
        analysis_missing: 1,
        ..ready.clone()
    };
    assert!(blocked_analysis.has_active_backlog());

    let blocked_embedding = DbCompletenessSnapshot {
        embedding_missing: 1,
        ..ready.clone()
    };
    assert!(blocked_embedding.has_active_backlog());

    let blocked_location = DbCompletenessSnapshot {
        location_missing: 1,
        ..ready.clone()
    };
    assert!(blocked_location.has_active_backlog());

    let blocked_queue = DbCompletenessSnapshot {
        body_sync: BodySyncQueueDepth {
            pending: 0,
            failed: 1,
            processing: 0,
            dead: 0,
        },
        ..ready
    };
    assert!(blocked_queue.has_active_backlog());
}

async fn load_test_database() -> Option<Database> {
    let url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())?;
    let db = Database::new(&url).await.ok()?;
    let _ = sqlx::raw_sql(EMBEDDED_SCHEMA).execute(&db.pool).await;
    Some(db)
}

async fn cleanup_test_emails(db: &Database, prefix: &str) {
    let pattern = format!("{}%", prefix);
    let _ = sqlx::query("DELETE FROM emails WHERE message_id LIKE $1")
        .bind(pattern)
        .execute(&db.pool)
        .await;
}

async fn cleanup_test_agent_runs(db: &Database, prefix: &str) {
    let pattern = format!("{}%", prefix);
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id LIKE $1")
        .bind(pattern)
        .execute(&db.pool)
        .await;
}

async fn cleanup_test_batches(db: &Database, prefix: &str) {
    let pattern = format!("{}%", prefix);
    let _ = sqlx::query("DELETE FROM analysis_batches WHERE batch_id LIKE $1")
        .bind(pattern)
        .execute(&db.pool)
        .await;
}

async fn cleanup_test_core_work_account(db: &Database, account_id: &str) {
    let _ = sqlx::query("DELETE FROM subagent_skill_lessons WHERE account_id = $1")
        .bind(account_id)
        .execute(&db.pool)
        .await;
    let _ = sqlx::query("DELETE FROM subagent_results WHERE account_id = $1")
        .bind(account_id)
        .execute(&db.pool)
        .await;
    let _ = sqlx::query("DELETE FROM subagent_tasks WHERE account_id = $1")
        .bind(account_id)
        .execute(&db.pool)
        .await;
    let _ = sqlx::query("DELETE FROM core_work_queue WHERE account_id = $1")
        .bind(account_id)
        .execute(&db.pool)
        .await;
}

async fn cleanup_test_conversations(db: &Database, prefix: &str) {
    let pattern = format!("{}%", prefix);
    let _ = sqlx::query(
        "DELETE FROM conversation_messages WHERE message_id LIKE $1 OR thread_id LIKE $1",
    )
    .bind(&pattern)
    .execute(&db.pool)
    .await;
    let _ = sqlx::query("DELETE FROM conversation_threads WHERE thread_id LIKE $1")
        .bind(&pattern)
        .execute(&db.pool)
        .await;
}

// Requires TEST_DATABASE_URL or DATABASE_URL to connect to a real database.
// Run with: cargo test -- --ignored --test-threads=1
#[tokio::test]
#[ignore]
async fn test_conversation_threads_round_trip_and_account_isolation() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping conversation thread test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("conversation-scope-{}-", Uuid::new_v4());
    cleanup_test_conversations(&db, &prefix).await;

    let account_a = "conversation-account-a";
    let account_b = "conversation-account-b";
    let active_thread = format!("{}active", prefix);
    let empty_thread = format!("{}empty", prefix);
    let other_thread = format!("{}other", prefix);
    let context_email_id = format!("{}context-email", prefix);
    let message_1 = format!("{}msg-1", prefix);
    let message_2 = format!("{}msg-2", prefix);
    let other_message = format!("{}msg-3", prefix);
    let run_id = format!("{}run-1", prefix);

    db.create_thread_for_account(
        account_a,
        &active_thread,
        "email-analyzer",
        Some("Initial title"),
        Some(&context_email_id),
    )
    .await
    .expect("create active thread");
    db.create_thread_for_account(account_a, &empty_thread, "digest-agent", None, None)
        .await
        .expect("create empty thread");
    db.create_thread_for_account(
        account_b,
        &other_thread,
        "email-analyzer",
        Some("Other"),
        None,
    )
    .await
    .expect("create thread for account B");

    db.add_message_for_account(crate::db::ConversationMessageInsert {
        account_id: account_a,
        message_id: &message_1,
        thread_id: &active_thread,
        role: "user",
        content: "hello from account A",
        agent_name: None,
        agent_run_id: None,
    })
    .await
    .expect("add first message");
    db.add_message_for_account(crate::db::ConversationMessageInsert {
        account_id: account_a,
        message_id: &message_2,
        thread_id: &active_thread,
        role: "agent",
        content: "reply from agent",
        agent_name: Some("email-analyzer"),
        agent_run_id: Some(&run_id),
    })
    .await
    .expect("add second message");
    db.add_message_for_account(crate::db::ConversationMessageInsert {
        account_id: account_b,
        message_id: &other_message,
        thread_id: &other_thread,
        role: "user",
        content: "hello from account B",
        agent_name: None,
        agent_run_id: None,
    })
    .await
    .expect("add account B message");

    db.update_thread_title_for_account(account_a, &active_thread, Some("Renamed thread"))
        .await
        .expect("rename thread");

    let threads_a = db
        .list_threads_for_account(account_a, 10, 0)
        .await
        .expect("list threads for account A");
    assert_eq!(threads_a.len(), 2);
    assert_eq!(threads_a[0].thread_id, active_thread);
    assert_eq!(threads_a[0].title.as_deref(), Some("Renamed thread"));
    assert_eq!(
        threads_a[0].context_email_id.as_deref(),
        Some(context_email_id.as_str())
    );
    assert_eq!(threads_a[0].message_count, 2);
    assert!(threads_a[0].last_message_at.is_some());

    let empty_summary = threads_a
        .iter()
        .find(|thread| thread.thread_id == empty_thread)
        .expect("find empty thread");
    assert_eq!(empty_summary.message_count, 0);
    assert!(empty_summary.last_message_at.is_none());

    let threads_b = db
        .list_threads_for_account(account_b, 10, 0)
        .await
        .expect("list threads for account B");
    assert_eq!(threads_b.len(), 1);
    assert_eq!(threads_b[0].thread_id, other_thread);
    assert_eq!(threads_b[0].message_count, 1);

    let messages = db
        .get_thread_messages_for_account(account_a, &active_thread, 50)
        .await
        .expect("get account A messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message_id, message_1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].message_id, message_2);
    assert_eq!(messages[1].agent_name.as_deref(), Some("email-analyzer"));
    assert_eq!(messages[1].agent_run_id.as_deref(), Some(run_id.as_str()));

    let limited_messages = db
        .get_thread_messages_for_account(account_a, &active_thread, 1)
        .await
        .expect("get limited account A messages");
    assert_eq!(limited_messages.len(), 1);
    assert_eq!(limited_messages[0].message_id, message_2);

    db.delete_thread_for_account(account_a, &active_thread)
        .await
        .expect("delete active thread");

    let remaining_threads_a = db
        .list_threads_for_account(account_a, 10, 0)
        .await
        .expect("list remaining threads for account A");
    assert_eq!(remaining_threads_a.len(), 1);
    assert_eq!(remaining_threads_a[0].thread_id, empty_thread);

    let remaining_messages: i64 = sqlx::query_scalar(
        r#"
            SELECT COUNT(*)
            FROM conversation_messages
            WHERE account_id = $1
              AND thread_id = $2
            "#,
    )
    .bind(account_a)
    .bind(&active_thread)
    .fetch_one(&db.pool)
    .await
    .expect("count remaining messages");
    assert_eq!(remaining_messages, 0);

    cleanup_test_conversations(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_recompute_imap_folder_priority_keeps_normal_folders_out_of_priority_one() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping folder priority test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let mut tx = db.pool.begin().await.expect("begin tx");
    sqlx::query("TRUNCATE imap_folders")
        .execute(&mut *tx)
        .await
        .expect("truncate imap_folders");

    for (folder_name, message_count) in [
        ("INBOX", 100),
        ("Archive", 5),
        ("Trash", 2),
        ("Projects", 80),
        ("Receipts", 20),
        ("Finance", 10),
    ] {
        sqlx::query(
            r#"
                INSERT INTO imap_folders (
                    folder_name, delimiter, is_noselect, attributes, message_count, priority
                ) VALUES ($1, '/', FALSE, ARRAY[]::TEXT[], $2, NULL)
                "#,
        )
        .bind(folder_name)
        .bind(message_count)
        .execute(&mut *tx)
        .await
        .expect("insert imap_folders row");
    }

    sqlx::query("SELECT recompute_imap_folder_priority()")
        .execute(&mut *tx)
        .await
        .expect("recompute priority");

    let rows = sqlx::query("SELECT folder_name, priority FROM imap_folders ORDER BY folder_name")
        .fetch_all(&mut *tx)
        .await
        .expect("fetch priorities");

    let priorities: std::collections::HashMap<String, i32> = rows
        .into_iter()
        .map(|row| (row.get("folder_name"), row.get("priority")))
        .collect();

    assert_eq!(priorities.get("INBOX"), Some(&10));
    assert_eq!(priorities.get("Archive"), Some(&1));
    assert_eq!(priorities.get("Trash"), Some(&1));
    assert!(matches!(priorities.get("Projects"), Some(priority) if (2..=9).contains(priority)));
    assert!(matches!(priorities.get("Receipts"), Some(priority) if (2..=9).contains(priority)));
    assert!(matches!(priorities.get("Finance"), Some(priority) if (2..=9).contains(priority)));
}

#[tokio::test]
#[ignore]
async fn test_get_unanalyzed_emails_force_includes_analyzed_rows() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping analyze force test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("force-analyze-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let unanalyzed_id = format!("{}unanalyzed", prefix);
    let analyzed_id = format!("{}analyzed", prefix);

    sqlx::query(
            r#"
            INSERT INTO emails (message_id, received_date, body_text, analyzed_at, deleted_from_server_at)
            VALUES ($1, NOW(), 'body', NULL, NULL),
                   ($2, NOW(), 'body', NOW(), NULL)
            "#,
        )
        .bind(&unanalyzed_id)
        .bind(&analyzed_id)
        .execute(&db.pool)
        .await
        .expect("insert analyze test rows");

    let normal = db
        .get_unanalyzed_emails(20, false)
        .await
        .expect("normal selection");
    let forced = db
        .get_unanalyzed_emails(20, true)
        .await
        .expect("force selection");

    let normal_ids: HashSet<String> = normal.into_iter().map(|record| record.message_id).collect();
    let forced_ids: HashSet<String> = forced.into_iter().map(|record| record.message_id).collect();

    assert!(normal_ids.contains(&unanalyzed_id));
    assert!(!normal_ids.contains(&analyzed_id));
    assert!(forced_ids.contains(&unanalyzed_id));
    assert!(forced_ids.contains(&analyzed_id));

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_claimed_failed_attempt_can_mark_permanent_before_releasing_claim() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping claimed permanent failure test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("claimed-permanent-failure-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let message_id = format!("{}email", prefix);
    let worker_id = format!("{}worker", prefix);

    sqlx::query(
        r#"
            INSERT INTO emails (
                account_id,
                message_id,
                received_date,
                body_text,
                analysis_attempts,
                analysis_permanent_failure,
                analysis_locked_at,
                analysis_worker_id,
                analysis_lock_expires_at,
                deleted_from_server_at
            )
            VALUES ($1, $2, NOW(), 'body', 4, FALSE, NOW(), $3, NOW() + INTERVAL '1 hour', NULL)
            "#,
    )
    .bind(DEFAULT_ACCOUNT_ID)
    .bind(&message_id)
    .bind(&worker_id)
    .execute(&db.pool)
    .await
    .expect("insert claimed email row");

    let rows = db
        .record_analysis_attempt_failed_for_claimed_account(
            DEFAULT_ACCOUNT_ID,
            &message_id,
            &worker_id,
            "analysis failed",
            true,
        )
        .await
        .expect("record claimed failed attempt");

    assert_eq!(rows, 1);

    let row = sqlx::query(
        r#"
            SELECT
                analysis_attempts,
                analysis_permanent_failure,
                last_analysis_error,
                analysis_locked_at,
                analysis_worker_id,
                analysis_lock_expires_at
            FROM emails
            WHERE account_id = $1
              AND message_id = $2
            "#,
    )
    .bind(DEFAULT_ACCOUNT_ID)
    .bind(&message_id)
    .fetch_one(&db.pool)
    .await
    .expect("fetch claimed email row");

    assert_eq!(row.get::<i32, _>("analysis_attempts"), 5);
    assert!(row.get::<bool, _>("analysis_permanent_failure"));
    assert_eq!(
        row.get::<Option<String>, _>("last_analysis_error")
            .as_deref(),
        Some("analysis failed")
    );
    assert!(row
        .get::<Option<DateTime<Utc>>, _>("analysis_locked_at")
        .is_none());
    assert!(row.get::<Option<String>, _>("analysis_worker_id").is_none());
    assert!(row
        .get::<Option<DateTime<Utc>>, _>("analysis_lock_expires_at")
        .is_none());

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_emails_needing_location_force_includes_existing_recommendations() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping locate force test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("force-locate-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let needs_locate_id = format!("{}needs", prefix);
    let already_recommended_id = format!("{}recommended", prefix);

    sqlx::query(
            r#"
            INSERT INTO emails (
                message_id,
                received_date,
                body_text,
                analyzed_at,
                category,
                location,
                location_recommendation,
                spam_status,
                phishing_status,
                threat_level,
                deleted_from_server_at
            )
            VALUES
                ($1, NOW(), 'body', NOW(), 'financial', 'INBOX', NULL, 'not-spam', 'not-phishing', NULL, NULL),
                ($2, NOW(), 'body', NOW(), 'financial', 'INBOX', 'INBOX/Finance', 'not-spam', 'not-phishing', NULL, NULL)
            "#,
        )
        .bind(&needs_locate_id)
        .bind(&already_recommended_id)
        .execute(&db.pool)
        .await
        .expect("insert locate test rows");

    let normal = db
        .get_emails_needing_location(20, false)
        .await
        .expect("normal locate selection");
    let forced = db
        .get_emails_needing_location(20, true)
        .await
        .expect("force locate selection");

    let normal_ids: HashSet<String> = normal.into_iter().map(|record| record.message_id).collect();
    let forced_ids: HashSet<String> = forced.into_iter().map(|record| record.message_id).collect();

    assert!(normal_ids.contains(&needs_locate_id));
    assert!(!normal_ids.contains(&already_recommended_id));
    assert!(forced_ids.contains(&needs_locate_id));
    assert!(forced_ids.contains(&already_recommended_id));

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_sender_history_for_account_isolated() {
    let Some(db) = load_test_database().await else {
        eprintln!(
            "Skipping sender-history account isolation test (no TEST_DATABASE_URL or DATABASE_URL)"
        );
        return;
    };

    let prefix = format!("sender-scope-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let account_a = "sender-scope-account-a";
    let account_b = "sender-scope-account-b";
    let sender = "same-sender@example.com";
    let message_id_a = format!("{}a", prefix);
    let message_id_b = format!("{}b", prefix);
    let pending_id_a = format!("{}pending-a", prefix);

    sqlx::query(
            r#"
            INSERT INTO emails (
                account_id,
                message_id,
                sender,
                received_date,
                ai_summary,
                body_text,
                deleted_from_server_at
            )
            VALUES
                ($1, $2, $3, NOW(), jsonb_build_object('marker', $4), 'body', NULL),
                ($1, $8, $3, NOW() - INTERVAL '1 minute', NULL, 'pending body with relationship context', NULL),
                ($5, $6, $3, NOW(), jsonb_build_object('marker', $7), 'body', NULL)
            "#,
        )
        .bind(account_a)
        .bind(&message_id_a)
        .bind(sender)
        .bind("account-a")
        .bind(account_b)
        .bind(&message_id_b)
        .bind("account-b")
        .bind(&pending_id_a)
        .execute(&db.pool)
        .await
        .expect("insert sender-history isolation rows");

    let history_a = db
        .get_sender_history_for_account(account_a, Some(sender), 10)
        .await
        .expect("get sender history for account A");
    let history_b = db
        .get_sender_history_for_account(account_b, Some(sender), 10)
        .await
        .expect("get sender history for account B");

    assert_eq!(
        history_a.len(),
        2,
        "account A should see analyzed and pending sender-history rows"
    );
    assert_eq!(
        history_b.len(),
        1,
        "account B should see exactly one sender-history row"
    );
    assert_eq!(
        history_a[0]
            .get("ai_summary")
            .and_then(|v| v.get("marker"))
            .and_then(|v| v.as_str()),
        Some("account-a"),
        "account A history should not contain account B data"
    );
    assert_eq!(
        history_a[0].get("analysis_status").and_then(|v| v.as_str()),
        Some("analyzed")
    );
    assert_eq!(
        history_a[0].get("body_excerpt").and_then(|v| v.as_str()),
        Some("body")
    );
    assert_eq!(
        history_a[1].get("analysis_status").and_then(|v| v.as_str()),
        Some("pending")
    );
    assert_eq!(
        history_a[1].get("body_excerpt").and_then(|v| v.as_str()),
        Some("pending body with relationship context")
    );
    let history_a_without_current = db
        .get_sender_history_for_account_excluding(account_a, Some(sender), 10, Some(&message_id_a))
        .await
        .expect("get sender history excluding current");
    assert!(
        history_a_without_current
            .iter()
            .all(|entry| entry.get("message_id").and_then(|v| v.as_str())
                != Some(message_id_a.as_str())),
        "current message should be excluded when requested"
    );
    assert_eq!(
        history_b[0]
            .get("ai_summary")
            .and_then(|v| v.get("marker"))
            .and_then(|v| v.as_str()),
        Some("account-b"),
        "account B history should not contain account A data"
    );

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_list_agent_runs_for_account_isolated() {
    let Some(db) = load_test_database().await else {
        eprintln!(
            "Skipping agent-runs account isolation test (no TEST_DATABASE_URL or DATABASE_URL)"
        );
        return;
    };

    let prefix = format!("agent-runs-scope-{}-", Uuid::new_v4());
    cleanup_test_agent_runs(&db, &prefix).await;

    let account_a = "agent-runs-account-a";
    let account_b = "agent-runs-account-b";
    let agent_name = "agent-run-scope-test";
    let run_a = format!("{}a", prefix);
    let run_b = format!("{}b", prefix);

    sqlx::query(
        r#"
            INSERT INTO agent_runs (
                run_id,
                account_id,
                agent_name,
                task_id,
                status,
                steps,
                llm_calls,
                tool_calls,
                started_at,
                escalated
            )
            VALUES
                ($1, $2, $3, 'task-a', 'completed', 1, 1, 1, NOW(), false),
                ($4, $5, $3, 'task-b', 'completed', 1, 1, 1, NOW(), false)
            "#,
    )
    .bind(&run_a)
    .bind(account_a)
    .bind(agent_name)
    .bind(&run_b)
    .bind(account_b)
    .execute(&db.pool)
    .await
    .expect("insert account isolation runs");

    let runs_a = db
        .list_agent_runs_for_account(account_a, 10, None, Some(agent_name))
        .await
        .expect("list runs for account A");
    let runs_b = db
        .list_agent_runs_for_account(account_b, 10, None, Some(agent_name))
        .await
        .expect("list runs for account B");

    assert_eq!(runs_a.len(), 1);
    assert_eq!(runs_b.len(), 1);
    assert_eq!(runs_a[0].run_id, run_a);
    assert_eq!(runs_b[0].run_id, run_b);

    cleanup_test_agent_runs(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_agent_run_stats_for_account_isolated() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping agent-run stats account isolation test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("agent-stats-scope-{}-", Uuid::new_v4());
    cleanup_test_agent_runs(&db, &prefix).await;

    let account_a = "agent-stats-account-a";
    let account_b = "agent-stats-account-b";
    let agent_name = "stats-scope-agent";
    let run_a_completed = format!("{}a-completed", prefix);
    let run_a_failed = format!("{}a-failed", prefix);
    let run_b_completed = format!("{}b-completed", prefix);

    sqlx::query(
        r#"
            INSERT INTO agent_runs (
                run_id,
                account_id,
                agent_name,
                task_id,
                status,
                steps,
                llm_calls,
                tool_calls,
                input_tokens,
                output_tokens,
                duration_ms,
                started_at,
                escalated
            )
            VALUES
                ($1, $2, $3, 'task-a1', 'completed', 2, 1, 1, 20, 10, 500, NOW(), false),
                ($4, $2, $3, 'task-a2', 'failed',    1, 1, 1, NULL, NULL, NULL, NOW(), false),
                ($5, $6, $3, 'task-b1', 'completed', 4, 1, 1, 80, 20, 900, NOW(), false)
            "#,
    )
    .bind(&run_a_completed)
    .bind(account_a)
    .bind(agent_name)
    .bind(&run_a_failed)
    .bind(&run_b_completed)
    .bind(account_b)
    .execute(&db.pool)
    .await
    .expect("insert account stats isolation runs");

    let since = Utc::now() - chrono::Duration::hours(1);
    let stats_a = db
        .get_agent_run_stats_for_account(account_a, since)
        .await
        .expect("stats for account A");
    let stats_b = db
        .get_agent_run_stats_for_account(account_b, since)
        .await
        .expect("stats for account B");

    assert_eq!(stats_a.len(), 1);
    assert_eq!(stats_b.len(), 1);
    assert_eq!(stats_a[0].agent_name, agent_name);
    assert_eq!(stats_b[0].agent_name, agent_name);
    assert_eq!(stats_a[0].completed, 1);
    assert_eq!(stats_a[0].failed, 1);
    assert_eq!(stats_b[0].completed, 1);
    assert_eq!(stats_b[0].failed, 0);

    cleanup_test_agent_runs(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_backfill_message_tokens_for_account_isolated() {
    let Some(db) = load_test_database().await else {
        eprintln!(
                "Skipping backfill message_tokens account isolation test (no TEST_DATABASE_URL or DATABASE_URL)"
            );
        return;
    };

    let prefix = format!("backfill-tokens-scope-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let account_a = "backfill-account-a";
    let account_b = "backfill-account-b";
    let message_a = format!("{}a", prefix);
    let message_b = format!("{}b", prefix);

    sqlx::query(
        r#"
            INSERT INTO emails (
                account_id,
                message_id,
                body_text,
                raw_email_content,
                message_tokens,
                deleted_from_server_at
            )
            VALUES
                ($1, $2, 'alpha body text', NULL, NULL, NULL),
                ($3, $4, 'beta body text', NULL, NULL, NULL)
            "#,
    )
    .bind(account_a)
    .bind(&message_a)
    .bind(account_b)
    .bind(&message_b)
    .execute(&db.pool)
    .await
    .expect("insert backfill account isolation rows");

    let updated = db
        .backfill_message_tokens_for_account(account_a)
        .await
        .expect("backfill tokens for account A");
    assert_eq!(updated, 1);

    let tokens_a: Option<i32> = sqlx::query_scalar(
        "SELECT message_tokens FROM emails WHERE account_id = $1 AND message_id = $2",
    )
    .bind(account_a)
    .bind(&message_a)
    .fetch_one(&db.pool)
    .await
    .expect("query account A tokens");
    let tokens_b: Option<i32> = sqlx::query_scalar(
        "SELECT message_tokens FROM emails WHERE account_id = $1 AND message_id = $2",
    )
    .bind(account_b)
    .bind(&message_b)
    .fetch_one(&db.pool)
    .await
    .expect("query account B tokens");

    assert!(
        tokens_a.unwrap_or_default() > 0,
        "account A row should be backfilled"
    );
    assert_eq!(tokens_b, None, "account B row should remain untouched");

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_batch_lifecycle_assign_and_query_roundtrip() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping batch lifecycle test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("batch-lifecycle-{}-", Uuid::new_v4());
    let batch_id = format!("{}batch", prefix);
    cleanup_test_batches(&db, &prefix).await;
    cleanup_test_emails(&db, &prefix).await;

    let message_a = format!("{}a", prefix);
    let message_b = format!("{}b", prefix);
    sqlx::query(
            r#"
            INSERT INTO emails (
                account_id,
                message_id,
                subject,
                sender,
                received_date,
                body_text,
                deleted_from_server_at
            )
            VALUES
                ($1, $2, 'batch A', 'sender-a@example.com', NOW() - INTERVAL '1 minute', 'body', NULL),
                ($1, $3, 'batch B', 'sender-b@example.com', NOW(), 'body', NULL)
            "#,
        )
        .bind(DEFAULT_ACCOUNT_ID)
        .bind(&message_a)
        .bind(&message_b)
        .execute(&db.pool)
        .await
        .expect("insert batch lifecycle emails");

    db.create_batch_for_account(DEFAULT_ACCOUNT_ID, &batch_id, 2)
        .await
        .expect("create batch");
    let assigned = db
        .assign_emails_to_batch_for_account(
            DEFAULT_ACCOUNT_ID,
            &batch_id,
            &[message_a.clone(), message_b.clone()],
        )
        .await
        .expect("assign emails to batch");
    assert_eq!(assigned, 2);

    let batch_emails = db
        .get_batch_emails_for_account(DEFAULT_ACCOUNT_ID, &batch_id)
        .await
        .expect("get batch emails");
    let ids: HashSet<String> = batch_emails
        .into_iter()
        .map(|email| email.message_id)
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&message_a));
    assert!(ids.contains(&message_b));

    let batch_results = db
        .get_batch_results_for_account(DEFAULT_ACCOUNT_ID, &batch_id)
        .await
        .expect("get batch results");
    let result_ids: HashSet<String> = batch_results.into_iter().map(|(id, _)| id).collect();
    assert_eq!(result_ids.len(), 2);
    assert!(result_ids.contains(&message_a));
    assert!(result_ids.contains(&message_b));

    db.update_batch_status_for_account(DEFAULT_ACCOUNT_ID, &batch_id, "processing")
        .await
        .expect("update batch status");
    db.complete_batch_for_account(DEFAULT_ACCOUNT_ID, &batch_id)
        .await
        .expect("complete batch");

    let recent = db
        .get_recent_batches_for_account(DEFAULT_ACCOUNT_ID, 20)
        .await
        .expect("get recent batches");
    assert!(recent.iter().any(|batch| batch.batch_id == batch_id));

    cleanup_test_batches(&db, &prefix).await;
    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_folder_email_samples_returns_correct_folder() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping folder email sample test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("folder-sample-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let folder_a = "Consolidator/TestA";
    let folder_b = "Consolidator/TestB";
    let message_a1 = format!("{}a1", prefix);
    let message_a2 = format!("{}a2", prefix);
    let message_b1 = format!("{}b1", prefix);

    sqlx::query(
            r#"
            INSERT INTO emails (
                message_id,
                location,
                subject,
                sender,
                category,
                email_type,
                organization,
                human_summary,
                received_date,
                body_text,
                deleted_from_server_at
            )
            VALUES
                ($1, $4, 'A1', 'alpha@example.com', 'shopping', 'receipt', 'Store A', 'summary-a1', NOW() - INTERVAL '2 minutes', 'body', NULL),
                ($2, $4, 'A2', 'alpha@example.com', 'shopping', 'receipt', 'Store A', 'summary-a2', NOW() - INTERVAL '1 minutes', 'body', NULL),
                ($3, $5, 'B1', 'beta@example.com', 'work', 'notification', 'Org B', 'summary-b1', NOW(), 'body', NULL)
            "#,
        )
        .bind(&message_a1)
        .bind(&message_a2)
        .bind(&message_b1)
        .bind(folder_a)
        .bind(folder_b)
        .execute(&db.pool)
        .await
        .expect("insert folder sample rows");

    let samples_a = db
        .get_folder_email_samples_for_account(DEFAULT_ACCOUNT_ID, folder_a, 10)
        .await
        .expect("query folder A samples");
    let samples_b = db
        .get_folder_email_samples_for_account(DEFAULT_ACCOUNT_ID, folder_b, 10)
        .await
        .expect("query folder B samples");

    let ids_a: HashSet<String> = samples_a
        .into_iter()
        .map(|sample| sample.message_id)
        .collect();
    let ids_b: HashSet<String> = samples_b
        .into_iter()
        .map(|sample| sample.message_id)
        .collect();

    assert_eq!(ids_a.len(), 2);
    assert!(ids_a.contains(&message_a1));
    assert!(ids_a.contains(&message_a2));
    assert_eq!(ids_b.len(), 1);
    assert!(ids_b.contains(&message_b1));

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_folder_email_samples_account_isolated() {
    let Some(db) = load_test_database().await else {
        eprintln!(
                "Skipping folder email sample account isolation test (no TEST_DATABASE_URL or DATABASE_URL)"
            );
        return;
    };

    let prefix = format!("folder-sample-iso-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let account_a = "folder-sample-account-a";
    let account_b = "folder-sample-account-b";
    let folder = "Receipts";
    let message_a = format!("{}a", prefix);
    let message_b = format!("{}b", prefix);

    sqlx::query(
        r#"
            INSERT INTO emails (
                account_id,
                message_id,
                location,
                sender,
                received_date,
                body_text,
                deleted_from_server_at
            )
            VALUES
                ($1, $2, $3, 'alpha@example.com', NOW() - INTERVAL '1 minutes', 'body', NULL),
                ($4, $5, $3, 'beta@example.com', NOW(), 'body', NULL)
            "#,
    )
    .bind(account_a)
    .bind(&message_a)
    .bind(folder)
    .bind(account_b)
    .bind(&message_b)
    .execute(&db.pool)
    .await
    .expect("insert account isolation sample rows");

    let samples_a = db
        .get_folder_email_samples_for_account(account_a, folder, 10)
        .await
        .expect("query account A samples");
    let samples_b = db
        .get_folder_email_samples_for_account(account_b, folder, 10)
        .await
        .expect("query account B samples");

    assert_eq!(samples_a.len(), 1);
    assert_eq!(samples_b.len(), 1);
    assert_eq!(samples_a[0].message_id, message_a);
    assert_eq!(samples_b[0].message_id, message_b);

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_email_stats_for_window_counts_correctly() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping digest stats test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("digest-stats-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let rows = [
        (
            format!("{}1", prefix),
            "work",
            "notification",
            Some("spam"),
            None,
            None,
            None,
            None,
            Some("filed"),
            "sender-a@example.com",
        ),
        (
            format!("{}2", prefix),
            "work",
            "notification",
            None,
            Some("phishing"),
            None,
            None,
            Some("high"),
            Some("trashed"),
            "sender-a@example.com",
        ),
        (
            format!("{}3", prefix),
            "financial",
            "receipt",
            None,
            None,
            Some("marketing"),
            Some("otp"),
            None,
            Some("junked"),
            "sender-b@example.com",
        ),
        (
            format!("{}4", prefix),
            "shopping",
            "receipt",
            None,
            None,
            None,
            None,
            None,
            Some("filed"),
            "sender-c@example.com",
        ),
        (
            format!("{}5", prefix),
            "personal",
            "conversation",
            None,
            None,
            None,
            None,
            None,
            None,
            "sender-d@example.com",
        ),
    ];

    for (
        message_id,
        category,
        email_type,
        spam_status,
        phishing_status,
        marketing_status,
        otp_status,
        threat_level,
        action_status,
        sender,
    ) in rows
    {
        sqlx::query(
            r#"
                INSERT INTO emails (
                    account_id,
                    message_id,
                    category,
                    email_type,
                    spam_status,
                    phishing_status,
                    marketing_status,
                    otp_status,
                    threat_level,
                    action_status,
                    sender,
                    received_date,
                    body_text,
                    deleted_from_server_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW(), 'body', NULL)
                "#,
        )
        .bind(DEFAULT_ACCOUNT_ID)
        .bind(message_id)
        .bind(category)
        .bind(email_type)
        .bind(spam_status)
        .bind(phishing_status)
        .bind(marketing_status)
        .bind(otp_status)
        .bind(threat_level)
        .bind(action_status)
        .bind(sender)
        .execute(&db.pool)
        .await
        .expect("insert digest stats row");
    }

    let since = Utc::now() - chrono::Duration::hours(1);
    let stats = db
        .get_email_stats_for_window_for_account(DEFAULT_ACCOUNT_ID, since)
        .await
        .expect("query digest window stats");

    assert_eq!(stats.total_received, 5);
    assert_eq!(stats.by_category.get("work"), Some(&2));
    assert_eq!(stats.by_category.get("financial"), Some(&1));
    assert_eq!(stats.spam_count, 1);
    assert_eq!(stats.phishing_count, 1);
    assert_eq!(stats.marketing_count, 1);
    assert_eq!(stats.otp_count, 1);
    assert_eq!(stats.threats_detected, 1);
    assert_eq!(stats.filed_count, 2);
    assert_eq!(stats.trashed_count, 1);
    assert_eq!(stats.junked_count, 1);

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_get_top_senders_for_window_returns_ordered() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping digest top senders test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("digest-senders-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    let senders = [
        "sender-a@example.com",
        "sender-a@example.com",
        "sender-a@example.com",
        "sender-b@example.com",
        "sender-c@example.com",
        "sender-c@example.com",
    ];
    for (idx, sender) in senders.iter().enumerate() {
        sqlx::query(
            r#"
                INSERT INTO emails (
                    account_id,
                    message_id,
                    sender,
                    received_date,
                    body_text,
                    deleted_from_server_at
                )
                VALUES ($1, $2, $3, NOW(), 'body', NULL)
                "#,
        )
        .bind(DEFAULT_ACCOUNT_ID)
        .bind(format!("{}{}", prefix, idx))
        .bind(sender)
        .execute(&db.pool)
        .await
        .expect("insert top sender row");
    }

    let since = Utc::now() - chrono::Duration::hours(1);
    let top = db
        .get_top_senders_for_window_for_account(DEFAULT_ACCOUNT_ID, since, 10)
        .await
        .expect("query top senders");

    assert!(top.len() >= 3, "expected at least 3 senders");
    assert_eq!(top[0], ("sender-a@example.com".to_string(), 3));
    assert_eq!(top[1], ("sender-c@example.com".to_string(), 2));
    assert_eq!(top[2], ("sender-b@example.com".to_string(), 1));

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_window_boundary_excludes_old_emails() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping digest boundary test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("digest-boundary-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;

    sqlx::query(
        r#"
            INSERT INTO emails (
                account_id,
                message_id,
                category,
                received_date,
                body_text,
                deleted_from_server_at
            )
            VALUES
                ($1, $2, 'work', NOW(), 'body', NULL),
                ($1, $3, 'financial', NOW(), 'body', NULL),
                ($1, $4, 'work', NOW() - INTERVAL '3 days', 'body', NULL)
            "#,
    )
    .bind(DEFAULT_ACCOUNT_ID)
    .bind(format!("{}in-1", prefix))
    .bind(format!("{}in-2", prefix))
    .bind(format!("{}old", prefix))
    .execute(&db.pool)
    .await
    .expect("insert digest boundary rows");

    let since = Utc::now() - chrono::Duration::days(1);
    let stats = db
        .get_email_stats_for_window_for_account(DEFAULT_ACCOUNT_ID, since)
        .await
        .expect("query boundary stats");

    assert_eq!(stats.total_received, 2);
    assert_eq!(stats.by_category.get("work"), Some(&1));
    assert_eq!(stats.by_category.get("financial"), Some(&1));

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_frontier_claim_prevents_duplicate_processing() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping frontier claim test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("frontier-claim-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;
    let message_id = format!("{}msg", prefix);

    sqlx::query(
            "INSERT INTO emails (message_id, received_date, body_text, deleted_from_server_at) VALUES ($1, NOW(), 'body', NULL)",
        )
        .bind(&message_id)
        .execute(&db.pool)
        .await
        .expect("insert frontier test email");

    db.enqueue_frontier_analysis(&message_id)
        .await
        .expect("enqueue frontier");

    let first_claim = db
        .claim_frontier_queue_batch("worker-a", 1)
        .await
        .expect("first claim");
    let second_claim = db
        .claim_frontier_queue_batch("worker-b", 1)
        .await
        .expect("second claim");

    assert_eq!(first_claim.len(), 1);
    assert_eq!(second_claim.len(), 0);
    assert_eq!(first_claim[0].message_id, message_id);
    assert_eq!(first_claim[0].attempt_count, 1);

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_release_core_work_for_worker_only_releases_owned_processing_rows() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping core work release test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let account_id = format!("core-release-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;

    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::SubagentTask,
        "release-owned",
        serde_json::json!({
            "task_id": format!("{}-task", account_id),
            "task_kind": "email_classification",
            "skill_bundle": "email_classification",
            "created_by": "mail-assistant",
            "reason": "release_test"
        }),
    )
    .await
    .expect("enqueue owned work");
    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::SyncIncremental,
        "release-other",
        serde_json::json!({"reason": "release_test"}),
    )
    .await
    .expect("enqueue other work");

    let owned = db
        .claim_core_work_for_account(&account_id, "worker-owned")
        .await
        .expect("claim owned work")
        .expect("owned work row");
    let other = db
        .claim_core_work_for_account(&account_id, "worker-other")
        .await
        .expect("claim other work")
        .expect("other work row");

    let task = SubagentTaskRecord {
        task_id: format!("{}-task", account_id),
        task_kind: "email_classification".to_string(),
        worker_name: "classification-worker".to_string(),
        skill_bundle: "email_classification".to_string(),
        message_ids: Vec::new(),
        input_context: Value::Null,
        priority: 0,
        correlation_id: account_id.clone(),
        created_by: "mail-assistant".to_string(),
    };
    db.upsert_subagent_task_for_account(&account_id, &task, Some(owned.id), "running")
        .await
        .expect("mark subagent running");

    let released = db
        .release_core_work_for_worker_for_account(
            &account_id,
            "worker-owned",
            "test shutdown cleanup",
        )
        .await
        .expect("release owned work");
    assert_eq!(released, 1);

    let owned_row = sqlx::query(
            "SELECT status, worker_id, locked_at, last_error FROM core_work_queue WHERE account_id = $1 AND id = $2",
        )
        .bind(&account_id)
        .bind(owned.id)
        .fetch_one(&db.pool)
        .await
        .expect("fetch owned row");
    assert_eq!(owned_row.get::<String, _>("status"), "failed");
    assert_eq!(owned_row.get::<Option<String>, _>("worker_id"), None);
    assert_eq!(owned_row.get::<Option<DateTime<Utc>>, _>("locked_at"), None);
    assert_eq!(
        owned_row.get::<Option<String>, _>("last_error").as_deref(),
        Some("test shutdown cleanup")
    );

    let other_row = sqlx::query(
            "SELECT status, worker_id, locked_at FROM core_work_queue WHERE account_id = $1 AND id = $2",
        )
        .bind(&account_id)
        .bind(other.id)
        .fetch_one(&db.pool)
        .await
        .expect("fetch other row");
    assert_eq!(other_row.get::<String, _>("status"), "processing");
    assert_eq!(
        other_row.get::<Option<String>, _>("worker_id").as_deref(),
        Some("worker-other")
    );
    assert!(other_row
        .get::<Option<DateTime<Utc>>, _>("locked_at")
        .is_some());

    let task_row = sqlx::query(
            "SELECT status, started_at, finished_at, error FROM subagent_tasks WHERE account_id = $1 AND task_id = $2",
        )
        .bind(&account_id)
        .bind(&task.task_id)
        .fetch_one(&db.pool)
        .await
        .expect("fetch subagent task");
    assert_eq!(task_row.get::<String, _>("status"), "pending");
    assert_eq!(task_row.get::<Option<DateTime<Utc>>, _>("started_at"), None);
    assert_eq!(
        task_row.get::<Option<DateTime<Utc>>, _>("finished_at"),
        None
    );
    assert_eq!(
        task_row.get::<Option<String>, _>("error").as_deref(),
        Some("test shutdown cleanup")
    );

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_pending_subagent_tasks_without_active_core_work_are_listed_for_reschedule() {
    let Some(db) = load_test_database().await else {
        eprintln!(
            "Skipping pending subagent reschedule test (no TEST_DATABASE_URL or DATABASE_URL)"
        );
        return;
    };

    let account_id = format!("subagent-reschedule-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;

    let stranded = SubagentTaskRecord {
        task_id: "stranded-review".to_string(),
        task_kind: "conflict_review".to_string(),
        worker_name: "conflict-review-worker".to_string(),
        skill_bundle: "conflict_review".to_string(),
        message_ids: Vec::new(),
        input_context: Value::Null,
        priority: 10,
        correlation_id: account_id.clone(),
        created_by: "mail-assistant".to_string(),
    };
    let active = SubagentTaskRecord {
        task_id: "active-review".to_string(),
        priority: 5,
        ..stranded.clone()
    };
    let completed = SubagentTaskRecord {
        task_id: "completed-review".to_string(),
        priority: 20,
        ..stranded.clone()
    };

    db.upsert_subagent_task_for_account(&account_id, &stranded, None, "pending")
        .await
        .expect("insert stranded task");
    db.upsert_subagent_task_for_account(&account_id, &active, None, "pending")
        .await
        .expect("insert active task");
    db.upsert_subagent_task_for_account(&account_id, &completed, None, "completed")
        .await
        .expect("insert completed task");
    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::SubagentTask,
        &active.task_id,
        serde_json::json!({
            "task_id": active.task_id,
            "task_kind": active.task_kind,
            "skill_bundle": active.skill_bundle,
            "created_by": active.created_by,
            "reason": "already_enqueued"
        }),
    )
    .await
    .expect("enqueue active task work");

    let pending = db
        .list_pending_subagent_tasks_without_active_core_work_for_account(&account_id, 10)
        .await
        .expect("list pending subagent tasks without active work");

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_id, stranded.task_id);

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_core_work_claim_prioritizes_pipeline_over_subagent_support() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping core work priority test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let account_id = format!("core-priority-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;

    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::SubagentTask,
        "support-subagent",
        serde_json::json!({
            "task_id": format!("{}-task", account_id),
            "task_kind": "email_classification",
            "skill_bundle": "email_classification",
            "created_by": "mail-assistant",
            "reason": "priority_test"
        }),
    )
    .await
    .expect("enqueue support subagent work");
    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::Analyze,
        "pipeline-analyze",
        serde_json::json!({"reason": "priority_test"}),
    )
    .await
    .expect("enqueue pipeline analyze work");

    let claimed = db
        .claim_core_work_for_account(&account_id, "priority-worker")
        .await
        .expect("claim work")
        .expect("work row");

    assert_eq!(claimed.work_type, CoreWorkType::Analyze);

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_core_work_claim_prioritizes_analysis_over_older_side_work() {
    let Some(db) = load_test_database().await else {
        eprintln!(
            "Skipping core work analysis priority test (no TEST_DATABASE_URL or DATABASE_URL)"
        );
        return;
    };

    let account_id = format!("core-analysis-priority-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;

    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::Locate,
        "older-locate",
        serde_json::json!({"reason": "priority_test"}),
    )
    .await
    .expect("enqueue older locate work");
    sqlx::query(
        "UPDATE core_work_queue SET available_at = NOW() - INTERVAL '10 minutes' WHERE account_id = $1 AND idempotency_key = 'older-locate'",
    )
    .bind(&account_id)
    .execute(&db.pool)
    .await
    .expect("age locate work");

    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::Analyze,
        "newer-analyze",
        serde_json::json!({"reason": "priority_test"}),
    )
    .await
    .expect("enqueue newer analyze work");

    let claimed = db
        .claim_core_work_for_account(&account_id, "analysis-priority-worker")
        .await
        .expect("claim work")
        .expect("work row");

    assert_eq!(claimed.work_type, CoreWorkType::Analyze);

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_core_work_completion_requires_current_claim_lease() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping core work claim lease test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let account_id = format!("core-lease-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;

    db.enqueue_core_work_for_account(
        &account_id,
        CoreWorkType::Analyze,
        "lease-fenced",
        serde_json::json!({"reason": "lease_fencing_test"}),
    )
    .await
    .expect("enqueue lease test work");

    let stale_claim = db
        .claim_core_work_with_lease_for_account(&account_id, "worker-stale", 60)
        .await
        .expect("claim stale work")
        .expect("stale claim");

    sqlx::query(
        "UPDATE core_work_queue SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE account_id = $1 AND id = $2",
    )
    .bind(&account_id)
    .bind(stale_claim.id)
    .execute(&db.pool)
    .await
    .expect("expire stale claim");

    let reset = db
        .reset_stale_core_work_for_account(&account_id, 60)
        .await
        .expect("reset expired lease");
    assert_eq!(reset, 1);

    let current_claim = db
        .claim_core_work_with_lease_for_account(&account_id, "worker-current", 60)
        .await
        .expect("claim current work")
        .expect("current claim");
    assert_eq!(current_claim.id, stale_claim.id);

    let stale_completed = db
        .mark_claimed_core_work_done_for_account(&account_id, &stale_claim)
        .await
        .expect("stale completion should be handled");
    assert!(!stale_completed);

    let current_completed = db
        .mark_claimed_core_work_done_for_account(&account_id, &current_claim)
        .await
        .expect("current completion should win");
    assert!(current_completed);

    let row = sqlx::query(
        "SELECT status, worker_id, locked_at, lease_expires_at FROM core_work_queue WHERE account_id = $1 AND id = $2",
    )
    .bind(&account_id)
    .bind(current_claim.id)
    .fetch_one(&db.pool)
    .await
    .expect("fetch completed row");
    assert_eq!(row.get::<String, _>("status"), "done");
    assert_eq!(row.get::<Option<String>, _>("worker_id"), None);
    assert_eq!(row.get::<Option<DateTime<Utc>>, _>("locked_at"), None);
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("lease_expires_at"),
        None
    );

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_core_work_enqueue_backpressure_rejects_new_active_rows() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping core work backpressure test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let account_id = format!("core-backpressure-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;
    let backpressure = CoreWorkBackpressureConfig { max_active: 1 };

    let first = db
        .try_enqueue_core_work_for_account(
            &account_id,
            CoreWorkType::Analyze,
            "first",
            serde_json::json!({"reason": "backpressure_test"}),
            backpressure,
        )
        .await
        .expect("enqueue first work");
    assert!(matches!(first, CoreWorkEnqueueOutcome::Enqueued(_)));

    let second = db
        .try_enqueue_core_work_for_account(
            &account_id,
            CoreWorkType::Embed,
            "second",
            serde_json::json!({"reason": "backpressure_test"}),
            backpressure,
        )
        .await
        .expect("backpressure second work");
    assert!(matches!(
        second,
        CoreWorkEnqueueOutcome::Backpressured(CoreWorkQueuePressure {
            active: 1,
            max_active: 1,
            backpressured: true
        })
    ));

    let existing = db
        .try_enqueue_core_work_for_account(
            &account_id,
            CoreWorkType::Analyze,
            "first",
            serde_json::json!({"reason": "backpressure_existing_update"}),
            backpressure,
        )
        .await
        .expect("existing active work can be refreshed");
    assert!(matches!(existing, CoreWorkEnqueueOutcome::Enqueued(_)));

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_subagent_skill_lessons_reinforce_existing_memory() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping skill lesson test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let account_id = format!("skill-lessons-{}", Uuid::new_v4());
    cleanup_test_core_work_account(&db, &account_id).await;
    let lesson = SubagentSkillLessonRecord {
        skill_bundle: "email_classification".to_string(),
        lesson_key: "strategy-test".to_string(),
        lesson_type: "strategy".to_string(),
        status: "candidate".to_string(),
        summary: "Prefer sender history before guessing category.".to_string(),
        evidence: serde_json::json!([]),
        score: Some(0.9),
        support_count: 1,
        negative_count: 0,
        source_task_id: Some("test-task".to_string()),
        source_result_id: None,
        source_run_id: Some("test-run".to_string()),
        worker_name: Some("classification-worker".to_string()),
        agent_spec_version: Some("test".to_string()),
    };

    db.upsert_subagent_skill_lesson_for_account(&account_id, &lesson)
        .await
        .expect("insert skill lesson");
    db.upsert_subagent_skill_lesson_for_account(&account_id, &lesson)
        .await
        .expect("reinforce skill lesson");

    let lessons = db
        .list_active_subagent_skill_lessons_for_account(&account_id, "email_classification", 10)
        .await
        .expect("list skill lessons");

    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].support_count, 2);

    cleanup_test_core_work_account(&db, &account_id).await;
}

#[tokio::test]
#[ignore]
async fn test_frontier_retry_backoff_persists_failed_state() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping frontier retry test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("frontier-retry-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;
    let message_id = format!("{}msg", prefix);

    sqlx::query(
            "INSERT INTO emails (message_id, received_date, body_text, deleted_from_server_at) VALUES ($1, NOW(), 'body', NULL)",
        )
        .bind(&message_id)
        .execute(&db.pool)
        .await
        .expect("insert frontier test email");

    db.enqueue_frontier_analysis(&message_id)
        .await
        .expect("enqueue frontier");
    let claim = db
        .claim_frontier_queue_batch("worker-retry", 1)
        .await
        .expect("claim frontier job");
    let entry = claim.into_iter().next().expect("claimed row");

    let status = db
        .mark_frontier_retry_or_dead(
            &entry.message_id,
            entry.attempt_count,
            5,
            45,
            "429 rate limit",
        )
        .await
        .expect("mark retry/dead");
    assert_eq!(status, "failed");

    let row = sqlx::query(
            r#"
            SELECT status, attempt_count, available_at > NOW() AS delayed, locked_at, worker_id, last_error
            FROM frontier_analysis_queue
            WHERE message_id = $1
            "#,
        )
        .bind(&message_id)
        .fetch_one(&db.pool)
        .await
        .expect("fetch frontier queue row");

    assert_eq!(row.get::<String, _>("status"), "failed");
    assert_eq!(row.get::<i32, _>("attempt_count"), 1);
    assert!(row.get::<bool, _>("delayed"));
    assert_eq!(row.get::<Option<String>, _>("worker_id"), None);
    assert!(row.get::<Option<String>, _>("last_error").is_some());
    assert_eq!(
        row.get::<Option<chrono::DateTime<Utc>>, _>("locked_at"),
        None
    );

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_frontier_dead_letters_after_max_attempts() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping frontier dead-letter test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("frontier-dead-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;
    let message_id = format!("{}msg", prefix);

    sqlx::query(
            "INSERT INTO emails (message_id, received_date, body_text, deleted_from_server_at) VALUES ($1, NOW(), 'body', NULL)",
        )
        .bind(&message_id)
        .execute(&db.pool)
        .await
        .expect("insert frontier test email");

    db.enqueue_frontier_analysis(&message_id)
        .await
        .expect("enqueue frontier");
    let claim = db
        .claim_frontier_queue_batch("worker-dead", 1)
        .await
        .expect("claim frontier job");
    let entry = claim.into_iter().next().expect("claimed row");

    let status = db
        .mark_frontier_retry_or_dead(
            &entry.message_id,
            entry.attempt_count,
            1,
            30,
            "permanent processing error",
        )
        .await
        .expect("mark dead");
    assert_eq!(status, "dead");

    let row = sqlx::query(
        r#"
            SELECT status, attempt_count, locked_at, worker_id
            FROM frontier_analysis_queue
            WHERE message_id = $1
            "#,
    )
    .bind(&message_id)
    .fetch_one(&db.pool)
    .await
    .expect("fetch frontier queue row");

    assert_eq!(row.get::<String, _>("status"), "dead");
    assert_eq!(row.get::<i32, _>("attempt_count"), 1);
    assert_eq!(
        row.get::<Option<chrono::DateTime<Utc>>, _>("locked_at"),
        None
    );
    assert_eq!(row.get::<Option<String>, _>("worker_id"), None);

    cleanup_test_emails(&db, &prefix).await;
}

#[tokio::test]
#[ignore]
async fn test_frontier_mark_done_removes_row() {
    let Some(db) = load_test_database().await else {
        eprintln!("Skipping frontier done test (no TEST_DATABASE_URL or DATABASE_URL)");
        return;
    };

    let prefix = format!("frontier-done-{}-", Uuid::new_v4());
    cleanup_test_emails(&db, &prefix).await;
    let message_id = format!("{}msg", prefix);

    sqlx::query(
            "INSERT INTO emails (message_id, received_date, body_text, deleted_from_server_at) VALUES ($1, NOW(), 'body', NULL)",
        )
        .bind(&message_id)
        .execute(&db.pool)
        .await
        .expect("insert frontier test email");

    db.enqueue_frontier_analysis(&message_id)
        .await
        .expect("enqueue frontier");
    let removed = db
        .mark_frontier_done(&message_id)
        .await
        .expect("mark frontier done");
    assert_eq!(removed, 1);

    let exists = sqlx::query("SELECT 1 FROM frontier_analysis_queue WHERE message_id = $1")
        .bind(&message_id)
        .fetch_optional(&db.pool)
        .await
        .expect("query frontier queue")
        .is_some();
    assert!(!exists);

    cleanup_test_emails(&db, &prefix).await;
}
