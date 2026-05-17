# MailSubsystem Database Guide

`schema.sql` is the single canonical schema definition for MailSubsystem.

Use this document for schema ownership, migration posture, and table semantics.
Use [`../schema.sql`](../schema.sql) for exact DDL: tables, columns,
constraints, indexes, functions, triggers, and extension setup.

## Canonical Source

- `schema.sql` is the executable source of truth.
- The Rust binary embeds `schema.sql` and applies it only through the configured
  schema migration mode.
- The schema fingerprint ignores full-line SQL comments and blank lines, so
  documentation-only comments in `schema.sql` do not force a migration.
- Documentation must not duplicate column-level DDL. When a column, constraint,
  index, trigger, or table changes, update `schema.sql` first.
- This guide may describe what tables mean, but it should not restate the full
  schema in prose.

## Schema Management

Startup uses a conservative release posture:

- `MAILSUBSYSTEM_SCHEMA_MODE=bootstrap` is the default. It initializes an empty
  database and records the embedded schema fingerprint in `system_metadata`.
- Existing databases that are missing tables, missing required columns, or have a
  stale/missing schema fingerprint are not migrated silently in the default mode.
- To apply schema changes intentionally, review `schema.sql` and run
  `mailsubsystem migrate-schema --apply` or `make migrate-schema`.
- `MAILSUBSYSTEM_SCHEMA_MODE=auto` restores startup-time schema application for
  development environments.
- `MAILSUBSYSTEM_SCHEMA_MODE=validate`, `manual`, or `off` never creates or
  migrates schema objects.

## Table Inventory

The canonical table definitions live in `schema.sql`. The current schema groups
tables by responsibility:

| Area | Tables |
|------|--------|
| Mail data | `emails`, `imap_folders`, `emails_missing_message_id` |
| Sync and runtime queues | `body_sync_queue`, `frontier_analysis_queue`, `core_work_queue`, `sync_window_runs` |
| Analysis batches | `analysis_batches` |
| Assistant and workers | `assistant_insights`, `subagent_tasks`, `subagent_results`, `subagent_skill_lessons` |
| Filing and learning | `email_location_events`, `folder_learning_rules` |
| Agent harness | `agent_runs`, `agent_state`, `agent_checkpoints`, `agent_tool_log` |
| Chat | `conversation_threads`, `conversation_messages` |
| Lifecycle helpers | `otp_codes` |
| System metadata | `system_metadata` |

The schema also installs the `vector` extension for embedding search.

## Key Concepts

### Account Scope

Most runtime data is scoped by `account_id`. The default account is `default`,
but table constraints and indexes are designed so multi-account support can
share one database without cross-account identity collisions.

### Email Identity

`emails` stores synced IMAP messages. The account-scoped identity is
`(account_id, message_id)`, where `message_id` comes from the RFC 5322
Message-ID header. IMAP UID and UIDVALIDITY fields track the message's current
folder position and are updated by sync.

### Body Sync

Envelope sync can create or update message rows before full bodies are fetched.
`body_sync_queue` tracks follow-up work for raw message and body text backfill.

### AI Analysis

Analysis results live on `emails` and in supporting queue/batch tables. The
schema keeps classification fields explicit enough for SQL filtering while
allowing richer model output to live in JSONB fields.

### Filing Safety

Filing policy data is split between current email state and event/learning
tables:

- `emails` tracks current location, recommendations, filing locks, pinned
  folders, and move counters.
- `email_location_events` records observed or core-applied move events.
- `folder_learning_rules` stores reusable filing guidance learned from observed
  behavior.

Core filing policy is implemented in Rust and should be treated as the safety
boundary for IMAP mutation.

### Mail Assistant Runtime

Mail Assistant and internal worker execution use:

- `core_work_queue` for durable runtime work.
- `subagent_tasks`, `subagent_results`, and `subagent_skill_lessons` for scoped
  worker execution and bounded skill memory.
- `assistant_insights` for mailbox/runtime observations surfaced back to the
  assistant layer.

### Agent Harness and Chat

The agent harness stores run state, checkpoints, tool logs, and scratchpad state
in the `agent_*` tables. User-facing chat state lives in
`conversation_threads` and `conversation_messages`.

## Index and Function Ownership

Indexes, triggers, helper functions, and generated search fields are owned by
`schema.sql`. Do not document or maintain a parallel index list here. Review
`schema.sql` directly when evaluating performance-critical query paths.

## Change Checklist

When changing the database schema:

1. Update `schema.sql`.
2. Make the Rust query/model changes that depend on it.
3. Update this guide only if table ownership or operational semantics changed.
4. Run `make migrate-schema` against a dev database.
5. Run `cargo test` and `cargo clippy --all-targets --all-features -- -D warnings`.

## Operational Notes

- Fresh local databases can bootstrap automatically in default mode.
- Existing databases require explicit migration when the embedded schema
  fingerprint is stale.
- `system_metadata` stores the embedded schema fingerprint under
  `schema.sql.md5`.
- Manual PostgreSQL setups must install pgvector support before applying
  `schema.sql`.
